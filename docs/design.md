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
- Not implemented: jobserver, `-l` (accepted, ignored),
  `-t browse/graph/compdb/missingdeps/msvc/wincodepage/multi-inputs/urtle`.

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
`rbe_target`, `rbe_deliverable`) and hands it to a `CommandRunner`. Runners
return a `TaskResult`; the builder then extracts deps, restats and logs
exactly as ninja does. The build loop is `async` (tokio); runners spawn tasks
and the loop awaits completions.

`ExecRunner` (in `ninja-rs`) runs a task locally or through a
`RemoteBackend` (trait in `ninja-core`, implemented in `ninja-remote`).
Local work is limited by `-j`, remote work by `--remote_jobs`; tasks wait for
a slot of their kind inside their own tokio task, so up to `-j +
--remote_jobs` tasks are in flight. Console-pool tasks always run locally.
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
   `PATH`, 8.3 short names expanded on Windows) and looked up in
   the `rbe_toolchains` file by `tool` (case-insensitive on Windows). Its
   `inputs` are sent whole as grafted Merkle subtrees, its `platform` becomes
   the REAPI platform. Without a declaration only the executable is sent.
4. **Include scanning** (`scanner`), see below.
5. **Exec root and path rewriting** (`paths`), see [paths.md](paths.md).
6. **Action**: `Command` (rewritten argv, `output_paths` and legacy
   `output_files` relative to the working directory, working directory,
   env `INCLUDE` for cl-style compilers, platform) and `Action` (command and
   input root digests, platform). Response files are put in the input root
   with rewritten contents.
7. **Cache, upload, execute**: `GetActionResult` first (only successful
   results are reused); otherwise `FindMissingBlobs` + `BatchUpdateBlobs`
   (ByteStream for large blobs, via `rbe-client`) and `Execute`.
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

## Later phases (designed for, not built)

- **Minimal download**: the builder's state is already behind
  `CommandRunner` results; a digest-based output state (path → digest,
  in `Store`) can replace mtimes for remote outputs. `deliverable = 1` is
  passed through in `Task::bindings`, so only those outputs would be
  fetched; local edges would request their inputs on demand (the runner
  sees every input path). Lease extension / action rewinding would hang off
  `Store::uploaded` (already the set of blobs we believe the CAS has).
- **BES**: `ActionMeta` (mnemonic, target) is already sent in
  `RequestMetadata`; `cargo-rbe`'s `rbe-bes` crate (ordered BES stream with
  acks and retries) can be reused, with targets from the File API codemodel.
- **Daemon with file watching**: `Service` is already reusable across
  builds; the remaining work is keeping `State` between builds (it has
  `reset()`), invalidating `Store` entries from file watcher events, and
  replacing the process-wide `chdir` in the CLI with per-build working
  directories in `Disk` and the runners.
