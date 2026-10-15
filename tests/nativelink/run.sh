#!/usr/bin/env bash
# End-to-end test against a local NativeLink (scheduler + local worker),
# building hermetic-llvm-cmake's sample (tests/hello) with its toolchain set
# up for remote execution as its README describes: the toolchain cache
# inside each checkout, and CMake's Ninja generator in RBE mode
# (CMAKE_NINJA_RBE). Two checkouts at different paths share every action:
#
#   1. checkout 1 executes remotely, strictly (no action runs locally);
#   2. checkout 2 builds with --remote_cache alone: every action is a hit;
#   3. a change in checkout 2 misses, runs locally, and its results are
#      stored;
#   4. the same change in checkout 1, executing remotely, is all hits;
#   5. checkout 1 again, racing local runs that start at once: whichever
#      wins, the outputs stay identical;
#   6. a change raced in checkout 1, executed remotely in checkout 2: local
#      runs of the remote command write the same bytes;
#   7. the static library as a thin archive (llvm-ar qcT), with a symbol
#      table too large to be read whole: checkout 1 with minimal downloads
#      keeps it remote, reading its members with ranged reads, and links
#      with them remotely; checkout 2 downloads everything.
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
#   TLS         1: the frontend requires client certificates (mTLS), with
#               the fake server's test certificates
#   NINJA_RS_FLAGS  more ninja-rs flags for every build (e.g.
#               --remote_include_scanner=preprocessor)
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
REMOTE=grpc://127.0.0.1:$PORT
TLS_LISTENER=
TLS_FLAGS=()
if [[ ${TLS:-} == 1 ]]; then
  certs=$here/crates/fake-reapi/testdata/tls
  REMOTE=grpcs://localhost:$PORT
  TLS_LISTENER=", tls: { cert_file: \"$certs/server.pem\", key_file: \"$certs/server-key.pem\", client_ca_file: \"$certs/ca.pem\" }"
  TLS_FLAGS=(--tls_certificate="$certs/ca.pem" --tls_client_certificate="$certs/client.pem" --tls_client_key="$certs/client-key.pem")
fi
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
    { listener: { http: { socket_address: "127.0.0.1:$PORT"$TLS_LISTENER } }, services: {
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

read -r -a EXTRA_FLAGS <<< "${NINJA_RS_FLAGS:-}"
COMMON=("${TLS_FLAGS[@]}" "${EXTRA_FLAGS[@]}" --remote_instance_name=main --remote_verbose --remote_mnemonics=Compile,Archive,Link)
# Strict: every action the policy sends remotely must run there.
EXEC=(--remote_executor=$REMOTE --remote_strict "${COMMON[@]}")
CACHE=(--remote_cache=$REMOTE "${COMMON[@]}")

build() {  # $1 = checkout, $2 = log name, rest = flags
  local dir=$1 log=$WORK/$2.log
  shift 2
  "$NINJA_RS" -C "$dir/build" "$@" > "$log" 2>&1 || { tail -40 "$log"; exit 1; }
  echo "  executed remotely: $(grep -c 'ninja: executed remotely' "$log" || true)," \
    "remote cache hits: $(grep -c 'ninja: remote cache hit' "$log" || true)"
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

# Local runs of prepared actions use the remote command (relative paths), so
# no output names the checkout it was built in.
no_checkout_paths() {  # $1 = checkout
  while read -r out; do
    [[ -f $1/build/$out ]] || continue
    if grep -q "$1" "$1/build/$out"; then echo "  $out names $1"; exit 1; fi
  done < <("$NINJA_RS" -C "$1/build" -t targets all | grep -E ': [A-Z]+_(COMPILER|STATIC_LIBRARY_LINKER|SHARED_LIBRARY_LINKER|EXECUTABLE_LINKER)__' | cut -d: -f1)
  echo "  no output names the checkout's path"
}

echo "=== 1. checkout 1 executes remotely"
build "$A" exec1 "${EXEC[@]}"
executed=$(count 'ninja: executed remotely' exec1)
[[ $executed -gt 0 ]]
"$A/build/hello_c" > /dev/null
"$A/build/hello_cxx" > /dev/null

echo "=== 2. checkout 2, remote cache only: every action is a hit"
build "$B" cache1 "${CACHE[@]}"
[[ $(count 'ninja: remote cache hit' cache1) -eq $executed ]]
"$B/build/hello_c" > /dev/null
same_outputs

echo "=== 3. a change in checkout 2 misses, runs locally, and is stored"
echo 'const char* ninja_rs_rbe_change = "changed";' >> "$B/hello.c"
build "$B" cache2 "${CACHE[@]}"
[[ $(count 'ninja: remote cache hit' cache2) -eq 0 ]]
"$B/build/hello_c" > /dev/null
no_checkout_paths "$B"

echo "=== 4. the same change in checkout 1, executing remotely: all hits"
cp "$B/hello.c" "$A/hello.c"
build "$A" exec2 "${EXEC[@]}"
[[ $(count 'ninja: executed remotely' exec2) -eq 0 ]]
[[ $(count 'ninja: remote cache hit' exec2) -gt 0 ]]
same_outputs

echo "=== 5. racing in checkout 1, local runs starting at once: outputs still identical"
"$NINJA_RS" -C "$A/build" -t clean > /dev/null
build "$A" race --remote_executor=$REMOTE --remote_racing --remote_racing_delay=0 "${COMMON[@]}"
grep "racing:" "$WORK/race.log" | sed 's/^/  /'
"$A/build/hello_c" > /dev/null
same_outputs
no_checkout_paths "$A"

echo "=== 6. a change raced in checkout 1 (local runs may win), executed remotely in checkout 2: identical"
echo 'const char* ninja_rs_rbe_race = "raced";' >> "$A/hello.c"
build "$A" race2 --remote_executor=$REMOTE --remote_racing --remote_racing_delay=0 "${COMMON[@]}"
grep "racing:" "$WORK/race2.log" | sed 's/^/  /'
no_checkout_paths "$A"
cp "$A/hello.c" "$B/hello.c"
build "$B" exec3 "${EXEC[@]}"
same_outputs

echo "=== 7. a thin static library: kept remote with minimal downloads, its members read with ranged reads"
archive=libgreeter_static.a
for co in "$A" "$B"; do
  # Enough symbols for a symbol table larger than what's read whole (64 KiB).
  for i in $(seq 3000); do
    printf 'int ninja_rs_thin_archive_test_symbol_%04d_with_a_long_name() { return %d; }\n' "$i" "$i"
  done > "$co/many_symbols.cpp"
  echo 'target_sources(greeter_static PRIVATE many_symbols.cpp)' >> "$co/hello_native.cmake"
  "$CMAKE" -S "$co" -B "$co/build" -DCMAKE_CXX_ARCHIVE_CREATE="<CMAKE_AR> qcT <TARGET> <LINK_FLAGS> <OBJECTS>" \
    > "$co/configure-thin.log" 2>&1 || { tail -30 "$co/configure-thin.log"; exit 1; }
done
"$NINJA_RS" -C "$A/build" -t clean > /dev/null
build "$A" thin1 "${EXEC[@]}" --remote_download_minimal
[[ ! -e $A/build/$archive ]] || { echo "  $archive was downloaded"; exit 1; }
grep -qx "+ CMakeFiles/greeter_static.dir/greeter.cpp.o" "$A/build/.ninja_remote" || { echo "  no members recorded for $archive"; exit 1; }
echo "  $archive stays remote, $(grep -c '^+ ' "$A/build/.ninja_remote") members recorded"
"$A/build/hello_cxx" > /dev/null
build "$B" thin2 "${EXEC[@]}"
[[ $(head -c 8 "$B/build/$archive") == '!<thin>' ]]
# Named on the command line, the archive comes down: the same bytes.
build "$A" thin3 "${EXEC[@]}" --remote_download_minimal "$archive"
same_outputs
no_checkout_paths "$A"
echo "PASS"
