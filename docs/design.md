# ninja-rs design notes

## Goals

`ninja-rs` reads ordinary `build.ninja` files and behaves like ninja for
local builds. When the manifest opts in (`rbe_version = 1`, see
[rbe-interface.md](rbe-interface.md)) and a REAPI endpoint is given, it
runs remote-eligible edges through the Remote Execution API v2.

## Own engine, not n2

[n2](https://github.com/evmar/n2) (Apache-2.0, by ninja's author) was
considered and rejected:

- **No dyndep.** n2's own feature comparison lists dynamic dependencies as
  missing; its parser only accepts the `dyndep` binding name. CMake needs
  dyndep for C++20 modules and Fortran, so this alone is disqualifying.
- No `console` pool semantics (treated as depth 1, output buffered), partial
  `subninja`, no `-n`, few `-t` tools, Windows "incomplete" (backslash path
  handling).
- Its single-threaded work loop and on-disk DB are designed as a standalone
  binary; we want an embeddable, async engine with a pluggable runner.

ninja-rs therefore ports ninja's algorithms directly (Apache-2.0 ninja,
upstream commit `4e4df1e`, 2026-09-24): the re2c lexer, manifest parser,
`BindingEnv`/`EdgeEnv` variable lookup (including its quirks), path
canonicalization with Windows slash bits, `DependencyScan::RecomputeDirty`,
`Plan` (wanted edges, pools, critical-path priority, `CleanNode` for restat,
dyndep loading and `RefreshDyndepDependents`), `Builder` (`StartEdge`,
`FinishCommand`, deps extraction, interruption cleanup), the depfile parser,
the /showIncludes parser, the dyndep parser/loader and the cheap `-t` tools.
Ninja's tests were ported as Rust unit tests where they apply: 51 manifest
parser tests (including the error message table), 68 build/plan tests
(dyndep, validations, restat, pools, failures, interruption, rsp files,
deps), 26 depfile parser tests, plus dyndep, lexer, canonicalization and
/showIncludes tests (all in `crates/ninja-core/src`).

### Deliberate differences from ninja

- **State files.** The build log and deps log use ninja-rs' own text formats
  in `.ninja_rs_log` / `.ninja_rs_deps`, so ninja and ninja-rs can share a
  build directory without corrupting each other. Consequently the first
  ninja-rs build in a ninja-built tree rebuilds everything ("command line not
  found in log"). Command hashes are ninja's rapidhash (log v2; v1 used
  FNV-1a and is discarded on load), so they equal ninja 1.13's. The deps log
  (v2) stores each path once and deps as path ids, like ninja's binary log;
  v1 repeated every path in every record (44.7 MB vs 1.75 MB for CMake's own
  build) and is discarded on load.
- `--version` prints `1.13.1` (the manifest language level implemented,
  including `$^`, validations and dyndep), which also satisfies CMake's
  feature checks.
- From ninja 1.14 (unreleased): `--status FMT` (the status line from
  `$`-variables: `$started`, `$finished`, `$total`, `$running`, `$rate`,
  `$progress`, `$elapsed`, `$eta`, ...; ninja-rs adds `$remote`,
  `$remote_cache_hits` and `$disk_cache_hits`), `NO_COLOR`/`FORCE_COLOR`,
  `-t inputs` flags and shell escaping, `-t rules -d`, `-t compdb` listing
  every input, `-t compdb-targets` and the jobserver (below).
- Not implemented: `-l` (accepted, ignored),
  `-t browse/graph/msvc/wincodepage/multi-inputs/urtle`.
- Tools write `\n` line endings on Windows too (ninja writes `\r\n`).
- Failed commands summary (ninja issue 2378): with many jobs, a `FAILED:`
  block scrolls away behind the output of the commands still running.
  Before "build stopped", the failed commands are shown again (at most 10,
  each with the first 20 lines of its output), unless the only one is
  still the last thing printed.
- Direct exec on POSIX (ninja issue 2665): a command without shell syntax
  (only letters, digits, spaces and `_-+=/.,:@%`), not starting with a
  variable assignment or a shell builtin or keyword, runs without
  `/bin/sh -c`, saving a shell per command (about 0.1 ms each: 5000
  `touch` commands take 0.99 s instead of 1.57 s). CMake's compile
  commands qualify. A program that can't start runs through the shell
  instead, for the shell's message and status (127).
- Dyndep outputs met as sources (ninja issues 2653 and 2743, open). When
  the dirty scan loads a dyndep file, its implicit outputs gain a producer,
  but edges already scanned may read them and took them for sources. ninja
  leaves those edges clean and builds them on the next run; with C++20
  modules, a consumer scanned before the target exporting the module is
  compiled against the old BMI. ninja-rs sends those edges and everything
  downstream back to unvisited: edges still on the scan's stack visit their
  inputs again, and when earlier targets of the same command line are
  affected, the builder scans them again and adds what became dirty to the
  plan. Still as in ninja: a dyndep output named directly (`ninja g.h`)
  whose dyndep file the scan never reaches is taken for a source.
- As in ninja 1.14 (`0b115d8`): an edge the scan already found dirty
  doesn't load its depfile or deps log entry into the graph, only checks
  that it exists, so stale discovered inputs can't order the build or form
  cycles. When a restat leaves such an edge's inputs clean, its deps are
  loaded then and decide (ninja PR 2842, open). Unlike ninja, the edge's
  task still lists them, as hints: remote actions send them, local ones
  download them first, and missing ones are skipped.
- `$^` doesn't require `ninja_required_version = 1.14`, since `--version`
  is 1.13.1.

### Jobserver

As in ninja 1.13: without `-j`, a GNU make jobserver named by `MAKEFLAGS`
limits local commands, each taking a token (the client's implicit slot
first) and giving the same byte back when done. As in ninja 1.14
(unreleased), `--jobserver-pool` (or `enable_jobserver_pool = 1` in the
manifest) creates a pool of `-j` slots for the commands, through `MAKEFLAGS`
in their environment, unless a pool already exists or with `-j1`. Pools are
FIFOs on Unix, named semaphores on Windows. The client also speaks GNU make
4.3's pipe protocol (`--jobserver-auth=R,W`), which upstream ninja dropped
and Kitware's fork restores. Remote actions take no token; local fallbacks
do. Tokens are taken inside the tasks in the order they started (by
priority), polling every few milliseconds, since other processes' releases
aren't signalled.

### Performance notes

Local overhead (parse, logs, dirty scan) matches ninja on a 20,000-edge
manifest; `-d stats` prints the same metrics as ninja to compare. The things
that made the difference:

- **Stat cache (Windows).** `std::fs::metadata` opens each file (~75 µs);
  like ninja, `RealDisk` lists each directory once and caches the mtimes
  while scanning for dirty nodes (~1.3 µs per stat). The cache is dropped
  before commands run so restat sees fresh timestamps; `-d nostatcache`
  disables it.
- **Lexing values.** In a variable value, spaces, `:` and `|` stay in the
  current literal run instead of being appended one at a time.
- **Already-canonical paths** skip the canonicalization rebuild.
- **FxHash** for the graph's maps instead of SipHash (keys are not
  adversarial; ninja uses MurmurHash for the same tables).
- **No teardown at exit.** The CLI sets `Service::skip_teardown`, leaking the
  graph and logs instead of freeing them just before the process exits.
- **Deps by id.** The builder resolves each deps-log path id to a node once
  per build (`deps_node_cache`), instead of hashing every header path again
  for every object that includes it.

On CMake's own build tree (1,136 edges, deps from `/showIncludes`), a no-op
takes 80 ms vs ninja 1.13.2's 69 ms; ~4 ms of that is process startup.
- Deps are loaded on the first visit of an edge (ninja ≤1.12 behavior), not
  lazily only when the edge is clean (1.13); results are the same.
- The smart terminal status line pads with spaces instead of using ANSI
  erase sequences, and the terminal width comes from `COLUMNS` (default 80).

## Crates

| Crate | Role |
|---|---|
| `crates/ninja-core` | Manifest language, graph, logs, dirty checking, plan, builder, local subprocesses, status printer. No global state; all I/O through `Disk` and `CommandRunner` traits. |
| `crates/ninja-remote` | REAPI backend: policy, command unwrapping, include scanner, path mapping, action construction, upload/execute/download, caches (`Store`). |
| `crates/ninja-rs` | `Service` (in-process executor service) and the CLI. |
| `crates/fake-reapi` | In-process fake REAPI server for tests (CAS, AC, Capabilities, ByteStream, Execution as local subprocesses). |
| `third_party/cargo-rbe/rbe-client` | REAPI client copied from the user's cargo-rbe project (see its README). |

### Engine and runners

The builder owns the graph (`State`) and never shares it: for each edge it
evaluates a self-contained `Task` (command, rspfile, depfile, deps type,
outputs, inputs, and the RBE bindings `rbe_local`, `rbe_mnemonic`,
`rbe_target`, `rbe_config`, `rbe_artifact`, `rbe_deliverable`) and hands it
to a `CommandRunner`. Runners return a `TaskResult`; the builder then extracts deps, restats and logs
exactly as ninja does. The build loop is `async` (tokio); runners spawn tasks
and the loop awaits completions.

`ExecRunner` (in `ninja-rs`) runs a task locally or through a
`RemoteBackend` (trait in `ninja-core`, implemented in `ninja-remote`).
Local work is limited by `-j` (or a jobserver's tokens), remote work by
`--remote_jobs`; tasks wait for a slot of their kind inside their own tokio
task, so up to `-j + --remote_jobs` tasks are in flight. Console-pool tasks always run locally.
A backend may answer `Fallback(reason)` to have the task run locally (command
not unwrappable, unknown compiler, or a service failure with
`--remote_local_fallback`).

### Service and caches

`Service` holds the remote connection and an `Arc<Store>` across builds; the
CLI runs one build in-process. `Store` contains:

- file digests keyed by (symlink-resolved path, size, mtime), persisted to
  `$builddir/.ninja_rs_digests` after remote builds (this matters: hashing
  a 150 MB toolchain takes seconds in release builds and minutes in debug);
- include scanner caches: parsed directives per (path, size, mtime);
  resolved includes per (file version, search path); directory listings
  (never for directories under the build dir); built-in include dirs per
  (compiler, relevant flags, language);
- toolchain input trees (built once per directory, concurrent requests wait
  on a `OnceLock`);
- hashes known to be in the CAS (skips `FindMissingBlobs` for them).

All keys are plain data (paths, sizes, mtimes, digests) so the store can be
persisted wholesale later, which the daemon needs.

## Remote execution flow

1. **Policy** (`backend::policy_wants`): remote only if the manifest has
   `rbe_version = 1` (checked once when the backend is created), an executor
   was given, the edge isn't `rbe_local = 1`, doesn't regenerate the manifest
   (`generator = 1`), isn't in the console pool, and its `rbe_mnemonic` ends
   with one of `--remote_mnemonics` (default `Compile`).
2. **Unwrapping** (`cmdline::unwrap_command`): strips `[path\]cmd.exe /C "..."`
   (Windows), splits `&&` chains outside quotes, drops the shell no-ops
   `cd .`, `cd /D .`, `:` and `true`, and tokenizes each step with
   `CommandLineToArgvW` rules (Windows) or POSIX shell word rules. Anything
   needing a shell otherwise (pipes, redirections, expansions) falls back to
   local. Steps running a tool the toolchains file declares `local` are run
   on the host before and after the remote action (`Prepared::local_before`,
   `local_after`); local steps between remote ones, or a command made only
   of local steps, fall back to local. Several remote steps run through
   `/bin/sh -c`, on POSIX hosts only.
3. **Toolchain**: argv[0] is resolved (relative to the build dir or via
   `PATH`, 8.3 short names expanded on Windows, a symlink keeping its name:
   LLVM's tools all link to one multi-call `llvm` binary) and looked up in
   the `rbe_toolchains` file by `tool` (case-insensitive on Windows). Its
   `inputs` are sent whole as grafted Merkle subtrees, its `platform` becomes
   the REAPI platform. Without a declaration only the executable is sent.
4. **Include scanning** (`scanner`), see below; a VFS overlay
   (`-ivfsoverlay`, `-vfsoverlay`) is an input. **Links** (`link`): a
   compiler driver that compiles nothing links. Its dry run (`-###`, first
   on the command line: arguments after clang-cl's `/link` go to the linker)
   gives the linker's command line; a linker's own arguments are read
   directly. From it come the linker binary (as the driver names it), files
   named by flags (`/vfsoverlay:`, `/def:`, `-T`, version scripts...), side
   outputs (`/pdb:`, `/implib:`, `-Map`), requested as optional outputs, and
   the library directories. Which libraries a link pulls in depends on
   directives inside objects and libraries, so library directories outside
   the build directory (once symlinks and junctions are resolved: the
   toolchain's) are sent whole, as grafted trees like toolchain inputs; the
   build's own libraries are declared inputs. A driver without a dry run
   sends its declared inputs only. On Windows hosts, arguments naming an
   input by a relative path are sent with forward slashes, so that an
   archive's member names don't depend on the host.
5. **Exec root and path rewriting** (`paths`), see [paths.md](paths.md).
6. **Action**: `Command` (rewritten argv, `output_paths` and legacy
   `output_files` relative to the working directory, working directory,
   env `INCLUDE` for cl-style compilers, platform) and `Action` (command and
   input root digests, platform). Response files are put in the input root
   with rewritten contents.
7. **Cache, upload, execute**: `GetActionResult` first (only successful
   results are reused); otherwise `FindMissingBlobs` + `BatchUpdateBlobs`
   (ByteStream for large blobs, via `rbe-client`) and `Execute`. When the
   server splices blobs, a large one is uploaded as its chunks instead
   (see "Split and spliced blobs").
8. **Download**: outputs are written atomically; files whose content is
   already identical are left untouched so `restat` keeps working. The
   depfile is rewritten to local paths before the builder reads it
   (`deps = gcc`); `/showIncludes` lines in stdout are rewritten for
   `deps = msvc`. stdout and stderr are concatenated like a local run, with
   the output of the local steps run before and after.

## Include scanner

`parse_compile` understands gcc/clang and cl/clang-cl command lines:
`-I`, `/I`, `-isystem`, `-iquote`, `-idirafter`, `-isystem-after`, `-imsvc`,
`/imsvc`, `/external:I`, `-include`, `-imacros`, `/FI`, `--sysroot`,
`-isysroot`, `/winsysroot`, `-nostdinc`, `/X`, `-x`, `/TP`, `/TC`,
`-fmodule-file=`, `-include-pch`, `-fprofile-use=` and `@file` response
files (expanded recursively; UTF-16 rsp files are decoded).

Built-in include directories are discovered once per (compiler, target,
sysroot, stdlib, ... flags, language) by running `<compiler> -E -x <lang> -v`
on an empty input and parsing the search list (cl.exe: `INCLUDE`).

Scanning follows `#include`, `#include_next`, `#import` and
`__has_include(...)` conservatively: conditionals are ignored and *every*
search directory that contains a candidate contributes it (which also covers
`include_next`). Header-like strings in `#define` lines are treated as
candidates too. A computed include (`#include MACRO`) sends the containing
directory whole with a warning. Files under toolchain input directories are
not added individually (the grafted subtree has them). Declared edge inputs
(including deps from earlier builds) are always sent.

## Build events (BES)

`crates/ninja-rs/src/bes.rs` reports a build in Bazel's Build Event
Protocol, to a Build Event Service (`--bes_backend`) through `rbe-bes`
(copied from cargo-rbe: ordered stream with acks, retries and lifecycle
events) and/or to a file (`--build_event_binary_file`):

- `ReportingStatus` wraps the status printer: every edge start/finish and
  message goes to both. `EdgeInfo` carries the edge's `rbe_*` bindings and,
  when it ran remotely, the digests of its outputs (`TaskResult::cas_outputs`,
  filled by the backend from the `ActionResult`).
- `Bes::plan` runs once the requested targets are in the builder's plan
  (before `already_up_to_date`, so no-op builds report too): the edges in
  `Plan::want` give the targets (`rbe_target`, `rbe_config`), their
  artifacts, and how many edges each still has to run (`Want::ToStart`).
  It posts the configurations, the pattern's expansion and one
  `TargetConfigured` per label; targets with nothing to run complete at once.
- A target completes when its last edge finishes, or fails with its first
  failed edge. Its outputs are CAS references (`bytestream://` from
  `--remote_executor`) when built remotely, `file://` with a digest (cached
  in `Store`) otherwise. At the end, targets that never completed are
  `Aborted` (`SKIPPED` after a failure, `USER_INTERRUPTED`), then come
  `BuildFinished`, `BuildMetrics` (per mnemonic and runner) and
  `BuildToolLogs`.
- Failed edges are `ActionExecuted` events (all of them with
  `--build_event_publish_all_actions`); the status lines and command output
  are `Progress`.
- The invocation id (`--invocation_id`, else random) is also the
  `tool_invocation_id` of every remote request, whose `target_id` is the
  label and `configuration_id` the configuration's id.

Build events never fail the build. Edges a dyndep file adds during the
build are not counted in their target's remaining edges.

## Disk cache

`--disk_cache=DIR` (`ninja-remote/src/disk_cache.rs`) is a local action cache
and CAS with REAPI's keys: the action digest `prepare()` computes, and blob
digests. Entries are in remote form (paths relative to the action's working
directory), so they are interchangeable with a remote cache's.

- **Lookup order:** disk, then the remote action cache. A disk hit runs the
  edge's local-tool steps, restores the outputs through the same code as a
  download (depfile and `/showIncludes` mapped back to local paths) and
  reports `ExecKind::DiskCacheHit`.
- **With a remote:** blobs come from the disk when it has them, else from
  the remote CAS, and are kept on disk; successful results are stored with
  their stdout/stderr inline.
- **Without a remote:** a miss returns `RemoteOutcome::RunLocally` with a
  `LocalCapture`: the runner runs the command locally (within `-j`), then the
  capture stores its declared outputs, its depfile and `/showIncludes` lines
  rewritten to remote form (`PathMapper::rewrite_arg`), and its output. A
  missing output or a failure stores nothing.
- Only cacheable actions use it: those the execution policy would send
  remotely (`rbe_version` manifests, not `rbe_local`, mnemonic suffixes from
  `--remote_mnemonics`), since the key needs the scanned inputs.
- **Eviction**, as in Bazel's disk cache: reads and hits update files'
  modification times, and the least recently used files are removed while
  the cache exceeds `--disk_cache_max_size` (down to 90% of it). Finding
  them walks the whole cache, which takes time (about 190 ms for 120k
  files), so after a build that wrote to the cache only the `size`
  estimate is updated; the walk runs when the estimate passes the limit,
  and rewrites it with the real size. `gc/lock` lets one process collect
  at a time (the others skip and count their writes next time); a file
  used during the walk is kept, and of files of the same age, action
  results go first, then chunk layouts, then blobs.
- **Content-defined chunking:** a blob larger than 2 MiB is stored as its
  FastCDC 2020 chunks (REAPI `FAST_CDC_2020` with the recommended
  parameters: 512 KiB average, 128 KiB to 2 MiB, normalization level 2,
  seed 0, using the `fastcdc` crate REAPI names as the reference), each a
  blob of `cas/`, plus a layout in `chunked/<hh>/<hash>`: a
  `SplitBlobResponse` listing their digests. Two builds of a large output
  differing by a few edits share most chunks, which are stored once.
  `read_blob` / `put_*` split and reassemble, so callers never see chunks;
  `put_file` streams a large file through the chunker (checking its hash
  again, in case it changed since it was digested). A blob is present when
  it is stored whole or when its layout and all its chunks are, so evicting
  a chunk turns the blobs using it into misses, and using a blob marks its
  layout and chunks, so a shared chunk lives as long as one of its blobs is
  used. Blobs stored whole by earlier versions are still read, so the
  format version is unchanged.
- **Layout:** `VERSION`, `cas/<hh>/<hash>`, `chunked/<hh>/<hash>`,
  `ac/<hh>/<hash>`, `size`, `gc/lock`, `tmp/` (atomic writes); a version
  mismatch disables the cache with a warning.

### Split and spliced blobs

With a remote cache that advertises them (`CacheCapabilities`), large blobs
travel as content-defined chunks, so a blob that differs from one seen
before by a few edits only sends the chunks around them:

- **Download** (`split_blob_support`, needs `--disk_cache`): a missing blob
  larger than the disk cache's chunk limit is split by the server
  (`GetChunkMapping` from RE API v2.13, `SplitBlob` before, picked from
  `high_api_version`); only the chunks the disk cache lacks are fetched,
  each checked against its digest before it's stored, and the assembled
  blob against its own, as REAPI asks. The disk cache keeps the server's
  chunks and its `SplitBlobResponse` as the layout. A server that can't
  split the blob, or any failure, falls back to fetching it whole.
- **Upload** (`splice_blob_support`): a missing input larger than four
  times the average chunk is chunked with the server's
  `FastCdc2020Params` (REAPI's recommended ones if it advertises none, the
  request then naming no chunking function), the chunks it lacks are
  uploaded (one `FindMissingBlobs` per blob), then
  `RegisterChunkMapping` (v2.13) or `SpliceBlob` composes the blob. A
  refused splice uploads the blob whole.
- Chunking lives in `rbe-client` (`chunking.rs`, the `fastcdc` crate's
  `v2020` module, REAPI's reference), shared by the disk cache, uploads
  and the fake server. The fake REAPI server serves either API version
  (`State::set_chunking`), and the end-to-end tests check that after an
  edit in the middle of a 10 MB source, about 6% of it is uploaded and of
  its object downloaded.

## Minimal downloads

`--remote_download_outputs=minimal` (or `--remote_download_minimal`) leaves
the outputs of remote actions in the remote CAS, except:

- the outputs of `rbe_deliverable = 1` edges (executables and shared
  libraries by default), with their side outputs (the PDB);
- what the command line names, through phony targets, even when it is up
  to date;
- depfiles, which the build reads;
- what a local command reads: before a task runs on the host (a local edge,
  a fallback, `rbe_local`), `RemoteBackend::materialize` downloads its
  inputs that only exist remotely.

`remote_outputs::RemoteOutputs` records the others in `.ninja_remote` in the
build directory: path, digest, executable bit and a time. The build's
`Disk` is wrapped (`RemoteDisk`) so that a recorded output missing locally
stats with its recorded time: ninja's up-to-date checks, build log and deps
log work unchanged, and a recorded output is as present as a local one. A
downloaded file gets its recorded time, so downloading it later doesn't make
what depends on it stale. Remote actions read recorded inputs by digest
(`InputTree::add_remote_file`). A local write of a recorded path, or a
remote action producing it again, drops the record; a stale local copy of a
remote-only output is deleted so local commands can't read it; `-t clean`
deletes the file. A file the build itself reads (a dyndep file) is
downloaded when it is read.

Eviction: at the start of a build, one `FindMissingBlobs` over the recorded
digests drops the records the remote CAS lost, so the build runs the
actions producing them again. The disk cache only keeps results whose
outputs were all downloaded.

**Action rewinding**, for blobs lost during the build. The backend notices
when an upload's `FindMissingBlobs` misses a remote-only input
(`rbe_client::MissingRemoteBlobs`), when Execute fails with
`FAILED_PRECONDITION` and a remote-only input is missing, or when a
download for a local command fails and the blob is missing. It drops those
records and returns the task's result with `lost_inputs`. The engine
(`Builder::rewind`) then puts the edge back to waiting, marks the lost
nodes dirty, and plans their producing edges again (whether they ran in this
build or an earlier one). The edge runs once they finish, like any edge
waiting for its inputs. The status line counts it as not started
(`Status::build_edge_rewound`). A lost input with no edge to make it (a
source), or a third rewind of the same edge, fails it.

Two checks keep rewinding from finding the same stale data again. A cache
hit whose outputs left the CAS is a miss in minimal mode, and Execute then
skips the cache. On `FAILED_PRECONDITION` without lost remote-only inputs,
blobs believed uploaded were evicted: they are uploaded again and Execute
retried once.

The default stays `all` for now. The design doc's default is `minimal`.

## Later phases (designed for, not built)

- **Minimal downloads, further**: lease extension (keeping remote-only
  blobs alive rather than rebuilding them once evicted); a digest-based
  state (an output whose digest didn't change doesn't dirty
  what depends on it, ninja's `restat` for every remote edge); a regex mode
  (download generated headers for IDEs).
- **Daemon with file watching**: `Service` is already reusable across
  builds; the remaining work is keeping `State` between builds (it has
  `reset()`), invalidating `Store` entries from file watcher events, and
  replacing the process-wide `chdir` in the CLI with per-build working
  directories in `Disk` and the runners.
