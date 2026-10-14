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
  `-t browse/graph/msvc/multi-inputs/urtle`.
- `-t wincodepage` says `Build file encoding: UTF-8`, on every system:
  ninja-rs reads build files as UTF-8 and gives Windows commands and paths
  in UTF-16, whatever the code page. Without the tool, CMake on Windows
  writes build files in the ANSI code page, and non-ASCII paths come out
  mangled.
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
| `crates/cpp-deps` | A C/C++ preprocessor that finds the files a compile reads: directives evaluated exactly, text skipped (see "Preprocessor"). No dependencies. |
| `crates/ninja-daemon` | The daemon keeping the preprocessor's, digests' and toolchain trees' caches between builds, watching their files (see "Daemon"). |
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

Fallbacks are guarded, so that a broken remote setup doesn't quietly turn
builds local:

- **Summary.** After the build, `N of M remote actions ran locally`, split
  into those that couldn't be sent and those after remote failures (a
  warning), each with its first reason.
- **Limits.** `--remote_max_fallbacks=N` and `--remote_max_fallback_ratio=R`
  (checked from 20 remote actions on) cap local runs after remote
  failures; past them, a remote failure fails its action, saying which
  limit, and the build stops as for any failure. Actions that couldn't be
  sent aren't counted: they don't depend on the service, and some setups
  have them by design (multi-step commands on Windows hosts).
- **Strict.** `--remote_strict`: an action the policy sends remotely runs
  there or fails with the reason, for checking a remote setup. Cache
  misses of `--remote_cache` still run locally: that's the mode, not a
  fallback.

Every local run of an action the backend prepared (a cache miss of
`--remote_cache` or `--disk_cache`, a fallback after a remote failure, a
raced local run) runs the remote action's command (`LocalCommand`): its
argv, with paths relative to the working directory (locally, the build
directory), and its rewritten response files: the task's own (ninja's
`rspfile`) rewritten in place, any other a private copy in
`.ninja_rs_rsp/`, named by its contents, which the command then names (a
checked-in `@flags.rsp` is never modified, nor read half written by a
concurrent command). It writes the bytes remote
execution would, rather than ones with the checkout's absolute paths (in
debug info, say): stored results, and the inputs of later actions, don't
depend on the checkout. Its depfile and `/showIncludes` lines are mapped
back to local paths as a download's are. Actions that couldn't be
prepared, and commands with steps on the host, run as the build file has
them.

### Racing

`--remote_racing` runs an action the policy sends remotely locally too,
and the first branch to finish wins (`run_raced` in `exec_runner.rs`):

- **Gated on local capacity.** After `--remote_racing_delay` (250 ms by
  default, so that most remote cache hits don't start a local command),
  and once the action is prepared, the local branch takes a local slot
  only if one is free: it never queues ahead of local tasks (the
  semaphore gives released slots to its waiters; jobserver tokens are
  tried the same way). When the remote branch gives up (a fallback, a
  cache miss without remote execution), it queues like any local task.
- **One writer.** Whichever branch first needs to write the outputs
  claims them (`Race`). The remote one claims before restoring a cache
  hit or downloading a result; when the local command is running, the
  claim stops it and every process it started (a process group on POSIX,
  a job object on Windows) and waits until they have exited. A local
  command that finished first claims its result, and the remote branch
  stops without writing. A command with steps on the host isn't raced:
  the remote branch claims before running them.
- **Same bytes.** The local branch runs the remote action's command (see
  above), so the winner doesn't change the outputs, nor the keys of the
  actions that read them.
- After the build: `racing: N actions finished first locally, M remotely`.
  `--remote_strict` excludes racing.

### Service and caches

`Service` holds the remote connection and an `Arc<Store>` across builds; the
CLI runs one build in-process. `Store` contains:

- file digests keyed by the symlink-resolved path and a `FileStamp`:
  size, modification time, and on Unix change time and inode (which no tool
  sets: an archive extracted in place can give new contents an old
  modification time), on Windows creation time (new for a file extracted
  into a new directory; not for one rewritten in place, nor created again
  under its name within 15 s, which NTFS gives the old one's). Persisted to
  `$builddir/.ninja_rs_digests` after remote builds, but for files modified
  within the time granularity before (a change since could go unseen: 2 s
  for whole-second times, else 100 ms).
  This matters: hashing a 150 MB toolchain takes seconds in release builds
  and minutes in debug;
- include scanner caches: parsed directives per (path, size, mtime);
  resolved includes per (file version, search path); directory listings
  (never for directories under the build dir); built-in include dirs per
  (compiler, relevant flags, language);
- toolchain input trees (built once per directory, concurrent requests wait
  on a `OnceLock`);
- hashes known to be in the CAS (skips `FindMissingBlobs` for them).

All keys are plain data (paths, sizes, mtimes, digests) so the store can be
persisted wholesale later, which the daemon needs.

### Connections

`grpcs://` endpoints (and endpoints without a scheme) use TLS through
tonic and rustls: server certificates are checked against the web PKI
roots plus `--tls_certificate`, and `--tls_client_certificate` with
`--tls_client_key` (PEM; PKCS#8, PKCS#1 or SEC1 keys) authenticate the
client (mTLS). `--tls_authority_override` checks the server's certificate
against another name than the endpoint's host. The flags are Bazel's, and
apply to the remote service and the Build Event Service alike; `grpc://`
endpoints ignore them. Tokens go in `--remote_header` / `--bes_header`.
With TLS 1.3 the server refuses a client certificate after the handshake,
so the client sees a closed connection on its first call: errors on TLS
endpoints say which certificate was sent and which CAs were trusted
(`options::tls_hint`). The fake server can require client certificates
(`ServerTls`; test certificates in `crates/fake-reapi/testdata/tls`).

Calls failing for a transient reason (UNAVAILABLE, RESOURCE_EXHAUSTED,
ABORTED, INTERNAL, UNKNOWN, CANCELLED, DEADLINE_EXCEEDED: Bazel's
`RemoteRetrier` list; a batch's per-blob statuses too) are made again, up
to `--remote_retries` times (5), after 100 ms doubling up to 5 s, give or
take 10%. `--remote_timeout` (60 s, Bazel's units) bounds each call, and
for a transfer how long it may go without moving (ByteStream reads and
writes, which take as long as their size). Executions aren't bounded, as
in Bazel: an `Execute` stream broken once the operation is named is
followed again (`WaitExecution`); an operation the server lost (NOT_FOUND,
or UNIMPLEMENTED for `WaitExecution`) or that failed for a transient
reason (in the operation, or the response's status but DEADLINE_EXCEEDED:
the action timed out) executes again. FAILED_PRECONDITION (missing inputs)
comes back as the response's status, however the server reports it. What
still fails is a remote failure: run locally, or with `--remote_strict`
the action's failure. The fake server injects these (`State::fail_next`,
`break_next_executions`).

Connections set `TCP_NODELAY` and size their HTTP/2 flow-control windows
to the bandwidth-delay product (as grpc-java does), not 64 KiB: a lone
large transfer isn't throttled by window updates waiting behind delayed
ACKs. (The fake server needs the same: without it, one 190 MB upload took
8 s instead of under one, which concurrent duplicate uploads had hidden.)

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
   The declared inputs are the edge's, with phony inputs replaced by what
   they depend on (recursively), as ninja orders them: an edge depending on
   CMake's `all` gets the build's outputs, remote-only ones by digest.
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
   archive's member names don't depend on the host. **Thin archives**
   among the inputs of any action but a compile bring their members, see
   below.
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
   (see "Split and spliced blobs"). A large file is read as it is sent, a
   megabyte at a time (compressed as it goes, the frame stating its size),
   never whole in memory. A blob another action is uploading (the
   toolchain, for every action of a cold build) isn't uploaded again: the
   action waits for that upload (`Client::inflight`, shared by the
   client's clones), and uploads it itself should that one fail.
8. **Download**: a result is checked first (`check_result`), whichever
   cache or execution it comes from: an output it names that the action
   didn't request (an absolute path, `..`: a server, or whoever can write to
   a shared cache, would write anywhere), a declared output missing (the old
   local file would stay, the edge look built), or a symlink make a cached
   result a miss, and an executed one a failure (run locally). Outputs go
   where the action asked for them, never where the result says. Every blob
   read is checked against its digest (size and SHA-256, as Bazel's
   `--remote_verify_downloads` does), and so are inlined contents and the
   disk cache's blobs (a corrupt one is removed: a miss). Outputs are written
   atomically; files whose content is already identical aren't rewritten,
   but made newer than the inputs. File work (writing outputs, hashing the
   local copies, the disk cache) runs off the async workers
   (`block_in_place`), which go on serving transfers (HTTP/2 flow control)
   and build events meanwhile. The
   depfile is rewritten to local paths before the builder reads it
   (`deps = gcc`); `/showIncludes` lines in stdout are rewritten for
   `deps = msvc`. stdout and stderr are concatenated like a local run, with
   the output of the local steps run before and after.

## Thin archives

A thin archive (`llvm-ar --thin` or `qcT`, `ar T`, `llvm-lib
/llvmlibthin`; always the GNU format) holds the symbol table and, for each
member, a header naming the file, relative to the archive's directory: an
action reading it reads its members too. They aren't declared inputs, so
`archive::expand` adds them: for each input that is a thin archive (on disk,
read once per version of the file by `Store::thin_members`, or recorded as
remote-only with its members), its members, resolved against its directory,
and theirs if they are thin archives too. A remote action gets them like
other inputs, by digest when they only exist remotely; `materialize`
downloads them with the archive for a local command. Compiles read no
archives and skip this. Member paths survive the input root's layout (tree
paths are relative to the exec root); a member named by an absolute path
wouldn't, and makes the action run locally. The action's key includes the
members' digests: a thin archive's bytes only depend on its members'
names, sizes and symbols.

With minimal downloads an archive often stays remote, so its members are
recorded with it in `.ninja_remote` (`+ <name>` lines after its entry),
read when the action producing it finishes. The member list follows the
symbol table, which is most of a thin archive: the first ranged ByteStream
read (`read_offset`, `read_limit`, through `Client::read_range`) gets the
magic and the first header, the second everything past the symbol table
(`archive::symbols_end`, `archive::members_from`). Archives up to 64 KiB
are read whole, and one in the disk cache is read there. An output is read
this way when its command creates a thin archive (`archive::creates_thin`:
the `T` modifier, `--thin`, `/llvmlibthin`) or its name ends with `.a` or
`.lib`; if the reads fail, it is downloaded.

`restat` and thin archives don't mix: when a member changes but not its
size or symbols, the archive's bytes stay the same, the download leaves it
untouched, and with `restat = 1` on the archiving edge the link would be
skipped. CMake doesn't set `restat` on archives.

## Include scanner

`parse_compile` understands gcc/clang and cl/clang-cl command lines:
`-I`, `/I`, `-isystem`, `-iquote`, `-idirafter`, `-isystem-after`, `-imsvc`,
`/imsvc`, `/external:I`, `-include`, `-imacros` (also behind `-Xclang`),
`/FI`, `--sysroot`,
`-isysroot`, `/winsysroot`, `-nostdinc`, `/X`, `-x`, `/TP`, `/TC`,
`-fmodule-file=`, `-include-pch`, `-fprofile-use=` and `@file` response
files (expanded recursively; UTF-16 rsp files are decoded).

Built-in include directories are discovered once per (compiler, target,
sysroot, stdlib, ... flags, language) by running `<compiler> -E -x <lang> -v`
on an empty input and parsing the search list (cl.exe: `INCLUDE`).

Scanning follows `#include`, `#include_next`, `#import` and
`__has_include(...)` conservatively: conditionals are ignored and *every*
search directory that contains a candidate contributes it (which also covers
`include_next`). Comments are skipped (glibc's `limits.h` says
`#include's` in one). Header-like strings in `#define` lines are treated as
candidates too, so a computed include (`#include MACRO`) whose macro the
scanned files define as a header (directly or through other macros) is
covered; otherwise it sends the containing directory whole, with a warning.
Files under toolchain input directories are not added individually (the
grafted subtree has them). Declared edge inputs (including deps from
earlier builds) are always sent.

### Preprocessor

`--remote_include_scanner=preprocessor` finds a GCC or Clang compile's
inputs with `crates/cpp-deps`, a preprocessor that evaluates directives
exactly and skips everything else; when it can't (unsupported flags such
as `-iprefix`, a compiler it can't probe, a result it marks inexact, or an
active `#include` it can't find: a lookup it may have got wrong drops
inputs otherwise), the conservative scanner above runs instead. Headers
compiled alone (`-x c++-header`, as for a precompiled header) are scanned
as their language, `.S`/`.sx` as `assembler-with-cpp`; `-include` and
`-imacros` behind `-Xclang` are forced includes, and a precompiled header
(`-include-pch`, also behind `-Xclang`) is an input that is not scanned,
when the header it was made from is forced too (otherwise its macros are
unknown: the scan falls back).

Lookups follow the compilers': forced includes in the working directory
then along the `#include "..."` chain (GCC, Clang, clang-cl's `/FI`), or as
`#include "..."` in the source (`cl`'s `/FI`); GCC's and Clang's search
lists without duplicates, a `-I` that is also a system directory searched
only there; a framework directory passes over names without a framework
part. A UTF-8 byte order mark is skipped; a UTF-16 file makes the scan
inexact. Queries `cl` only evaluates in `#if` get their value one bit at a
time (`__has_cpp_attribute(nodiscard) >= 201907L` needs the value).

- **Lexing** (`lex`): each file is reduced once per version to its
  directive lines, tokenized, with `#define`s parsed and include guards
  detected; text is skipped, exactly about what hides a `#` (comments,
  string and raw string literals, digit separators, line splices). The
  lexed files and directory listings are shared by every compile, and
  trusted for a build outside the build directory (`Cache` sessions).
- **Macros** (`macros`): expansion with hide sets (Prosser's algorithm),
  `#`, `##` with placemarkers, `__VA_ARGS__`, `__VA_OPT__`, GNU's
  `, ## __VA_ARGS__`, `push_macro`/`pop_macro`. Only `#if`/`#elif` and
  `#include` operands are expanded; in `#if`, `defined`, `__has_include`
  and the compiler's queries are evaluated as they appear.
- **`#if`** (`expr`): C's 64-bit integer arithmetic; `true`/`false` and
  the alternative operators (`and`, `or`, `not`, `bitand`...) by dialect
  (C++; `<iso646.h>` macros in C). A value may be unknown: `&&`, `||` and
  `?:` still decide when one side is enough; otherwise every branch that
  can't be ruled out is processed and the result is inexact.
- **Includes** (`scan`): quote, `-iquote`, `-I`, `-isystem`, built-in,
  `-idirafter` chains, `#include_next`, `#pragma once`, guards (a guarded
  header isn't processed again), `#import`, MSVC's includer-stack lookup.
  Files `__has_include` finds are inputs too (absent remotely, the answer
  would differ; Clang lists them in its depfiles).
- **Rarer inputs**: Apple frameworks (`-F`, `-iframework`, and built-in
  framework directories: `<Foo/Bar.h>` in `Foo.framework/Headers` or
  `PrivateHeaders`), Clang header maps (an `-I` that is a `.hmap` file),
  VFS overlays (`-ivfsoverlay`, `-Xclang -ivfsoverlay`, `/vfsoverlay:`;
  their flow-style YAML, case-insensitive views, `directory-remap`,
  fallthrough, fallback and redirect-only; lookups use the virtual paths,
  the real files are reported), C23's `#embed` (with `--embed-dir`) and
  `__has_embed`, and `_Pragma("...")` written in text (`once`,
  `push_macro`).
- **The compiler's knowledge** (`probe`, `driver`), once per compiler and
  flags that matter (`-std`, `-target`, `-O`, `-m`, `-f`, sysroot...;
  not `-I`, `-D`, outputs), for three command styles. GCC and Clang:
  predefined macros and operators from `-dM -E`, built-in directories
  from `-E -v`, files every compile reads from `-E -M` on an empty source
  (GCC pre-includes `stdc-predef.h`). `clang-cl`: `/clang:-dM /E` and
  `-v /E`. `cl.exe`: `/EP /Zc:preprocessor /PD` (MSVC prints its macros
  only with the conforming preprocessor; `_MSVC_TRADITIONAL` is set back
  to 1 for compiles without it), operators from `#ifdef` lines through
  `/EP`, built-in directories from `INCLUDE` (unless `/X`). The `#if`
  dialect is probed too: `true` (C++, C23) and the alternative operators
  (`and`, `or`...: C++, or MSVC's conforming mode). Queries
  (`__has_builtin(x)`, `__has_cpp_attribute(x)`...) are answered by running
  the compiler on generated lines, in batches (as `#if` tests for those it
  doesn't expand in text), then the compile is scanned again; answers are
  kept per profile. `cl`-style compiles search the directories of every
  includer. Static profiles per compiler and target could replace probing
  later.

Checked against the compiler's own `-M` (`crates/cpp-deps/examples/compare.rs`,
a compilation database in, differences out) with the hermetic toolchain and
GCC 13 in WSL:

| Corpus | Compiles | Files from `-M` | Missing | Extra | Scan | `-M` |
|---|---|---|---|---|---|---|
| 256 files including random sets of 36 libc++ headers | 256 | 187,385 | 0 | 0 | 9.0 ms | 98 ms |
| every libc++ header alone, C++17/20/23 | 324 | 70,922 | 0 | 0 | 4.2 ms | 28 ms |
| every glibc header alone, C17 and `_GNU_SOURCE` | 390 | 6,435 | 0 | 0 | 0.8 ms | 9.7 ms |
| every libstdc++ header alone with GCC, C++17/23 | 210 | 18,159 | 0 | 35 | 2.0 ms | 16 ms |

All exact; the extras are headers `__has_include` found, which GCC's `-M`
doesn't list (and which are inputs either way). Times are per compile on
one thread, warm (the first compile of a profile also probes the compiler:
40–180 ms). For comparison on the first corpus: `clang-scan-deps` in one
batch 18–25 ms per compile and thread, the conservative scanner 24–26 ms
with 2.5 times the files. The NativeLink test passes with the preprocessor
(`NINJA_RS_FLAGS=--remote_include_scanner=preprocessor`): the sample's C++
compiles send 460–574 files instead of 1,478–1,539, and outputs are
identical.

Two alternatives, smaller and exact by construction but a process per
compile (`cpp_deps::external`): `--remote_include_scanner=clang-scan-deps`
runs the `clang-scan-deps` next to the compiler on the command
(`-format=make`), and `--remote_include_scanner=compiler` runs the
compiler itself without its outputs, with `-M` (GCC, Clang; GCC doesn't
list what `__has_include` finds) or `/E /showIncludes` (`cl`, `clang-cl`;
the build's `msvc_deps_prefix`). In the NativeLink test both send exactly
the preprocessor's inputs (70 for the C compile, 460 and 574 for the C++
ones), and the outputs are identical.

On Windows, against the compilers' `/showIncludes` (`compare.rs` uses
`/E /showIncludes` for `cl`-style commands): the hermetic toolchain's
`clang-cl` building for the MSVC ABI (its headers through a case-insensitive
VFS overlay), 5 compiles, 720 files, none missing (3 extra: a header
`__has_include_next` found, which `/showIncludes` doesn't list); and
`cl.exe` 14.44 with the system Windows SDK, every MSVC standard library
header and common Windows headers alone in C++17, C++20 and C++latest with
`/Zc:preprocessor`, 393 compiles (21 more that `cl.exe` itself rejects),
27,979 files, none missing or extra. Warm scans take 1 to 14 ms; probing a
profile takes 0.3 to 0.7 s on Windows (process starts).

Not handled yet: Apple sub-frameworks and
header-map lookups relative to a framework, `_Pragma` produced by macros
in text (text isn't expanded), MSVC's traditional preprocessor's own
expansion rules (only `#if` and `#include` operands are expanded, where they
rarely differ), raw string literals spanning a line splice.

### Benchmarks

`crates/ninja-remote/examples/scanbench.rs` runs every way of finding a
compile's inputs over a compilation database, twice with their caches kept
(cold: probing compilers and reading headers; warm: as a daemon would),
with N threads taking compiles from a queue as a build does; the compiler
(`-M`, `/E /showIncludes`) is the reference for accuracy. Per compile, in
milliseconds (32 threads: wall time divided by the compiles), on a 32-thread
machine, Linux in WSL and Windows:

| Corpus | Method | 1 thread cold | warm | 32 threads cold | warm | Inputs | Missing | Extra |
|---|---|---|---|---|---|---|---|---|
| 256 files, random sets of 36 libc++ headers (Clang) | compiler | 97.6 | | 7.57 | | 732 | | |
| | conservative scanner | 26.0 | 25.3 | 4.33 | 3.16 | 1,629 | 0 | 229,642 |
| | preprocessor | 9.71 | 8.89 | 2.05 | 0.94 | 732 | 0 | 0 |
| | clang-scan-deps per compile | 72.0 | | 5.88 | | 732 | 0 | 0 |
| | clang-scan-deps one batch | 16.2 | | 12.8 | | | | |
| every libc++ header alone, C++17/20/23 (324) | compiler | 27.3 | | 2.05 | | 219 | | |
| | conservative scanner | 10.4 | 10.3 | 1.99 | 1.33 | 816 | 0 | 193,486 |
| | preprocessor | 4.84 | 2.33 | 2.94 | 0.26 | 219 | 0 | 0 |
| | clang-scan-deps per compile | 26.5 | | 1.89 | | 219 | 0 | 0 |
| | clang-scan-deps one batch | 5.86 | | 4.38 | | | | |
| every glibc header alone (390) | compiler | 9.25 | | 0.61 | | 17 | | |
| | conservative scanner | 0.40 | 0.10 | 0.48 | 0.04 | 25 | 0 | 3,383 |
| | preprocessor | 1.01 | 0.17 | 0.91 | 0.02 | 16 | 0 | 0 |
| | clang-scan-deps per compile | 11.3 | | 0.69 | | 16 | 0 | 0 |
| every libstdc++ header alone, GCC 13 (210) | compiler | 16.0 | | 1.31 | | 87 | | |
| | conservative scanner | 1.19 | 0.63 | 0.28 | 0.17 | 159 | 0 | 15,248 |
| | preprocessor | 1.93 | 0.88 | 1.08 | 0.13 | 87 | 0 | 35 |
| MSVC STL and Windows headers alone, `cl.exe` 14.44 (414) | compiler | 124.8 | | 17.2 | | 71 | | |
| | conservative scanner | 7.03 | 5.58 | 0.92 | 0.49 | 96 | 0 | 11,144 |
| | preprocessor | 6.62 | 2.85 | 3.32 | 0.25 | 69 | 0 | 0 |
| the same with the hermetic `clang-cl` and VFS overlay (402) | compiler | 134.6 | | 19.2 | | 149 | | |
| | conservative scanner | 15.6 | 14.3 | 2.15 | 1.40 | 250 | 0 | 41,005 |
| | preprocessor | 8.91 | 4.61 | 5.45 | 0.37 | 148 | 0 | 305 |
| | clang-scan-deps per compile | 137.7 | | 19.0 | | 152 | 0 | 692 |
| | clang-scan-deps one batch | 34.8 | | 6.14 | | | | |

- Nothing is missing anywhere: the conservative scanner is a superset (1.4
  to 4 times the files), the preprocessor is exact. Its extras are headers
  `__has_include` found, which GCC's `-M` and `/showIncludes` don't list
  (inputs either way); clang-scan-deps's are the same kind.
- Warm, the preprocessor is the fastest on every heavy corpus: 2 to 4.5
  times faster than the conservative scanner on one thread, 2 to 5 times
  with 32 (it scales about 9 times). On the small C corpora (16 to 25
  files per compile) all scanners take well under a millisecond.
- Cold, probing the compiler (three to five processes per profile, 0.1 to
  0.7 s) dominates the small corpora; the conservative scanner only runs
  `-v` once per profile.
- Per compile, `clang-scan-deps` costs about what the compiler's own `-M`
  does (a process reading every header each time). As one batch it's 4 to
  5 times faster on one thread, but it scaled poorly here (on Linux, 32
  threads were slower than per-compile processes).
- The compiler is 10 to 50 times slower than the preprocessor warm.

Making the preprocessor scale needed: read locks only on cached files and
directory listings, atomic counters, one probe per profile (concurrent
scans wait for it), one scan at a time asking the compiler queries, and
scans whose unknown queries another scan answered scanning again. A batch
of queries the compiler rejects is split in halves, so that one bad query
doesn't leave the others unanswered.

## Build events (BES)

`crates/ninja-rs/src/bes.rs` reports a build in Bazel's Build Event
Protocol, to a Build Event Service (`--bes_backend`) through `rbe-bes`
(copied from cargo-rbe: ordered stream with acks, retries and lifecycle
events) and/or to a file (`--build_event_binary_file`):

- `BuildStarted` waits for the manifest: once it is loaded (before any
  regeneration), `Bes::announce` sends the invocation's events, and
  `BuildStarted` announces as its `pattern` child what the build builds:
  the targets named on the command line, or else the manifest's defaults
  (`all` for CMake), a target of the `rbe_targets` file by its label. BES
  services take the invocation's title from there. Console messages from
  before are sent after it. The structured command lines (`original`,
  `canonical`) have Bazel's sections: executable, startup options (none),
  command (`build`), the options as given (`Options::given`) and the
  targets named as the residual; UIs show the command line from them.
- `ReportingStatus` wraps the status printer: every edge start/finish and
  message goes to both. `EdgeInfo` carries the edge's `rbe_*` bindings and,
  when it ran remotely, the digests of its outputs (`TaskResult::cas_outputs`,
  filled by the backend from the `ActionResult`).
- `Bes::plan` runs once the requested targets are in the builder's plan
  (before `already_up_to_date`): the edges they need (through every input
  and validation, whether the plan has them or not, and those the plan
  has besides) give the targets (`rbe_target`, `rbe_config`) and their
  artifacts; the plan, how many edges each still has to run
  (`Want::ToStart`). It posts the configurations, the pattern's expansion
  and one `TargetConfigured` per label; targets with nothing to run (up to
  date) complete at once, with their artifacts (CAS references when they
  were left remote): a no-op build reports every target, as Bazel does.
- A target completes when its last edge finishes or leaves the plan (a
  `restat` found its inputs unchanged: `Status::edge_removed_from_plan`
  says which edge, and so does `edge_added_to_plan` for one added since),
  or fails with its first failed edge. Its outputs are CAS references (`bytestream://` from
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

Build events never fail the build. Edges only a dyndep file's inputs
lead to are not counted in their target's remaining edges.

`NINJA_RS_BES_DEBUG=1` traces the upload on stderr (stream, acks,
lifecycle events).

## Settings files

`rc.rs` reads `.ninjars` files, with the format and features of Bazel's
`.bazelrc`, so that a project keeps its settings (executor, BES, disk cache,
mnemonics) in the repository and `cmake --build` or an IDE builds with them.

- **Which files** (`prescan`, `RcFiles::discover`): what the command line
  says decides, before any file is read (Bazel's startup options): `-C`,
  `-t`, `--ninjars=FILE`, `--[no]system_rc`, `--[no]workspace_rc`,
  `--[no]home_rc`, `--ignore_all_rc_files`. These only go on the command
  line. Then, in order: the system's (`/etc/ninjars`,
  `%ProgramData%\ninjars`); the workspace's, the first `.ninjars` from the
  build directory up to the repository's root (a `.git`, `.hg`, `.jj`,
  `.sl` or `.svn`), short of the home directory, whose directory is the
  workspace (else the build directory); never one someone else could have
  put there: in a file system's root, a directory shared by all users
  (`/tmp`: writable by all and sticky), or (on Unix) a file another user
  owns; the user's (`~/.ninjars`); the
  `--ninjars` files (`/dev/null` ends them; a missing one is an error). A
  file found twice is read once.
- **Lines**: `COMMAND[:CONFIG] OPTIONS...`, `import FILE`, `try-import FILE`
  (relative to the importing file; import loops are errors). `#` at a word's
  start comments out the rest, a trailing `\` joins the next line, words
  split like the shell's. `%workspace%` expands in imports and options (an
  extension: Bazel expands it in imports); `~` expands in path options, as
  Bazel does.
- **Commands** (`commands`): levels from least to most specific: `common`
  and `always`, then `build` (builds and tools), then `exec` (`-t exec`).
  A level's lines go in the order read, across files and imports; the
  levels in that order; then the command line. `common` lines skip options
  ninja-rs doesn't know (Bazel 7's rule), and with a note those whose
  values it can't take (a value only Bazel has); `always` and the others
  fail on them. So do all the lines of Bazel's own files (named
  `*bazelrc`): `import %workspace%/.bazelrc` shares the executor, cache
  and BES settings, and skips the rest. Bazel's other commands (`test`,
  `query`, `startup`...) are skipped, others warned about.
- **Options**: each is checked alone (`options::check_rc_option`): known,
  no target or tool, not one of the command-line-only options above. With
  no targets in files, a word after an option without `=` is its value,
  unless the option is a boolean. Values take Bazel's forms everywhere:
  booleans `--name`, `--noname`, `--name=true|false|yes|no|1|0`; an empty
  value unsets an option (`--disk_cache=` turns off a cache a file sets,
  `--remote_executor=` remote execution); durations a number and a unit
  (`60s`, `5m`, `500ms`; a number alone is the flag's old unit), and
  `--bes_timeout=0` waits as long as it takes; `--remote_download_outputs`
  takes Bazel's `toplevel` (ninja-rs's `minimal`).
- **Configs**: `--config=NAME` (or `--config NAME`), on the command line or
  in a file, adds the options of the `COMMAND:NAME` lines of the applicable
  commands, least specific first, where it is; configs nest, cycles and
  undefined configs are errors (a config defined for a Bazel command only
  is defined). `--enable_platform_specific_config` adds the config named
  after the host OS (`linux`, `macos`, `windows`, `freebsd`, `openbsd`), if
  defined, between the files' options and the command line's.
- `--announce_rc` prints each file's options by command, and each config's
  expansion.

The expanded options are parsed as one command line (`Expanded::args`), and
each remembers whether it was typed (`GivenOption::explicit`): build events
show the typed ones in the `original` structured command line and
`explicit_cmd_line`, all of them in the `canonical` one and `cmd_line`.

## Nested invocations

Builds start builds: a custom command running `cmake --build` (LLVM's
`NATIVE` tablegen build, ExternalProject's steps) runs ninja-rs again in
another build directory. They are one invocation:

- **Settings** (`nested.rs`): the outermost invocation exports
  `NINJA_RS_PARENT`, JSON with its invocation and build request ids, its
  remote settings as flags (executor, TLS, disk cache, mnemonics, download
  mode, scanner...; paths made absolute; not the BES and output flags), its
  execution log, a relay address and the `.ninjars` files it read. A nested
  invocation puts the flags before its own command line (its own flags
  win), takes the ids, so that every remote request has the outer
  `tool_invocation_id`, and appends to the execution log (one `write` per
  line: the processes share the file). Header values (`--remote_header`, often tokens) aren't in the
  environment every build command sees: they go in a file only the user
  can read (`flags-<pid>.json` in the daemon's private directory), which
  the variable names and the invocation removes when it ends. Build events
  and `--announce_rc` show headers' names, not their values. The files'
  options are among the
  flags, so it doesn't apply them again; it reads the files for the
  `--config`s of its own command line only. The environment carries it to
  grandchildren unchanged.
- **Build events**: with a BES backend or event file, the outermost
  invocation listens on a relay (`relay-<pid>`, a Unix socket or named pipe
  of `ninja-daemon`'s IPC); a nested invocation without its own BES flags
  connects and sends its events as length-prefixed `BuildEvent`s instead of
  opening a stream. The outer stream re-posts them (`Bes::relayed`):
  progress as its own; configurations, targets and actions under a
  configuration of their own (`<id>-<dir tag>`, mnemonic suffixed with the
  nested build directory, as Bazel shows a transition), output paths
  prefixed with that directory, file sets with the directory and the
  connection (each invocation numbers its own from 0, and LLVM runs a
  nested build per tool in `NATIVE`); metrics are added to the outer
  ones. Started, finished and pattern events stay the outer
  invocation's. A nested build without `rbe_version` (a `try_compile`)
  leaves the relay: it reports nothing. A nested stream ends with an empty
  frame, which the outer invocation acknowledges once it merged what came
  before: a nested process exits with its events in the outer stream.
- ExternalProject passes `CMAKE_MAKE_PROGRAM` and `CMAKE_NINJA_RBE` to the
  projects it configures with Ninja, so their builds run with ninja-rs.
- **`-t exec [--] COMMAND`** makes any command the outermost invocation:
  a configure building external projects, a script running several builds.
  It exports the same context, owns the stream (`BuildStarted` with the
  command `exec`, the command as the residual), runs the command and ends
  with its exit code. The `exec` lines of `.ninjars` files apply to it, and
  the `build` ones.

A nested build runs in a console-pool edge (CMake puts recursive builds
there). Its local jobs share the outer `-j` only through a jobserver pool
(`--jobserver-pool`), which it joins as a client; remote actions take no
token either way.

## Disk cache

`--disk_cache=DIR` (`ninja-remote/src/disk_cache.rs`) is a local action cache
and CAS with REAPI's keys: the action digest `prepare()` computes, and blob
digests. Entries are in remote form (paths relative to the action's working
directory), so they are interchangeable with a remote cache's.

- **Lookup order:** disk, then the remote action cache. A disk hit runs the
  edge's local-tool steps, restores the outputs through the same code as a
  download (depfile and `/showIncludes` mapped back to local paths) and
  reports `ExecKind::DiskCacheHit`. An output already there with the same
  contents isn't written again, but its modification time is set to now,
  as if it had been: older than an input (a header changed and changed
  back), it would make every later build run the action again.
- **With a remote:** blobs come from the disk when it has them, else from
  the remote CAS, and are kept on disk; successful results are stored with
  their stdout/stderr inline. With minimal downloads, a result is stored
  with the outputs that were downloaded; those left remote stay in the
  remote CAS (see "Minimal downloads").
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

### Compression

Blobs travel compressed with zstd when the remote advertises it
(`supported_compressors`), unless `--noremote_cache_compression` (Bazel's
flag; on by default here): ByteStream reads and writes use
`compressed-blobs/zstd/` resource names, batch reads list zstd as
acceptable, and batch updates are compressed when the server lists zstd
for them too (`supported_batch_update_compressors`). A blob under 1 KiB,
or one compression doesn't shrink, is sent as it is. Ranged reads (thin
archives' members) stay uncompressed, since their offsets are the blob's.
`--remote_verbose` prints the bytes transferred and those on the wire.
The fake server serves all of it (`State::set_compression`).

EngFlow's glass advertises zstd for ByteStream and batch reads, not for
batch updates. Rebuilding CMake after a clean, every action cached, took
10.9 s instead of 23.1 s with minimal downloads (199 MiB, 98 on the wire)
and 13.6 s instead of 22.9 s with all of them (318 MiB, 175 on the wire).
Spliced blobs' chunks are batched, unless the server compresses
ByteStream writes and not batches, as glass does: then each missing chunk
goes through ByteStream, compressed, 16 at a time per blob, as Bazel's
`ChunkedBlobUploader` sends them (it never batches uploads). A new 12 MB
source spliced to glass went over as 0.1 MiB instead of 9.9 (a
repetitive comment: real sources compress far less).

## Remote cache only

`--remote_cache=URL` uses a remote cache without remote execution: builds
without an executor still share results, across machines and checkouts.

- Actions are prepared as for remote execution: the same policy decides
  which edges qualify, the include scanner gives the inputs, and the key is
  the action digest. It doesn't depend on deps an earlier build recorded,
  so a fresh checkout's first build already hits.
- **Lookup order:** the disk cache (with `--disk_cache`), then the remote
  action cache. A hit runs the edge's local-tool steps and restores the
  outputs as a remote execution's (`ExecKind::RemoteCacheHit`); with
  `--remote_download_minimal`, they are recorded instead, as for remote
  execution, and a hit whose outputs left the CAS is a miss.
- **A miss** returns `RemoteOutcome::RunLocally`: the command runs locally
  within `-j`, and the capture stores its outputs, depfile (remote form)
  and output in the disk cache, then uploads them, with the action and its
  command, before the action cache entry pointing at them. stdout larger
  than 64 KiB goes to the CAS instead of inside the entry. Absolute paths
  into the input tree in that output (diagnostics) are rewritten relative
  to the working directory (`PathMapper::rewrite_text`), as a remote
  execution prints them: another checkout replaying a warning gets its own
  file. The disk cache stores local results the same way.
- `--noremote_upload_local_results` reads only: developer machines can
  use a cache only CI fills. Uploads also stop when the server reports
  `update_enabled = false`, with a warning.
- With `--remote_executor`, the executor's cache is used; both naming
  different services is an error.
- A cache outage follows `--remote_local_fallback`: the edge runs locally,
  nothing is stored.

## Minimal downloads

`--remote_download_outputs=minimal` (or `--remote_download_minimal`) leaves
the outputs of remote actions in the remote CAS, except:

- the outputs of `rbe_deliverable = 1` edges (executables and shared
  libraries by default), with their side outputs (the PDB);
- what the command line names, through phony targets, even when it is up
  to date;
- depfiles, which the build reads;
- what a remote action reads is never downloaded for it: a file a scanner
  finds that is a recorded output (a precompiled header) goes in by digest,
  like a declared one;
- what a local command reads: before a task runs on the host (a local edge,
  a fallback, `rbe_local`), `RemoteBackend::materialize` downloads its
  inputs that only exist remotely. Tasks download theirs at the same time;
  a file another task is downloading is waited for, not written twice
  (`Inner::materializing`, by path).

`remote_outputs::RemoteOutputs` records the others in `.ninja_remote` in the
build directory: path, digest, executable bit, a time, and the members of a
thin archive (see "Thin archives"). The build's
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
actions producing them again.

**With a disk cache**, results whose outputs stayed remote are kept too
(the downloaded ones on disk), and a later build trusts them without
asking the remote: a build after a clean then makes no remote lookups.
That is what Bazel does with action rewinding on (its disk cache only
checks that a result's blobs are present without it): an output that
proves gone is cheap to recover. Such a result is used when every output
this build downloads (deliverables, named targets, depfiles) is on disk;
with full downloads it isn't. Hashes found gone from the remote CAS during
the build (lost inputs, evicted records at its start) make the results
referencing them stale: removed, and the remote path runs, whose hits
are checked (below) and whose executions bring the outputs back. Without
that, a rewound action would find the same result again until rewinding
gave up, as Bazel found ("Bust caches for rewound actions").

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

## Daemon

`--daemon` (or `NINJA_RS_DAEMON=1`), for developers' builds: a daemon
(`crates/ninja-daemon`) keeps what builds learn and watches the files it
comes from, so that a build checks again only what changed instead of
every header and toolchain file.

- **What it keeps:** the preprocessor's lexed files, directory listings
  and compiler profiles (with their query answers), file digests, and
  toolchain input trees. A build asks it for a compile's scan, for the
  digests of an action's inputs (one request per action) and for the
  listing of a toolchain directory, from which it builds the Merkle tree
  as `InputTree::add_dir` would. The build still does everything else
  (manifest, dirty checking, execution).
- **One per user and ninja-rs binary** (its path, size and modification
  time name it), serving every build: started by the first build that
  wants it, detached (out of the terminal's process group, or the
  console), exiting after `NINJA_RS_DAEMON_IDLE` seconds without builds
  (3 hours), or once the binary it comes from is rebuilt or removed (it
  checks every few seconds, idle for ten), so that a rebuilt ninja-rs's
  daemon doesn't run beside the old one's watches. A lock file keeps it
  single. On Windows it runs a copy of the binary, so that ninja-rs can be
  rebuilt or upgraded meanwhile. `-t daemon [status|start|stop]`.
- **Talking:** length-prefixed JSON over a Unix socket in a directory only
  the user can enter (`$XDG_RUNTIME_DIR/ninja-rs`, else
  `/tmp/ninja-rs-<uid>`), or a Windows named pipe that only the user (and
  the system) may open, nor remote clients. Both ends check the other: a
  client only connects to a socket in a directory the user owns and no one
  else can enter, served by a process of the user's (`SO_PEERCRED`,
  `getpeereid`), or to a pipe whose server runs as the user; a daemon only
  accepts the user's processes. (A predictable path or name could
  otherwise be another user's first, answering with chosen digests.) A pool
  of connections lets
  concurrent actions ask in parallel. A build begins with the build
  directory, its environment (compilers are probed in the build's, and
  `INCLUDE`, `CPATH` and the like are part of a profile's key), the
  profile directory and its digests file (`.ninja_rs_digests`, to start
  from after a restart). A build that can't reach the daemon, or whose
  daemon fails, does the work itself, and says so once.
- **Watching:** the roots are the repository a checked file is in (the
  nearest directory with `.git`, `.hg`, `.jj`, `.sl` or `.svn`) and the
  toolchain directories listed whole. With inotify (a watch per
  directory), a repository is watched lazily: its root (and its `.git`, for
  cookies) at first, then the directory of each file checked, with those
  above it (their renaming moves the file), each vouched for once a sync
  followed its watch (`Watcher::epoch`); a toolchain directory, in the
  checkout or not, with everything under it, directories created there
  since included. A dotfiles repository in the home directory doesn't
  watch the home directory, nor do other build trees in a checkout get
  watched. Out of watches (ENOSPC), a root gives back all of its own (IDEs
  need them too) and is checked once per build for ten minutes.
  `ReadDirectoryChangesW` watches a root whole with one handle. Events drop the
  entries they concern: a file's, its directory's names when it's
  created, removed or renamed, and everything under a directory that
  was. Build directories are never trusted (their files change during
  builds; on Linux they aren't even watched, so that compiles writing
  objects don't flood the queue), nor the other directories a build writes
  to (outputs outside the build directory, as an in-source generator
  writes them; the build directory with symlinks resolved): their files
  are checked every time, even within a build, as a generator can rewrite a
  header between two actions. Nor are paths through symbolic links (events
  come for real paths). Roots on a file system that may not report every
  change are never watched: on Linux, any but an allow-list of local ones
  (ext4, XFS, Btrfs, tmpfs, ZFS...: not NFS, SMB, FUSE, nor WSL's view of
  Windows drives, 9p); on Windows, network drives. Files outside every
  watched root (system headers) are checked once per build, as without the
  daemon.
- **Trust:** an entry checked while its root is in sync, with no event
  meanwhile, stays valid until an event says otherwise (`WATCHED` in
  `cpp_deps::scan::Cache` and `ninja_daemon::files`); other entries are
  valid for one build. A root is in sync after the daemon created a file
  in it (a cookie, in `.git` when there is one) and saw its event, which
  comes after every change made before (Watchman's technique): before
  each build. A root found during a build is trusted from the next one.
  Lost events (an inotify queue overflow, an overflowed
  `ReadDirectoryChangesW` buffer) and broken watches put a root out of
  sync and drop everything under it; a cookie that doesn't come back
  within two seconds restarts the watch. Roots the daemon can't write to
  are trusted without a cookie.
- **Backends:** inotify on Linux (a watch per directory, lazily;
  `IN_OPEN` isn't watched, so compilers reading headers aren't events), `ReadDirectoryChangesW` per root on Windows (synchronous reads
  in a thread each; directories' own "modified" events, which follow
  their files', are ignored; short names are expanded). Not the `notify`
  crate: it watches opens on Linux and, in 8.x, misreads overflowed
  buffers on Windows. Elsewhere nothing is watched yet: every entry is
  checked once per build, which still saves lexing, probing and hashing.
- **Profiles persist** in the disk cache (`cpp-deps/`), with or without
  the daemon: a file per profile, its whole key checked when loading. The
  key has the compiler's path, size and modification time, its banner
  (`--version`, or `cl`'s: a wrapper's or a shim's file doesn't change with
  the compiler behind it), the flags, the working directory and the
  variables that change answers (`INCLUDE`, `CPATH`, `SDKROOT`,
  `DEVELOPER_DIR`, `VCToolsInstallDir`...). What it doesn't see (a clang
  configuration file's contents) is stale for a day at most: profiles are
  probed again after a day, however often used. Those unused for a month
  are removed when another is saved.

Measured on 256 compiles (36 standard headers each, a chain of project
headers), from the disk cache, 32 threads; median of five builds:

| | Windows (clang-cl, MSVC STL) | Linux (Clang, libc++) |
|---|---|---|
| no-op, without / with the daemon | 86 / 85 ms | 41 / 42 ms |
| all compiles again (clean), without / with | 2723 / 1347 ms | 992 / 869 ms |
| a header every file includes changed (cache hits) | 2706 / 1287 ms | 1011 / 850 ms |
| the daemon's first build | 2437 ms | 1073 ms |

Windows gains most: its file system calls are expensive, and the build
then neither scans nor stats the headers. Neither toolchain declared
input directories here; with them, the daemon also skips walking them.

## Later phases (designed for, not built)

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
