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

  -C DIR  -f FILE  -j N  -k N  -l N (ignored)  -n  -v/--verbose  --quiet  --status FMT
  -d explain|keepdepfile|keeprsp|stats|nostatcache  -w phonycycle=err|warn  --version
  -t list|targets|commands|inputs|query|deps|rules|clean|cleandead|recompact|restat
     |compdb|compdb-targets|missingdeps
  -t exec [--] COMMAND [ARGS...] (run COMMAND: the ninja-rs builds it runs take these
                                 options and report under this invocation)
  --jobserver-pool               (GNU make jobserver pool for the commands)

settings files (.ninjars, Bazel's .bazelrc format, see below):
  --config=NAME  --announce_rc  --enable_platform_specific_config
  --ninjars=FILE  --[no]system_rc  --[no]workspace_rc  --[no]home_rc  --ignore_all_rc_files

remote execution (only for manifests with rbe_version = 1):
  --remote_executor=grpc://host:port | grpcs://host:port
  --remote_cache=grpc://host:port  (remote cache alone: local execution on a miss)
  --[no]remote_upload_local_results  (with --remote_cache, default on)
  --remote_instance_name=NAME
  --remote_header=K=V            (repeatable)
  --remote_jobs=N                (default 50)
  --exec_root=DIR                (default: per-action common ancestor)
  --remote_mnemonics=Compile     (comma-separated mnemonic suffixes)
  --[no]remote_local_fallback    (default on)
  --remote_strict                (actions sent remotely run there or fail)
  --remote_max_fallbacks=N  --remote_max_fallback_ratio=R
                                 (cap local runs after remote failures)
  --remote_racing  --remote_racing_delay=MS
                                 (also run actions sent remotely locally, on free
                                 local slots; the first to finish wins)
  --remote_verbose               (log cache hits and local fallbacks)
  --remote_mnemonics=Compile,Link,Archive  (also links and archives)
  --remote_download_outputs=all|minimal    (minimal: deliverables, named targets,
                                            local inputs; alias --remote_download_minimal)
  --execution_log_file=FILE      (JSON line per remote action: key, command, inputs)
  --remote_include_scanner=conservative|preprocessor|clang-scan-deps|compiler
                                 (preprocessor: exact inputs for GCC/Clang compiles,
                                 from crates/cpp-deps; clang-scan-deps, compiler (-M,
                                 /showIncludes): external alternatives; default
                                 conservative)
  --disk_cache=DIR               (local action cache + CAS, with or without a remote)
  --disk_cache_max_size=SIZE     (LRU eviction above SIZE, default 10G)
  --[no]remote_cache_compression (zstd transfers when the remote supports them,
                                 default on)
  --[no]daemon                   (or NINJA_RS_DAEMON=1: a daemon keeps scanned headers,
                                 compiler profiles, digests and toolchain trees between
                                 builds, watching their files; -t daemon status|stop)

TLS for grpcs:// endpoints (remote service and BES; flags named as in Bazel):
  --tls_certificate=FILE         (CA trusted besides the public roots, PEM)
  --tls_client_certificate=FILE  --tls_client_key=FILE  (mTLS client identity, PEM)
  --tls_authority_override=NAME  (name the server's certificate must have)

build events (Bazel's Build Event Protocol, flags named as in Bazel):
  --bes_backend=grpc://host:port | grpcs://host:port
  --bes_instance_name=NAME  --bes_header=K=V  --bes_results_url=URL
  --bes_keywords=A,B  --[no]bes_lifecycle_events  --bes_timeout=SECONDS
  --build_event_binary_file=PATH
  --build_event_publish_all_actions
  --build_metadata=K=V  --invocation_id=UUID  --build_request_id=UUID
```

### Settings files

Options can live in `.ninjars` files, which have the format and features of
Bazel's `.bazelrc`: `COMMAND[:CONFIG] OPTIONS...` lines, `import` and
`try-import`, `%workspace%`, `--config`, `--announce_rc`,
`--enable_platform_specific_config`. For example, at the top of a repository:

```
# Every build.
build --remote_include_scanner=preprocessor --remote_download_minimal
build --disk_cache=~/.cache/ninja-rs/disk

# ninja-rs --config=glass
build:glass --remote_executor=grpcs://glass.cluster.engflow.com
build:glass --bes_backend=grpcs://glass.cluster.engflow.com
build:glass --bes_results_url=https://glass.cluster.engflow.com/invocations/default/
build:glass --remote_mnemonics=Compile,Archive,Link --remote_jobs=200

# Certificates and other personal settings, out of the repository.
try-import %workspace%/user.ninjars
```

- **Files**, read in this order, later options winning: `/etc/ninjars`
  (`%ProgramData%\ninjars` on Windows); the workspace's, the first
  `.ninjars` found from the build directory (`-C`) up to the repository's
  root, whose directory is `%workspace%` (never in a file system's root, in
  `/tmp` or another directory shared by all users, nor one another user
  owns); `~/.ninjars`; the `--ninjars=FILE` files. Then the command line,
  where each `--config` adds its options in place.
- **Commands**: `common` and `always` lines apply to every invocation,
  `build` lines to builds and tools, `exec` lines to `-t exec` (which takes
  the `build` ones too); within a file, `common` lines go first. `common`
  lines skip the options ninja-rs doesn't have, and lines of Bazel's other
  commands (`test`, `query`, `startup`...) are skipped, so one file can serve
  both.
- **Paths**: `~` expands in path options; relative paths are relative to
  where ninja-rs started, as on the command line: use `%workspace%`.
- `--announce_rc` prints what each file gave; `--ignore_all_rc_files`,
  `--nosystem_rc`, `--noworkspace_rc` and `--nohome_rc` leave files out.

Builds that ninja-rs runs (a custom command running `cmake --build`) take
the outer invocation's options, `.ninjars` ones included, and report under
its invocation. `-t exec` does the same for builds started outside a build,
such as a CMake configure building external projects:
`ninja-rs --config=glass -t exec -- cmake -G Ninja -S . -B build`.

It can be used as CMake's make program:
`cmake -G Ninja -DCMAKE_MAKE_PROGRAM=/path/to/ninja-rs ...`
(`-t restat`, `-t recompact` and the regeneration loop are supported).

State files live in the build directory (or `$builddir`):
`.ninja_rs_log`, `.ninja_rs_deps` (ninja-rs formats, so ninja's own files are
left alone) and `.ninja_rs_digests` (file digest cache for remote builds).

## Formatting

Rust code is formatted with `cargo fmt --all` (`rustfmt.toml`; the code
copied from cargo-rbe keeps its own). A pre-commit hook refuses commits
with unformatted Rust files; enable it once per clone:

```
git config core.hooksPath .githooks
```

## Tests

- `bazelisk test //...` (or `cargo test`): ninja-core unit tests (ported from
  ninja's manifest parser, build, depfile, dyndep, lexer and clparser tests),
  ninja-remote unit tests (unwrapping, tokenizers, toolchains, path mapping,
  scanner, policy, store), rbe-client tests, and
  `//crates/ninja-rs:remote_e2e`: a hand-written contract-style `build.ninja`
  built through an in-process fake REAPI server (`crates/fake-reapi`), with
  the test binary itself acting as the compiler, and
  `//crates/ninja-rs:cli_e2e`: tools, `--status` and the jobserver (pools,
  external pools, GNU make 4.3 pipes) against ninja's outputs.
- `tests/nativelink/run.sh` (Linux): builds hermetic-llvm-cmake's sample
  (`tests/hello`) with its toolchain set up for remote execution (the
  toolchain cache inside each checkout, CMake's Ninja generator in RBE
  mode) against a local NativeLink (scheduler + local worker), in two
  checkouts at different paths: remote execution in one, the remote cache
  alone in the other, local misses stored and reused, outputs identical:
  `NATIVELINK=/path/to/nativelink HERMETIC=/path/to/hermetic-llvm-cmake
  CMAKE=/path/to/cmake tests/nativelink/run.sh` after
  `bazelisk build //crates/ninja-rs`.

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

Compiles, archives and links run remotely (`--remote_mnemonics`); a link's
linker, library directories and side outputs are found from the compiler
driver's dry run. With `--remote_download_minimal`, only deliverables, named
targets and the inputs of local commands are downloaded (see
docs/design.md, "Minimal downloads").

Thin archives work too: an action reading one gets its members, and with
minimal downloads a thin archive stays remote, its member list read with
ranged reads past its symbol table (see docs/design.md, "Thin archives").

Without remote execution, a remote cache (`--remote_cache`) shares the
results of actions run locally, across machines and checkouts (see
docs/design.md, "Remote cache only").

A local disk cache (`--disk_cache`) caches the actions remote execution
would run, with or without a remote service; large blobs are stored as
content-defined chunks, which similar outputs share, and travel as the
chunks the other side lacks with a remote that splits and splices blobs
(see docs/design.md, "Disk cache").

Outputs evicted from the remote cache during a build are rebuilt by their
own actions in the same build (action rewinding).

GNU make's jobserver is supported as a client (FIFO, pipe and Windows
semaphore) and as a pool (`--jobserver-pool`).

For developers, a daemon (`--daemon`) keeps what builds learn (scanned
headers, compiler profiles, file digests, toolchain trees) and watches the
files they come from, so that a build checks again only what changed (see
docs/design.md, "Daemon").

Not done yet (later phases): lease extension in minimal-download mode,
running whole builds in the daemon.

## Layout

```
MODULE.bazel .bazelversion .bazelrc BUILD.bazel Cargo.toml Cargo.lock
platforms/                 host platforms for rules_rs/@llvm
crates/ninja-core/         manifest language, graph, build engine, local runner
crates/ninja-remote/       REAPI backend, scanner, path mapping, store
crates/cpp-deps/           C/C++ preprocessor finding a compile's inputs
crates/ninja-daemon/       daemon keeping caches between builds, file watching
crates/ninja-rs/           Service + CLI (bin "ninja-rs"), e2e test
crates/fake-reapi/         in-process fake REAPI server (+ standalone bin)
third_party/cargo-rbe/     rbe-client copied from cargo-rbe
tests/nativelink/run.sh    NativeLink end-to-end test
docs/                      contract copy, design, paths, build notes
```
