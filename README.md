# ninja-rs

A ninja-compatible build executor written in Rust, with native Remote
Execution API (REAPI v2) support for `build.ninja` files annotated with the
generator-neutral RBE interface ([docs/rbe-interface.md](docs/rbe-interface.md)),
such as those CMake's Ninja generators write with `CMAKE_NINJA_RBE`.

- Reads `build.ninja` unchanged and builds locally exactly like ninja
  (own port of ninja's parser and build engine, including dyndep).
- When the manifest has `rbe_version = 1` and `--remote_executor` is given,
  edges whose `rbe_mnemonic` ends in `Compile` and that aren't
  `rbe_local = 1` run through REAPI: C/C++ includes are scanned (no separate
  scan step), inputs become a Merkle tree, the action cache is checked, the
  action executed and the outputs downloaded. Depfiles and `/showIncludes`
  output are mapped back to local paths. Steps of a command running a tool
  declared local (such as `cmake -E rm -f` before archiving) run on the
  host around the remote action.

Design: [docs/design.md](docs/design.md) · path mapping:
[docs/paths.md](docs/paths.md) · build/toolchain notes:
[docs/build-notes.md](docs/build-notes.md).

## Building

Bazel (Bzlmod) with the hermetic LLVM toolchain (`llvm` module) and
`rules_rs`; Bazel 9.3.0 via bazelisk.

```sh
# Linux x86_64 (also from WSL)
bazelisk build //crates/ninja-rs          # -> bazel-bin/crates/ninja-rs/ninja-rs
bazelisk test //...

# Windows x86_64, MSVC ABI. Needs the Microsoft license acceptance in a
# git-ignored user.bazelrc (see .bazelrc), and the patches in
# third_party/patches (applied automatically; see docs/build-notes.md).
bazelisk build //crates/ninja-rs
bazelisk test //...

# Windows GNU ABI binary cross-built from Linux (no Microsoft downloads)
bazelisk build --config=cross_windows_gnullvm //crates/ninja-rs
```

For development, `cargo build` / `cargo test` also work (they use your
local Rust toolchain and linker, unlike the Bazel build).

## Usage

```
ninja-rs [options] [targets...]

  -C DIR  -f FILE  -j N  -k N  -l N (ignored)  -n  -v/--verbose  --quiet
  -d explain|keepdepfile|keeprsp|stats|nostatcache  -w phonycycle=err|warn  --version
  -t list|targets|commands|inputs|query|deps|rules|clean|cleandead|recompact|restat

remote execution (only for manifests with rbe_version = 1):
  --remote_executor=grpc://host:port | grpcs://host:port
  --remote_instance_name=NAME
  --remote_header=K=V            (repeatable)
  --remote_jobs=N                (default 50)
  --exec_root=DIR                (default: per-action common ancestor)
  --remote_mnemonics=Compile     (comma-separated mnemonic suffixes)
  --[no]remote_local_fallback    (default on)
  --remote_verbose               (log cache hits and local fallbacks)
  --disk_cache=DIR               (local action cache + CAS, with or without a remote)
  --disk_cache_max_size=SIZE     (LRU eviction above SIZE, default 10G)

build events (Bazel's Build Event Protocol, flags named as in Bazel):
  --bes_backend=grpc://host:port | grpcs://host:port
  --bes_instance_name=NAME  --bes_header=K=V  --bes_results_url=URL
  --bes_keywords=A,B  --[no]bes_lifecycle_events  --bes_timeout=SECONDS
  --build_event_binary_file=PATH
  --build_event_publish_all_actions
  --build_metadata=K=V  --invocation_id=UUID  --build_request_id=UUID
```

It can be used as CMake's make program:
`cmake -G Ninja -DCMAKE_MAKE_PROGRAM=/path/to/ninja-rs ...`
(`-t restat`, `-t recompact` and the regeneration loop are supported).

State files live in the build directory (or `$builddir`):
`.ninja_rs_log`, `.ninja_rs_deps` (ninja-rs formats, so ninja's own files are
left alone) and `.ninja_rs_digests` (file digest cache for remote builds).

## Tests

- `bazelisk test //...` (or `cargo test`): ninja-core unit tests (ported from
  ninja's manifest parser, build, depfile, dyndep, lexer and clparser tests),
  ninja-remote unit tests (unwrapping, tokenizers, toolchains, path mapping,
  scanner, policy, store), rbe-client tests, and
  `//crates/ninja-rs:remote_e2e`: a hand-written contract-style `build.ninja`
  built through an in-process fake REAPI server (`crates/fake-reapi`), with
  the test binary itself acting as the compiler.
- `tests/nativelink/run.sh` (Linux): builds a small C++ project through
  ninja-rs against a local NativeLink (scheduler + local worker), with remote
  compiles by the hermetic clang from the llvm module:
  `NATIVELINK=/path/to/nativelink tests/nativelink/run.sh` after
  `bazelisk build -c opt //crates/ninja-rs`.

## Status

| Milestone | Status |
|---|---|
| 1. Skeleton (Bazel 9.3.0, llvm 0.8.24, rules_rs 0.1.0, Rust 1.99.0, crates from Cargo.lock) | Linux: done, hermetic toolchains verified. Windows MSVC: configured, blocked on the Microsoft license acceptance (user decision), so unverified. Windows gnullvm binary cross-built from Linux runs on Windows. |
| 2. Local ninja (parser, graph, depfile/deps, dyndep, pools, restat, validations, rspfiles, `-C -f -j -k -n -v -t`) and the RBE execution policy + command unwrapping | Done. Builds CMake projects (incl. C++20 modules with dyndep, and ninja itself) on Windows with no-op and incremental rebuilds behaving like ninja. |
| 3. REAPI client (Capabilities, CAS, ByteStream, AC, Execute), Merkle trees with digest cache, action construction, outputs + depfile/showIncludes mapping, flags | Done (client reused from cargo-rbe). |
| 4. Include scanner | Done (gcc/clang/cl/clang-cl flags, rsp files, conservative directive following, built-in dirs via `-v`, caches, parallel per action). |
| 5. Integration | Done: fake REAPI server e2e test (Windows + Linux, also in Bazel) and NativeLink 1.7.4 e2e (Linux/WSL). Also verified manually: the RBE-mode CMake fork's output for a C++20 modules project built remotely on Windows through the fake server. Docker was not used. |

Build events (BES/BEP) are reported with targets, configurations and
outputs from the RBE interface (see docs/design.md, "Build events").

A local disk cache (`--disk_cache`) caches the actions remote execution
would run, with or without a remote service; large blobs are stored as
content-defined chunks, which similar outputs share, and travel as the
chunks the other side lacks with a remote that splits and splices blobs
(see docs/design.md, "Disk cache").

Not done yet (later phases): minimal-download mode, daemon mode; jobserver.

## Layout

```
MODULE.bazel .bazelversion .bazelrc BUILD.bazel Cargo.toml Cargo.lock
platforms/                 host platforms for rules_rs/@llvm
crates/ninja-core/         manifest language, graph, build engine, local runner
crates/ninja-remote/       REAPI backend, scanner, path mapping, store
crates/ninja-rs/           Service + CLI (bin "ninja-rs"), e2e test
crates/fake-reapi/         in-process fake REAPI server (+ standalone bin)
third_party/cargo-rbe/     rbe-client copied from cargo-rbe
tests/nativelink/run.sh    NativeLink end-to-end test
docs/                      contract copy, design, paths, build notes
```
