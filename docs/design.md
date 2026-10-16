# ninja-rs design notes

This document was the original design description. It has been replaced by the
[developer guide](README.md#developer-guide), which describes each part as it is now: the use case,
why it is harder than it looks, the solution chosen and its limits. The full former text is in
the repository's history (`git show 43f29d9:docs/design.md`).

Where its sections went:

| Former section | Now |
| --- | --- |
| Goals | [Architecture](dev/architecture.md) (design principles) |
| Own engine, not n2; deliberate differences from ninja; jobserver; performance notes | [The engine](dev/engine.md), [Getting started](user/getting-started.md) (differences) |
| Crates; engine and runners; racing | [Architecture](dev/architecture.md), [The engine](dev/engine.md), [The command line](dev/cli.md) |
| Service and caches; connections | [The transport](dev/transport.md), [The remote backend](dev/remote.md) |
| Remote execution flow; thin archives | [The remote backend](dev/remote.md), [Remote execution](user/remote-execution.md) |
| Include scanner; preprocessor; benchmarks | [Include scanning](dev/include-scanning.md), [user view](user/include-scanning.md) |
| Build events (BES) | [The transport](dev/transport.md), [Build events](user/build-events.md) |
| Settings files | [The command line](dev/cli.md), [Settings files](user/settings.md) |
| Nested invocations | [The engine](dev/engine.md), [The command line](dev/cli.md), [Nested builds](user/nested-builds.md) |
| Disk cache; split and spliced blobs; compression; remote cache only | [The remote backend](dev/remote.md), [Caching](user/caching.md) |
| Minimal downloads | [The remote backend](dev/remote.md), [Remote execution](user/remote-execution.md) |
| Daemon | [The daemon](dev/daemon.md), [user view](user/daemon.md) |

## Future work (designed for, not built)

- **Minimal downloads, further**: lease extension (keeping remote-only
  blobs alive rather than rebuilding them once evicted); a digest-based
  state (an output whose digest didn't change doesn't dirty
  what depends on it, ninja's `restat` for every remote edge); a regex mode
  (download generated headers for IDEs).
- **Builds in the daemon**: the daemon serves caches, not builds. Running
  them there (`Service` is already reusable across builds) would also keep
  the manifest, logs and `State` (it has `reset()`) and the remote
  connections between builds, and skip ninja's own stats from watcher
  events; it needs per-build working directories in `Disk` and the
  runners instead of the CLI's process-wide `chdir`, and each build's
  environment and terminal for local commands. macOS watching (FSEvents)
  is missing too.
