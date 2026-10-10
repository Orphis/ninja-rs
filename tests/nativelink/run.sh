#!/usr/bin/env bash
# End-to-end test against a local NativeLink (scheduler + local worker),
# building hermetic-llvm-cmake's sample (tests/hello) with its toolchain set
# up for remote execution as its README describes: the toolchain cache
# inside each checkout, and CMake's Ninja generator in RBE mode
# (CMAKE_NINJA_RBE). Two checkouts at different paths share every action:
#
#   1. checkout 1 executes remotely;
#   2. checkout 2 builds with --remote_cache alone: every action is a hit;
#   3. a change in checkout 2 misses, runs locally, and its results are
#      stored;
#   4. the same change in checkout 1, executing remotely, is all hits.
#
# Each step's outputs (objects included) must be byte-identical between the
# checkouts. Linux only.
#
# Usage:
#   NATIVELINK=/path/to/nativelink HERMETIC=/path/to/hermetic-llvm-cmake \
#     CMAKE=/path/to/cmake tests/nativelink/hermetic.sh
#
# CMAKE is a CMake whose Ninja generators have the RBE mode.
#
# Optional environment:
#   HERMETIC_DOWNLOADS  a toolchain cache copied into each checkout as its
#                       .hermetic-cpp, to skip the downloads (default:
#                       ~/.cache/hermetic-cpp, when it exists). A copy, not
#                       a link: clang finds its linker next to its resolved
#                       path, which must stay inside the tree.
#   TARGET      HERMETIC_TARGET (default: linux-x86_64)
#   NINJA_RS    ninja-rs binary (default: bazel-linux-bin/crates/ninja-rs/ninja-rs)
#   WORK        scratch directory (default: /tmp/ninja-rs-nativelink-hermetic)
#   PORT        NativeLink frontend port (default: 50985; worker API uses PORT+1)
set -euo pipefail

here=$(cd "$(dirname "$0")/../.." && pwd)
: "${NATIVELINK:?set NATIVELINK to a nativelink binary}"
: "${HERMETIC:?set HERMETIC to a hermetic-llvm-cmake checkout}"
: "${CMAKE:?set CMAKE to a CMake with the Ninja RBE mode}"
HERMETIC_DOWNLOADS=${HERMETIC_DOWNLOADS:-$HOME/.cache/hermetic-cpp}
TARGET=${TARGET:-linux-x86_64}
NINJA_RS=${NINJA_RS:-$here/bazel-linux-bin/crates/ninja-rs/ninja-rs}
WORK=${WORK:-/tmp/ninja-rs-nativelink-hermetic}
PORT=${PORT:-50985}
WORKER_PORT=$((PORT + 1))
for p in "$NINJA_RS" "$CMAKE" "$HERMETIC/toolchain.cmake" "$HERMETIC/tests/hello/CMakeLists.txt"; do
  [[ -e $p ]] || { echo "missing $p"; exit 1; }
done
rm -rf "$WORK"
mkdir -p "$WORK/nl"

# --- NativeLink: filesystem CAS/AC, one local worker -------------------------
cat > "$WORK/nl/config.json5" <<EOF
{
  stores: [
    { name: "AC", filesystem: { content_path: "$WORK/nl/ac", temp_path: "$WORK/nl/ac-tmp", eviction_policy: { max_bytes: 2000000000 } } },
    { name: "CAS", fast_slow: { fast: { filesystem: { content_path: "$WORK/nl/cas", temp_path: "$WORK/nl/cas-tmp", eviction_policy: { max_bytes: 10000000000 } } }, slow: { noop: {} } } },
  ],
  schedulers: [{ name: "S", simple: { supported_platform_properties: {} } }],
  workers: [{ local: {
    worker_api_endpoint: { uri: "grpc://127.0.0.1:$WORKER_PORT" },
    cas_fast_slow_store: "CAS",
    upload_action_result: { ac_store: "AC" },
    work_directory: "$WORK/nl/work",
    platform_properties: {},
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

# --- Two checkouts of the sample, configured as the README documents ---------
checkout() {  # $1 = directory
  mkdir -p "$1"
  find "$HERMETIC/tests/hello" -maxdepth 1 -type f -exec cp {} "$1"/ \;
  if [[ -d $HERMETIC_DOWNLOADS ]]; then
    cp -a "$HERMETIC_DOWNLOADS" "$1/.hermetic-cpp"
  fi
  "$CMAKE" -S "$1" -B "$1/build" -G Ninja -DCMAKE_MAKE_PROGRAM="$NINJA_RS" \
    -DCMAKE_TOOLCHAIN_FILE="$HERMETIC/toolchain.cmake" -DHERMETIC_TARGET="$TARGET" \
    -DHERMETIC_CACHE_DIR="$1/.hermetic-cpp" -DCMAKE_NINJA_RBE=ON -DCMAKE_BUILD_TYPE=RelWithDebInfo \
    > "$1/configure.log" 2>&1 || { tail -30 "$1/configure.log"; exit 1; }
}
A=$WORK/co1
B=$WORK/deeper/path/co2
checkout "$A"
checkout "$B"

COMMON=(--remote_instance_name=main --remote_verbose --remote_mnemonics=Compile,Archive,Link)
EXEC=(--remote_executor=grpc://127.0.0.1:$PORT "${COMMON[@]}")
CACHE=(--remote_cache=grpc://127.0.0.1:$PORT "${COMMON[@]}")

build() {  # $1 = checkout, $2 = log name, rest = flags
  local dir=$1 log=$WORK/$2.log
  shift 2
  "$NINJA_RS" -C "$dir/build" "$@" > "$log" 2>&1 || { tail -40 "$log"; exit 1; }
  echo "  executed remotely: $(grep -c 'ninja-rs: executed remotely' "$log" || true)," \
    "remote cache hits: $(grep -c 'ninja-rs: remote cache hit' "$log" || true)"
}
count() { grep -c "$1" "$WORK/$2.log" || true; }

# The outputs of compiles and links (objects, libraries, executables) must
# be identical; build.ninja and the like name each checkout's paths.
same_outputs() {
  local n=0
  while read -r out; do
    [[ -f $A/build/$out ]] || continue
    cmp -s "$A/build/$out" "$B/build/$out" || { echo "  $out differs"; exit 1; }
    n=$((n + 1))
  done < <("$NINJA_RS" -C "$A/build" -t targets all | grep -E ': [A-Z]+_(COMPILER|STATIC_LIBRARY_LINKER|SHARED_LIBRARY_LINKER|EXECUTABLE_LINKER)__' | cut -d: -f1)
  [[ $n -gt 0 ]]
  echo "  $n outputs identical"
}

echo "=== 1. checkout 1 executes remotely"
build "$A" exec1 "${EXEC[@]}"
executed=$(count 'ninja-rs: executed remotely' exec1)
[[ $executed -gt 0 ]]
"$A/build/hello_c" > /dev/null
"$A/build/hello_cxx" > /dev/null

echo "=== 2. checkout 2, remote cache only: every action is a hit"
build "$B" cache1 "${CACHE[@]}"
[[ $(count 'ninja-rs: remote cache hit' cache1) -eq $executed ]]
"$B/build/hello_c" > /dev/null
same_outputs

echo "=== 3. a change in checkout 2 misses, runs locally, and is stored"
echo 'const char* ninja_rs_rbe_change = "changed";' >> "$B/hello.c"
build "$B" cache2 "${CACHE[@]}"
[[ $(count 'ninja-rs: remote cache hit' cache2) -eq 0 ]]
"$B/build/hello_c" > /dev/null

echo "=== 4. the same change in checkout 1, executing remotely: all hits"
cp "$B/hello.c" "$A/hello.c"
build "$A" exec2 "${EXEC[@]}"
[[ $(count 'ninja-rs: executed remotely' exec2) -eq 0 ]]
[[ $(count 'ninja-rs: remote cache hit' exec2) -gt 0 ]]
same_outputs
echo "PASS"
