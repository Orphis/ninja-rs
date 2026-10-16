# Files

Every file ninja-rs reads or writes besides the build's own inputs and outputs: its state in
the build directory, the settings files, the daemon's directory, the disk cache, compiler
profiles, the execution log and the build event file, with their formats. Options are in
[options.md](options.md), environment variables in [environment.md](environment.md).

## Where they are

```text
<build directory>/              the directory ninja-rs runs in (-C)
├── build.ninja                 the manifest (-f), its includes and subninjas
├── .ninja_rs_rsp/              private copies of response files (remote builds)
└── <$builddir, if set>/        else the build directory itself
    ├── .ninja_rs_log           build log
    ├── .ninja_rs_deps          deps log
    ├── .ninja_rs_lock          lock of the logs
    ├── .ninja_remote           outputs left in the remote CAS
    ├── .ninja_rs_digests       file digest cache
    └── .ninja_lock             ninja's: the file system's time, during a build

~/.ninjars, <workspace>/.ninjars, /etc/ninjars    settings files
<daemon directory>/             the daemon, relays, nested invocations' secret options
<--disk_cache>/                 action cache, CAS, compiler profiles
<--execution_log_file>          JSON lines, one per action
<--build_event_binary_file>     build events
```

ninja's own `.ninja_log` and `.ninja_deps` are neither read nor written: ninja and ninja-rs can
take turns in one build directory without undoing each other's records (each rebuilds what the
other built, the first time).

## Build directory

### `.ninja_rs_log`: build log

Per output: the hash of the command that last built it, and its recorded modification time.
Text, one record per line, tab-separated, after a signature line:

```text
# ninja-rs log v2
<start ms>\t<end ms>\t<mtime ns>\t<output>\t<command hash>
```

| Field | Meaning |
|---|---|
| `start ms`, `end ms` | When the command started and ended, in milliseconds since its build started. |
| `mtime ns` | The time recorded for the output, in nanoseconds since the Unix epoch, as ninja records it: the file system's time when the command started (read from `.ninja_lock`); for `restat` and generator edges, the newest output's modification time. |
| `output` | The output's path, as the manifest names it. |
| `command hash` | Hex rapidhash of the command (with the response file's content), as ninja 1.13's `.ninja_log` v7 computes it. |

The last record of an output wins. A log without the signature (an older version, v1, which
hashed commands with FNV-1a) is ignored with the warning `build log version is too old or
invalid; starting over`. The build holding the lock rewrites it (keeping the records of the
manifest's outputs and of files that exist) when it holds more than 100 records and over 3
times as many as outputs, through `.ninja_rs_log.recompact` renamed into place. Records are
written once per turn of the build loop, in one write.

### `.ninja_rs_deps`: deps log

The dependencies `deps = gcc` and `deps = msvc` edges discovered. Text: each path is written
once with an id, and records refer to paths by id, which keeps the file small when thousands of
objects share headers.

```text
# ninja-rs deps v3
p\t<id>\t<path>
d\t<output id>\t<mtime ns>\t<id> <id> ...
```

A `p` line defines the next id (0, 1, ...), just before the first `d` line using it. A `d` line
gives an output's dependencies, in order, and the output's time when they were recorded. The
last `d` line of an output wins. Reading stops at the first line that doesn't fit (an id out of
sequence, a path seen before, a malformed line): what follows is cut off before the next
append, with the warning `premature end of file; recovering`, and those outputs build again. A
log without the signature (v2 had positional path records) is ignored, with the warning `deps
log version is too old or invalid; starting over`.

Builds the lock holder runs (see below) append at once: each locks the file itself while it
reads or appends, and first reads what the others appended, so that its new paths get the next
ids. The build holding the directory's lock rewrites the file from what it knows when it can't
follow what was appended, and recompacts it, through `.ninja_rs_deps.recompact`, when it holds
more than 1000 records and over 3 times as many as outputs.

### Torn lines

A write cut short (a crash, a kill, a full disk) leaves a last line without its newline, which
may still parse: a hash, an id or a path cut short. Readers leave it out, and writers make sure
the next record starts on a line of its own.

| File | Reader | Build holding the lock | Build appending under it |
|---|---|---|---|
| `.ninja_rs_log` | complete lines only | cuts the torn line off | ends it with `\n` (it then reads as a malformed line, or a record whose hash matches no command) |
| `.ninja_rs_deps` | complete lines; skips lines ending with NUL | cuts the torn line off | ends it with `\0\n` |
| `.ninja_remote` | complete lines; skips lines ending with NUL, and a `+` or `=` line's record with them | ends it with `\0\n` | ends it with `\0\n` |

No path or number holds a NUL, so a line ending with one is never a record. A file that
changed since it was read (written by a process not taking the lock) is rewritten from what was
read rather than cut.

### `.ninja_rs_lock`: lock of the logs

A build takes an exclusive OS lock on this file before reading the logs, and holds it until it
ends: two builds in one directory (an IDE's and a terminal's) would otherwise give their new
paths the same ids in the deps log. A second build waits, saying `waiting for the build running
in this directory (pid 1234, started 5 s ago) to finish...`. ninja has no such lock.

The lock is `flock` on Unix, and on Windows a lock on a byte far past any data, so that reads
and writes of the file through other handles still work. It goes when its process ends,
however it ends. While held, the file says who holds it, one line:

```text
<pid> <since ms> <start> <namespace>
```

| Field | Meaning |
|---|---|
| `pid` | The holder's process id. |
| `since ms` | When it took the lock, in milliseconds since the Unix epoch. |
| `start` | When the process started: on Linux in clock ticks since boot, on macOS in microseconds since the epoch, on Windows in 100 ns units since 1601. |
| `namespace` | Linux: the inode of its pid namespace (0 when unreadable); 0 elsewhere. |

The last two tell the holder from another process with the same id (one started since, or in
another container sharing the directory); an older ninja-rs wrote the first two only. The
holder empties the file when it releases the lock.

A build passes the lock's identity to its commands (`NINJA_RS_LOGS_LOCK`, see
[environment.md](environment.md#variables-ninja-rs-sets)): an invocation they run that writes
the same logs (a generator's `-t restat`, a build of another configuration in the same
directory) shares the lock instead of waiting for it, and only appends: it neither recompacts
nor cuts torn lines. One whose environment lost the variable (a wrapper scrubbing it) still
recognizes the holder among its ancestor processes by the fields above. On Unix, the file is
opened read-only when another user owns it (a directory a group shares); on a file system
without locks, the build warns and goes on unlocked.

### `.ninja_remote`: outputs left in the remote CAS

With `--remote_download_outputs=minimal`, the outputs of remote actions that weren't
downloaded: the build treats them as present, with their recorded times, and downloads one
when a local command or the build itself needs it. A journal of changes, appended as they are
made, after a version line:

```text
# ninja-rs remote outputs v3
<mtime ns> <hash> <size> <x|-> <path>
+ <member>
= <owner path>
- <path>
```

| Line | Meaning |
|---|---|
| `<mtime ns> <hash> <size> <x\|-> <path>` | An output recorded (replacing an earlier record of the path): its time, its SHA-256 digest and size, executable or not. |
| `+ <member>` | After a thin archive's record: a member, as the archive names it (actions reading the archive get its members by digest). |
| `= <owner path>` | After a side output's record (a compile's split DWARF, a link's PDB): the output of its action, whose record it goes with. |
| `- <path>` | The record of `path` dropped: the output was written locally, or cleaned. |

Versions 1 and 2, without `-` lines, are still read. The build holding the lock rewrites the
file with the records alone, through `.ninja_remote.<pid>.tmp`, when it has more than twice as
many lines as records plus 100; and removes it when none is left, or when the build has no
remote service (none, a disk cache alone, or unreachable): those outputs build again locally.
Builds it runs append to it meanwhile, each change in one write.

### `.ninja_rs_digests`: file digest cache

The digests of the files remote actions and build events used (sources, headers, toolchain
files, artifacts), so that the next build hashes only what changed. Saved at the end of builds
with a remote service, a disk cache or build events, whole, through `.ninja_rs_digests.tmp`
renamed into place; the daemon reads it too.

```text
# ninja-rs digests v3
<path>\t<size>\t<mtime ns>\t<change>\t<created>\t<volume>\t<id>\t<sha256>
```

| Field | Meaning |
|---|---|
| `path` | Absolute path. Paths holding a tab or newline aren't saved. |
| `size`, `mtime ns` | The file's size and modification time. |
| `change` | Its change time: Unix in nanoseconds, Windows in 100 ns units. Tools restoring modification times don't set it back. |
| `created` | Windows: its creation time in 100 ns units; 0 elsewhere. |
| `volume`, `id` | Windows: the volume serial number and file ID; Unix: 0 and the inode. A file created again under its name gets a new one. |
| `sha256` | The digest, hex. |

A digest is used only while every field still matches. Files modified within their file
system's time granularity of being hashed (2 s for whole-second times, 100 ms otherwise) aren't
saved: changed again in that window, they could keep the same stamp.

### `.ninja_lock`

As in ninja: written at each command's start to learn the file system's current time (which a
network file system may not share with the host), removed at the end of the build.

### `.ninja_rs_rsp/`

In the build directory (not `$builddir`): private copies of response files that local runs of
remote actions read, rewritten as the remote action reads them (a checked-in `@flags.rsp`, or
one several commands share), named by their contents' digest, `<32 hex digits>.rsp`. The
original is never modified. Copies written more than 24 hours ago, and temporary files left by
interrupted writes, are removed when a build with a remote service or a disk cache starts; one
needed again is written again. An edge's own response file (`rspfile`) is rewritten in place
instead.

### Temporary files next to outputs

Downloaded outputs are written through a file next to them, the output's name followed by
`.ninja-rs-tmp-<pid>-<n>-<random>`, renamed into place. Files of this form an hour old (left
by a build that was killed) are removed when a build next writes to their directory. A file
`.ninja-rs-tmp-...` is also made and removed at once in a directory to read its file system's
clock.

### Files the manifest names

ninja-rs reads the manifest and its `include` and `subninja` files, depfiles (removed once read
into the deps log, unless `-d keepdepfile`), dyndep files, and the RBE interface's
`rbe_toolchains` and `rbe_targets` JSON files (see [rbe-interface.md](../rbe-interface.md)). It
writes response files from `rspfile_content` before a command runs and removes them after it
succeeds, unless `-d keeprsp`.

## Settings files

Read before the command line, in this order; see [settings.md](../user/settings.md).

| File | Where |
|---|---|
| The system's | `/etc/ninjars`; on Windows `%ProgramData%\ninjars`. Skipped, with a warning, when someone other than you and the administrators could have written it. |
| The workspace's | The first `.ninjars` from the build directory (`-C`) up to the repository's root (a directory holding `.git`, `.hg`, `.jj`, `.sl` or `.svn`), short of the home directory and of a file system's root; its directory is `%workspace%`. Skipped, with a warning, when others could have written it or put it there. |
| The user's | `~/.ninjars`. |
| Given | Each `--ninjars=FILE`, in order; `/dev/null` (or `NUL` on Windows) ends the list. |
| Imported | `import FILE` and `try-import FILE` lines, relative to the importing file; Bazel's files (named `*bazelrc`) and what they import are read as Bazel reads them. |

A file found twice (the workspace's is the user's) is read once. Nested invocations read again
the files the outermost one read, for their own `--config` options.

## Daemon directory

Where the daemon of each ninja-rs binary lives (see [daemon.md](../user/daemon.md)), and where
invocations put their build event relays and the options they keep out of the environment:

| System | Default |
|---|---|
| Linux, macOS | `$XDG_RUNTIME_DIR/ninja-rs`, else `$TMPDIR/ninja-rs-<uid>` (`/tmp/ninja-rs-<uid>`) |
| Windows | `%LOCALAPPDATA%\ninja-rs\daemon` |

`NINJA_RS_DAEMON_DIR` replaces it. It must be yours alone: on Unix mode `0700` and owned by
you, on Windows created with access for you and SYSTEM only (an existing one may let the
administrators in, no one else). An existing directory that isn't private is refused, and the
build goes on without the daemon. Its paths are predictable, so another user could make it
first, with a socket of theirs in it; and the daemon's answers go into action keys.

```text
<daemon directory>/
├── <id>.lock          held by the running daemon (empty): one daemon per binary
├── <id>.log           the daemon's log
├── <id>.log.1         the log before, once the log reached 1 MiB
├── <id>.failed        why the daemon last failed to start
├── <id>.cookies/      cookies of watched roots without a repository directory (Linux)
├── <id>.sock          Unix: the daemon's socket
├── <id>.exe           Windows: the copy of ninja-rs the daemon runs
├── <id>.<pid>.tmp     Windows: that copy, being made
├── relay-<pid>.sock   Unix: an outermost invocation's build event relay
└── flags-<pid>.json   an invocation's options nested ones read, kept out of the environment
```

- **`<id>`**: 16 hex digits, a digest of the ninja-rs binary's identity (its volume and file
  ID, or device and inode), size and modification time. Every spelling of the binary's path
  (case, links, junctions, a `subst` drive) is the same daemon; a rebuilt binary gets another,
  and the old daemon exits soon after.
- **`<id>.log`**: lines `<unix seconds> [<pid>] <message>`: starts, failures (with the reason a
  start failed), panics, exits. When a line would take it past 1 MiB, it moves to `<id>.log.1`
  (replacing the one there) and a new log starts.
- **`<id>.failed`**: the reason; its modification time is when. For 5 minutes after it, builds
  don't start the daemon again (each would wait for it in vain), and say so; `-t daemon start`
  removes it, and so does a successful start.
- **Socket**: when the path would be 100 bytes or longer (a socket path's limit), the socket is
  `/tmp/ninja-rs-<uid>/<hash>-<id>.sock` instead, `<hash>` being 16 hex digits of the
  directory's digest. On Windows the daemon listens on the named pipe
  `\\.\pipe\ninja-rs-<hash>-<id>` (and a relay on `\\.\pipe\ninja-rs-<hash>-relay-<pid>`),
  which only you may open and remote clients can't.
- **`<id>.exe`** (Windows): a running program can't be replaced there, so the daemon runs a
  copy, and ninja-rs can be rebuilt or upgraded while it runs.
- **Clean-up**: the files of other binaries' daemons (`.exe`, `.lock`, `.log`, `.log.1`,
  `.sock`, `.cookies`, `.failed`) are removed when they don't run: by a daemon starting on Unix,
  by a build copying the binary on Windows. Copies left half made go after 10 minutes.
- **`flags-<pid>.json`**: a JSON array of the options whose values may be credentials
  (`--remote_header`), written readable by you alone, removed when its invocation exits; files
  of processes gone (killed) are removed when another invocation writes its own. See
  [environment.md](environment.md#ninja_rs_parent).
- **`relay-<pid>.sock`** (Unix): nested invocations connect to it and send their build events,
  as frames of a little-endian `u32` length and the bytes: first the nested build's directory,
  then each `BuildEvent` message, then an empty frame. Removed when the invocation ends.

The daemon's protocol is the same framing, each message JSON.

The daemon also makes cookie files, `.ninja-rs-cookie-<pid>-<n>`, for an instant in the roots
it watches: in a repository's own directory (`.git` and the like) when there is one, else in
`<id>.cookies/` (Linux) or at the root's top (a Windows worktree or submodule, whose `.git` is
a file; a toolchain directory). Tree walks skip them.

## Disk cache

`--disk_cache=DIR`: an action cache and a CAS with the keys of a REAPI service (action
digests, blob digests, SHA-256) and entries in the same form, so that they are interchangeable
with a remote cache's; the layout is Bazel's, with additions.

```text
<disk cache>/
├── VERSION                format version: "1"
├── size                   estimated bytes in cas/, chunked/ and ac/, decimal
├── ac/<hh>/<hash>         ActionResult messages, by action digest
├── cas/<hh>/<hash>        blobs and chunks, by digest
├── chunked/<hh>/<hash>    SplitBlobResponse messages: a large blob's chunks, in order
├── tmp/                   files being written, renamed into place when done
├── gc/lock                held while a process collects garbage
└── cpp-deps/              compiler profiles: see below
```

`<hash>` is the digest's 64 hex digits, `<hh>` its first two. Messages are REAPI protobuf.

- **Chunks**: a blob larger than 2 MiB is stored as its content-defined chunks (FastCDC 2020
  with REAPI's recommended parameters, 512 KiB on average), each a blob of `cas/`, plus its
  layout in `chunked/`. Two builds of an executable then share most chunks. A blob from a
  remote cache that splits blobs keeps the remote's chunks. A blob is present when stored whole,
  or when its layout and all its chunks are.
- **Eviction**: least recently used, as in Bazel's disk cache: reads update a file's
  modification time; after a build that wrote to the cache, once `size` passes
  `--disk_cache_max_size`, one process (`gc/lock`) walks the cache, removes the oldest files down
  to 90% of the limit and writes the real size back. Using a chunked blob marks its layout and
  chunks. Files in `tmp/` an hour old go too.
- **`VERSION`**: a directory without it (removed by a cleaner of old files) is taken as ninja-rs's
  again when it has `size`, or nothing stored in `ac/` and `cas/`, and `VERSION` is written back.
  One with Bazel's layout and entries is Bazel's `--disk_cache` (a shared `.bazelrc` names it):
  used as it is, without writing `VERSION` there nor collecting its garbage (Bazel does), with a
  note once per invocation tree. Any other directory that isn't empty (a mistyped path) isn't
  used: a warning, and the build goes on without it. Another format version is refused the same
  way.

Writers in several processes (nested builds, parallel builds, containers sharing the volume)
name their temporary files with the process id, a counter and a random part.

## Compiler profiles

What the preprocessor scanner learns by probing a compiler (its predefined macros, built-in
include directories, dialect, answers to `__has_include`-like queries), saved in the disk
cache's `cpp-deps/` directory, so that later builds don't probe again. Without a disk cache they
live in memory only (or in the daemon).

```text
<disk cache>/cpp-deps/
├── <32 hex digits>.profile    one per compiler and set of flags that matter
└── cleaned                    its modification time is the last clean-up's
```

A profile is text: the signature `cpp-deps profile 5`, then `key`, `probed` (Unix seconds),
`dialect`, `trigraphs`, `va_comma`, `operator`, `dir` and `framework`, `implicit`, `cwd` and
`answer` lines, tab-separated, then a `predefines` line followed by the predefined macros as
`#define` lines. The key (the compiler's path, size and modification time, its version banner,
the flags that matter, its configuration files' digest, the variables that matter: see
[environment.md](environment.md#the-builds-environment-and-remote-actions)) is checked when
loading. A profile is probed again after 24 hours, however often it is used. Profiles unused for
30 days are removed when another is saved, at most once a day. Writes go through
`<name>.tmp<pid>-<n>` renamed into place.

## Execution log

`--execution_log_file=FILE`: a JSON object per line, for each action prepared for the remote
service or the disk cache (whether it then hits a cache, runs remotely or runs locally). The
file is emptied when the invocation (or `-t exec`) starts; nested invocations append to it,
each line in one write.

```json
{"outputs":["obj/a.o"],"mnemonic":"CXXCompile","target":"//src:app","action":"<hash>/<size>","arguments":["../../llvm/bin/clang++","-c","../src/a.cc","-o","obj/a.o"],"working_directory":"proj/build","environment":{"SDKROOT":"..."},"platform":{"OSFamily":"Linux"},"input_root":"<hash>/<size>","inputs":{"proj/src/a.cc":"<hash>/<size>","llvm":"tree:<hash>/<size>"}}
```

| Field | Meaning |
|---|---|
| `outputs` | The edge's outputs. |
| `mnemonic`, `target` | Its `rbe_mnemonic`, and its target's label from the targets file. |
| `action`, `input_root` | The action's digest (its cache key) and its input root's, as `hash/size`. |
| `arguments`, `working_directory`, `environment`, `platform` | The REAPI `Command`, with paths as the worker sees them (see [paths.md](../paths.md)). |
| `inputs` | Every input, by path in the input root: `hash/size` for a file (followed by ` x` when executable), `tree:hash/size` for a toolchain directory sent whole. |

Comparing two logs' lines for the same outputs shows why their keys differ.

## Build event file

`--build_event_binary_file=PATH`: the invocation's `BuildEvent` messages, each preceded by its
varint length, as Bazel writes its file. Created at the start; a nested invocation's events go
to the outermost invocation's file (through its relay), not to one of its own, unless it is
given its own option.

## Other files

| File | Read or written for |
|---|---|
| `--tls_certificate`, `--tls_client_certificate`, `--tls_client_key` | Read (PEM) when connecting to a TLS endpoint. |
| A toolchain's `inputs` (from the toolchains file) | Read and sent to remote workers. |
| `$TMPDIR/NinjaFIFO<pid>` (Unix) | `--jobserver-pool`'s FIFO, mode `0600`, removed at the end. |
| `/proc/self/mountinfo` (Linux) | Which file systems are Windows drives under WSL (settings files' trust rules), and mounts under the daemon's roots. |
| `/proc/sys/kernel/hostname` | The host name build events report, without `HOSTNAME`. |
