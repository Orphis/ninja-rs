#!/usr/bin/env bash
# End-to-end test: builds a small C++ project through ninja-rs against a
# local NativeLink server (scheduler + local worker), using the hermetic
# clang from the llvm Bazel module for remote compiles. Linux only.
#
# Usage:
#   NATIVELINK=/path/to/nativelink tests/nativelink/run.sh
#
# Optional environment:
#   NINJA_RS      ninja-rs binary (default: bazel-linux-bin/crates/ninja-rs/ninja-rs)
#   WORK          scratch directory (default: /tmp/ninja-rs-nativelink-e2e)
#   PORT          NativeLink frontend port (default: 50981; worker API uses PORT+1)
#   OUTPUT_BASE   Bazel output base holding the llvm module repositories
#                 (default: `bazel info output_base`)
set -euo pipefail

here=$(cd "$(dirname "$0")/../.." && pwd)
: "${NATIVELINK:?set NATIVELINK to a nativelink binary}"
NINJA_RS=${NINJA_RS:-$here/bazel-linux-bin/crates/ninja-rs/ninja-rs}
WORK=${WORK:-/tmp/ninja-rs-nativelink-e2e}
PORT=${PORT:-50981}
WORKER_PORT=$((PORT + 1))
OUTPUT_BASE=${OUTPUT_BASE:-$(cd "$here" && bazelisk info --config=linux output_base 2>/dev/null)}
EXT=$OUTPUT_BASE/external
LLVM=$EXT/llvm++llvm_toolchain_minimal+llvm-toolchain-minimal-linux-amd64
GLIBC=$(ls -d "$EXT"/llvm++glibc+glibc_headers_x86_64-linux-gnu.* | head -1)
KERNEL=$(ls -d "$EXT"/llvm++kernel_headers+linux_kernel_headers_x86.* | head -1)
CLANG_VER=$(ls "$LLVM/lib/clang")
for p in "$NINJA_RS" "$LLVM/bin/clang++" "$GLIBC/include/stdio.h" "$KERNEL/include/linux"; do
  [[ -e $p ]] || { echo "missing $p (run: bazelisk build --config=linux //crates/ninja-rs)"; exit 1; }
done

rm -rf "$WORK"
mkdir -p "$WORK"/{nl,proj/src,proj/include/detail,proj/build/CMakeFiles}

# --- NativeLink: memory-backed CAS/AC, one local worker ----------------------
cat > "$WORK/nl/config.json5" <<EOF
{
  stores: [
    { name: "AC", filesystem: { content_path: "$WORK/nl/ac", temp_path: "$WORK/nl/ac-tmp", eviction_policy: { max_bytes: 2000000000 } } },
    { name: "CAS", fast_slow: { fast: { filesystem: { content_path: "$WORK/nl/cas", temp_path: "$WORK/nl/cas-tmp", eviction_policy: { max_bytes: 10000000000 } } }, slow: { noop: {} } } },
  ],
  schedulers: [{ name: "S", simple: { supported_platform_properties: { cpu_count: "minimum", OSFamily: "exact" } } }],
  workers: [{ local: {
    worker_api_endpoint: { uri: "grpc://127.0.0.1:$WORKER_PORT" },
    cas_fast_slow_store: "CAS",
    upload_action_result: { ac_store: "AC" },
    work_directory: "$WORK/nl/work",
    platform_properties: { cpu_count: { values: ["8"] }, OSFamily: { values: ["Linux"] } },
  } }],
  servers: [
    { listener: { http: { socket_address: "127.0.0.1:$PORT" } }, services: {
      cas: [{ instance_name: "main", cas_store: "CAS" }],
      ac: [{ instance_name: "main", ac_store: "AC" }],
      execution: [{ instance_name: "main", cas_store: "CAS", scheduler: "S" }],
      capabilities: [{ instance_name: "main", remote_execution: { scheduler: "S" } }],
      bytestream: [{ instance_name: "main", cas_store: "CAS" }],
    } },
    { listener: { http: { socket_address: "127.0.0.1:$WORKER_PORT" } }, services: { worker_api: { scheduler: "S" } } },
  ],
}
EOF
"$NATIVELINK" "$WORK/nl/config.json5" > "$WORK/nl/nativelink.log" 2>&1 &
NL_PID=$!
trap 'kill $NL_PID 2>/dev/null || true' EXIT
for _ in $(seq 100); do
  (exec 3<>/dev/tcp/127.0.0.1/$PORT) 2>/dev/null && break
  sleep 0.1
done

# --- The project ---------------------------------------------------------------
P=$WORK/proj
cat > "$P/include/detail/area.h" <<'EOF'
#pragma once
namespace shapes::detail {
template <typename T> constexpr T square(T v) { return v * v; }
}
EOF
cat > "$P/include/shapes.h" <<'EOF'
#pragma once
#include "detail/area.h"
namespace shapes {
class Shape {
 public:
  virtual ~Shape() = default;
  virtual double area() const = 0;
};
class Square : public Shape {
 public:
  explicit Square(double s) : s_(s) {}
  double area() const override { return detail::square(s_); }
 private:
  double s_;
};
double total_area(const Shape* const* shapes, int n);
}  // namespace shapes
EOF
cat > "$P/src/shapes.cpp" <<'EOF'
#include "shapes.h"
namespace shapes {
double total_area(const Shape* const* shapes, int n) {
  double t = 0;
  for (int i = 0; i < n; ++i) t += shapes[i]->area();
  return t;
}
}  // namespace shapes
EOF
cat > "$P/src/main.cpp" <<'EOF'
#include <stdio.h>
#include "shapes.h"
int main() {
  shapes::Square a(2), b(3);
  const shapes::Shape* all[] = {&a, &b};
  printf("total area %.1f\n", shapes::total_area(all, 2));
  return 0;
}
EOF
cat > "$P/build/CMakeFiles/rbe_toolchains.json" <<EOF
{
  "version": 1,
  "toolchains": [
    {
      "language": "CXX",
      "compiler": "$LLVM/bin/clang++",
      "inputs": ["$LLVM/bin", "$LLVM/lib/clang"],
      "platform": { "OSFamily": "Linux" }
    }
  ]
}
EOF
CXXFLAGS="--target=x86_64-linux-gnu -nostdinc -nostdinc++ -fno-exceptions -fno-rtti -isystem $LLVM/lib/clang/$CLANG_VER/include -isystem $GLIBC/include -isystem $KERNEL/include -I$P/include -O2"
cat > "$P/build/build.ninja" <<EOF
# Written by hand following docs/cmake-interface.md.
ninja_required_version = 1.5
cmake_rbe = 1
cmake_rbe_toolchains = CMakeFiles/rbe_toolchains.json

rule CXX_COMPILER__shapes_Release
  depfile = \$DEP_FILE
  deps = gcc
  command = $LLVM/bin/clang++ \$FLAGS -MD -MT \$out -MF \$DEP_FILE -o \$out -c \$in
  description = Building CXX object \$out

rule CXX_STATIC_LIBRARY_LINKER__shapes_Release
  command = : && /usr/bin/cmake -E rm -f \$TARGET_FILE && /usr/bin/ar qc \$TARGET_FILE \$in && /usr/bin/ranlib \$TARGET_FILE && :
  description = Linking CXX static library \$TARGET_FILE

rule CXX_EXECUTABLE_LINKER__app_Release
  command = : && /usr/bin/g++ \$in -o \$TARGET_FILE \$LINK_LIBRARIES && :
  description = Linking CXX executable \$TARGET_FILE

build CMakeFiles/shapes.dir/src/shapes.cpp.o: CXX_COMPILER__shapes_Release $P/src/shapes.cpp
  DEP_FILE = CMakeFiles/shapes.dir/src/shapes.cpp.o.d
  FLAGS = $CXXFLAGS
  cmake_target = shapes
  mnemonic = CXXCompile

build libshapes.a: CXX_STATIC_LIBRARY_LINKER__shapes_Release CMakeFiles/shapes.dir/src/shapes.cpp.o
  TARGET_FILE = libshapes.a
  cmake_target = shapes
  mnemonic = CXXArchive

build CMakeFiles/app.dir/src/main.cpp.o: CXX_COMPILER__shapes_Release $P/src/main.cpp
  DEP_FILE = CMakeFiles/app.dir/src/main.cpp.o.d
  FLAGS = $CXXFLAGS
  cmake_target = app
  mnemonic = CXXCompile

build app: CXX_EXECUTABLE_LINKER__app_Release CMakeFiles/app.dir/src/main.cpp.o | libshapes.a
  LINK_LIBRARIES = libshapes.a
  TARGET_FILE = app
  cmake_target = app
  mnemonic = CXXLink
  deliverable = 1

build all: phony app
default all
EOF

cd "$P/build"
REMOTE=(--remote_executor=grpc://127.0.0.1:$PORT --remote_instance_name=main --remote_header=x-ninja-rs-test=1 --remote_verbose)
echo "=== clean build"
"$NINJA_RS" "${REMOTE[@]}" | tee "$WORK/build1.log"
./app
grep -q "2 remote actions (0 remote cache hits)" "$WORK/build1.log"
echo "=== no-op build"
"$NINJA_RS" "${REMOTE[@]}" | tee "$WORK/build2.log"
grep -q "no work to do" "$WORK/build2.log"
echo "=== header change"
sed -i 's/v \* v/v * v * 1/' "$P/include/detail/area.h"
"$NINJA_RS" "${REMOTE[@]}" | tee "$WORK/build3.log"
grep -q "2 remote actions (0 remote cache hits)" "$WORK/build3.log"
echo "=== clean + rebuild: remote cache hits"
"$NINJA_RS" -t clean
"$NINJA_RS" "${REMOTE[@]}" | tee "$WORK/build4.log"
grep -q "2 remote actions (2 remote cache hits)" "$WORK/build4.log"
./app
echo "=== deps from the remote depfile"
"$NINJA_RS" -t deps CMakeFiles/app.dir/src/main.cpp.o | tee "$WORK/deps.log"
grep -q "$P/include/detail/area.h" "$WORK/deps.log"
grep -q "$GLIBC/include/stdio.h" "$WORK/deps.log"
echo "=== archive edges are local even if allowed by --remote_mnemonics (2-step chain)"
"$NINJA_RS" -t clean > /dev/null
"$NINJA_RS" "${REMOTE[@]}" --remote_mnemonics=Compile,Archive | tee "$WORK/build5.log"
grep -q "running locally (command runs 2 steps)" "$WORK/build5.log"
echo "PASS"
