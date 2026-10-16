# ninja-rs

A ninja-compatible build executor written in Rust, with native Remote Execution API (REAPI v2)
support for `build.ninja` files annotated with the generator-neutral RBE interface
([docs/rbe-interface.md](docs/rbe-interface.md)), such as those CMake's Ninja generators write
with `CMAKE_NINJA_RBE`.

- **A drop-in for ninja.** It reads `build.ninja` unchanged and builds locally like ninja 1.13
  (its own port of ninja's parser and build engine, dyndep and the jobserver included), so it can
  be CMake's make program.
- **Remote execution and caching.** When the manifest has `rbe_version = 1`, compiles (and, if
  asked, archives and links) run through REAPI: their inputs are found by scanning, sent as a
  Merkle tree, looked up in the action cache, executed, and their outputs downloaded, all or only
  those needed. A remote cache alone, or a local disk cache, reuses results across builds,
  checkouts and machines.
- **Exact include scanning.** A C and C++ preprocessor finds exactly the headers each compile
  reads, for GCC, Clang, clang-cl and cl (cl's traditional preprocessor included), falling back
  to the compiler's own list when it can't be exact.
- **Build events.** Builds are reported to a Build Event Service, as Bazel reports them.
- **A daemon** keeps what builds learn (file digests, scanned headers, compiler profiles) and
  watches the files they come from.
- Nested invocations, `.ninjars` settings files in Bazel's `.bazelrc` format, Windows, Linux and
  WSL.

## Quickstart

A CMake project, built remotely:

```sh
# 1. Build ninja-rs (or with Bazel: see docs/build-notes.md).
cargo build --release -p ninja-rs

# 2. Configure with the CMake fork (the ninja-rbe branch) in RBE mode, ninja-rs as the make program.
cmake -G Ninja -S . -B build -DCMAKE_NINJA_RBE=ON \
  -DCMAKE_MAKE_PROGRAM=$PWD/target/release/ninja-rs -DCMAKE_TOOLCHAIN_FILE=toolchain.cmake

# 3. Build, with compiles, archives and links sent to a remote service.
ninja-rs -C build --remote_executor=grpcs://remote.example.com:443 \
  --remote_mnemonics=Compile,Archive,Link --remote_download_minimal
```

The toolchain file says what a compiler needs on a worker and which workers run it; see
[Using ninja-rs with CMake](docs/user/cmake.md) for a complete example with
[hermetic-llvm-cmake](https://github.com/Orphis/hermetic-llvm-cmake), whose toolchain works as it
is. With a remote cache alone (`--remote_cache=...`), misses run locally and their results are
stored; `--disk_cache=DIR` keeps results on this machine. Options can live in a `.ninjars` file.

## Documentation

The [documentation](docs/README.md) has three parts:

- **User guide:** [getting started](docs/user/getting-started.md),
  [CMake](docs/user/cmake.md), [remote execution](docs/user/remote-execution.md),
  [caching](docs/user/caching.md), [TLS and authentication](docs/user/certificates.md),
  [build events](docs/user/build-events.md), [include scanning](docs/user/include-scanning.md),
  [settings files](docs/user/settings.md), [the daemon](docs/user/daemon.md),
  [nested builds](docs/user/nested-builds.md), [platforms](docs/user/platforms.md),
  [troubleshooting](docs/user/troubleshooting.md).
- **Reference:** [options](docs/reference/options.md), [tools](docs/reference/tools.md),
  [environment variables](docs/reference/environment.md), [files](docs/reference/files.md),
  [the RBE interface](docs/rbe-interface.md).
- **Developer guide:** [architecture](docs/dev/architecture.md), then one page per part, each
  explaining the problems it solves, why they are harder than they look, and the solution chosen.

## Building and testing

```sh
bazelisk build //crates/ninja-rs     # hermetic toolchains; Windows needs the Microsoft license
bazelisk test //...                  # accepted in user.bazelrc, see docs/build-notes.md
cargo build && cargo test            # with your local Rust toolchain
```

Rust code is formatted with `cargo fmt --all`; enable the pre-commit hook once per clone with
`git config core.hooksPath .githooks`. See [build notes](docs/build-notes.md) and
[testing](docs/dev/testing.md).

## Layout

```text
crates/ninja-core/         manifest language, graph, build engine, local runner
crates/ninja-remote/       REAPI backend, conservative scanner, path mapping, disk cache
crates/cpp-deps/           C/C++ preprocessor finding a compile's inputs
crates/ninja-daemon/       daemon keeping caches between builds, file watching
crates/ninja-os/           what the crates share from the OS
crates/ninja-rs/           the CLI (bin "ninja-rs"), build events, settings, e2e tests
crates/fake-reapi/         in-process fake REAPI and BES server for tests
crates/test-dirs/          test directories and runfiles
third_party/cargo-rbe/     REAPI client and BES uploader
tests/nativelink/run.sh    end-to-end test against a local NativeLink
docs/                      the documentation
```

## License

Apache-2.0 ([LICENSE](LICENSE)); third-party notices in [NOTICE](NOTICE).
