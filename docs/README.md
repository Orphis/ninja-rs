# ninja-rs documentation

ninja-rs builds `build.ninja` files like ninja and, for manifests annotated with the RBE
interface (such as those the CMake fork writes with `-DCMAKE_NINJA_RBE=ON`), runs their compiles,
archives and links through the Remote Execution API, with a disk cache, a remote cache, build
events, exact C and C++ include scanning and a per-user daemon.

Where to start:

- **New to ninja-rs:** [Getting started](user/getting-started.md), then
  [Using ninja-rs with CMake](user/cmake.md).
- **Setting up remote execution or caching for a project:** [Remote execution](user/remote-execution.md),
  [Caching](user/caching.md), [TLS and authentication](user/certificates.md),
  [Settings files](user/settings.md).
- **Something went wrong:** [Troubleshooting](user/troubleshooting.md).
- **Working on ninja-rs:** [Architecture](dev/architecture.md), then the page for the part you
  touch.

## User guide

| Page | What it covers |
| --- | --- |
| [Getting started](user/getting-started.md) | What ninja-rs is and isn't, building and installing it, using it in place of ninja, the deliberate differences from ninja |
| [Using ninja-rs with CMake](user/cmake.md) | The CMake fork, the toolchain file (remote inputs, platform), what CMake writes, configure-time builds, a complete worked example |
| [Remote execution](user/remote-execution.md) | Executor, instance and platform; which edges run remotely and why one doesn't; fallback and strict modes; jobs, retries, downloads, racing, the execution log |
| [Caching](user/caching.md) | The disk cache, a remote cache alone, what the cache key covers, which local results are stored and why some aren't |
| [TLS and authentication](user/certificates.md) | `grpcs://`, CA files and bundles, client certificates, the certificate rules, refusals and their hints |
| [Build events](user/build-events.md) | Sending builds to a Build Event Service: options, results URL, ids, what is sent, failures |
| [Include scanning](user/include-scanning.md) | Why ninja-rs scans includes, the scanners and their fallback chain, MSVC specifics, why a compile runs locally |
| [Settings files](user/settings.md) | `.ninjars` files: where they are found, precedence, `--config`, private settings, trust rules |
| [The daemon](user/daemon.md) | What the daemon speeds up, enabling and controlling it, its files, start-up failures and back-off |
| [Nested builds](user/nested-builds.md) | ninja-rs inside ninja-rs (custom commands, regeneration, `-t exec`): what nested builds share, the lock |
| [Platforms](user/platforms.md) | Windows, WSL, Linux and macOS specifics |
| [Troubleshooting](user/troubleshooting.md) | Messages and what they mean, debugging options, exit codes, summaries, known limitations |

## Reference

| Page | What it covers |
| --- | --- |
| [Command-line options](reference/options.md) | Every option: value, default, meaning, Bazel or ninja equivalent |
| [Tools, debug modes and warning flags](reference/tools.md) | Every `-t` tool, `-d` mode and `-w` flag, and the differences from ninja 1.13 |
| [Environment variables](reference/environment.md) | Every variable ninja-rs reads or sets |
| [Files](reference/files.md) | Every file ninja-rs reads or writes, with layouts and formats |
| [RBE interface](rbe-interface.md) | The contract between a generator and an executor: bindings, toolchains and targets files, local tools |

## Developer guide

Each page explains, per mechanism, the use case, why it is harder than it looks, the solution
chosen and the alternatives rejected, its known limits, and the tests that guard it.

| Page | What it covers |
| --- | --- |
| [Architecture](dev/architecture.md) | The crates, the life of a build end to end, threads and runtimes, where state lives, design principles |
| [The engine](dev/engine.md) | `ninja-core`: parity with ninja, the plan and its local and remote slots, the build and deps logs, the lock, subprocesses, the jobserver |
| [The command line](dev/cli.md) | The `ninja-rs` crate: options, settings files, nested invocations, the exec runner, build events, certificates, tools |
| [The OS layer](dev/os.md) | `ninja-os`: file stamps and identities, process identity, process trees and jobs, locks, Windows security |
| [The remote backend](dev/remote.md) | `ninja-remote`: what is sent, command unwrapping, toolchains, link analysis, uploads, downloads, captures, the disk cache, minimal downloads |
| [The transport](dev/transport.md) | `rbe-client` and `rbe-bes`: connections, permits, retries and fault classification, TLS and refusal judgement, ByteStream, chunking, build events |
| [Include scanning](dev/include-scanning.md) | `cpp-deps` (the exact scanner, compiler profiles, cl's traditional preprocessor), the conservative scanner, the scanner chain |
| [The daemon](dev/daemon.md) | `ninja-daemon`: watching, vouching, fences, path keys and case folding, IPC, start-up and back-off |
| [Exec root and path mapping](paths.md) | How host paths become action paths and back |
| [Testing](dev/testing.md) | Unit tests, the e2e harness, the fake REAPI server, Bazel targets, lint rules, real-project measurements |
| [Build notes](build-notes.md) | Building ninja-rs itself: versions, Bazel and Cargo, patches, platforms |

[`design.md`](design.md) was the original design document; the developer guide replaces it.
