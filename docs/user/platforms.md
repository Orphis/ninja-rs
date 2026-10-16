# Platforms

ninja-rs runs on Windows, Linux (WSL included) and macOS, and behaves the same on each where it
can. This page covers where they differ: paths, how commands run, environments, the settings
files' trust rules, case, and the daemon. Windows and Linux are built and tested; macOS isn't.

## Windows

### Long paths

`ninja-rs.exe` declares itself long-path aware in its application manifest, as ninja does: once
long paths are enabled on the machine (the `LongPathsEnabled` system setting, which an
administrator turns on), ninja-rs reads and writes paths longer than 260 characters. Two limits
remain, which ninja has too:

- **A command's working directory** must be shorter than 260 characters: Windows refuses to start
  a process in a deeper directory, manifest or not.
- **Tools that aren't long-path aware** fail on long paths, whatever ninja-rs does. MSVC's
  `link.exe` is one (see [known limitations](#known-limitations)).

Keep build directories short (`C:\src\app\build` rather than a deep path under your profile).
hermetic-llvm-cmake's toolchain cache, inside the checkout (see [CMake](cmake.md)), makes this
more pressing.

### Commands

- **Local commands** are given to `CreateProcess` as the build file writes them, as ninja does:
  no shell, unless the command runs one. CMake wraps commands with several steps in
  `cmd.exe /C "..."`.
- **Programs** are found as Windows finds them: relative to the build directory or through
  `PATH`, with `.exe` added. 8.3 short names (`PROGRA~1`) are expanded to the long ones, and the
  toolchains file is matched ignoring case.
- **Remote actions** take apart `cmd.exe /C "..."` and `&&` chains, but refuse what needs
  cmd.exe: redirections and pipes outside quotes, `&`, `^`, parentheses, and `%NAME%` when `NAME`
  is set (or is one of cmd's own, such as `%CD%`). A `%NAME%` that isn't set stays as written,
  as cmd.exe leaves it: `/pdbaltpath:%_PDB%` reaches the linker. A command with several remote
  steps runs locally, since cmd.exe's quoting doesn't survive a worker's own. See
  [remote execution](remote-execution.md#commands-that-need-a-shell).
- **Encoding.** ninja-rs reads build files as UTF-8 and gives commands and paths to Windows as
  UTF-16, on every code page. `-t wincodepage` says so, and CMake then writes UTF-8 build files
  (it writes them in the ANSI code page otherwise, mangling non-ASCII paths).

### Jobs and interruption

Each command runs in a job object it joins before it starts, in a console process group of its
own: Ctrl-C and Ctrl-Break reach ninja-rs only. To stop a command (an interrupt, a failed build),
ninja-rs sends Ctrl-Break to its process group (without a console, it terminates the job), waits
for it, and kills what's left 2 s after it exited. `console` pool commands stay in the
foreground and get the console's Ctrl-C themselves.

- A process started with `CREATE_BREAKAWAY_FROM_JOB` (a compiler server, a build daemon) leaves
  the job, as the job allows: it isn't stopped, like a process leaving its group on POSIX.
- A command that can't join its job doesn't run: it fails with
  `putting the command in a job: ...`.
- A command ending with Windows' Ctrl-C status (`STATUS_CONTROL_C_EXIT`) counts as an
  interruption, not a failure.

### MSVC environments

A developer prompt (`vcvars64.bat`, `VsDevCmd.bat`) sets variables the MSVC tools read:
`INCLUDE`, `LIB`, `LIBPATH`, `CL`, `LINK`... A command run locally inherits them; a remote
action only has what ninja-rs gives it. So ninja-rs carries, into each action's environment and
key, the variables its tools read, with their paths rewritten like arguments (see
[caching](caching.md#what-the-key-covers)):

| Tool | Variables carried |
|---|---|
| cl.exe, clang-cl compiling | `INCLUDE` and `EXTERNAL_INCLUDE` (unless `/X`; for clang-cl, unless `/vctoolsdir` or `/winsysroot`), `CL` and `_CL_`, the variable `/external:env:NAME` names, `LIBPATH` for cl's `/clr`; for clang-cl, `VCToolsInstallDir` and `VCINSTALLDIR` (unless the toolchain is named) |
| Clang targeting MSVC | The same include and Visual Studio variables, unless `-nostdinc` or `-Xmicrosoft-*` name the toolchain |
| link.exe, lld-link, a driver linking | `LIB`, `LINK` and `_LINK_` (lld-link with `/lldignoreenv` reads none) |
| lib.exe, llvm-lib | `LIB`, only when an input isn't found from the working directory or `/LIBPATH:` |

- The Windows SDK's variables (`WindowsSdkDir`, `UCRTVersion`) are never carried: the tools find
  the SDK from their command line or the registry.
- `CL`, `_CL_`, `LINK` and `_LINK_` add options to every command line: ninja-rs reads them as part
  of it, for the include scanner and the key.
- A hermetic toolchain naming its toolset and SDK on the command line (hermetic-llvm-cmake's
  `/vctoolsdir`, `/winsysroot`) gets the same keys from a plain shell and from a developer
  prompt.
- cl.exe's `/Zi` writes a PDB several compiles share, which no action can return: those
  compiles run locally. Embed the debug information instead (`/Z7`; with CMake,
  `-DCMAKE_MSVC_DEBUG_INFORMATION_FORMAT=Embedded`). clang-cl puts it in the object anyway.

How cl.exe's preprocessor mode (`/Zc:preprocessor`) affects include scanning is in
[include scanning](include-scanning.md).

### LIB and link analysis

An MSVC link reads libraries it looks for: the inputs its command line names, the
`/defaultlib:` libraries, and those the objects and libraries ask for in their directives (the
`.drectve` section: `/DEFAULTLIB:MSVCRT`, `kernel32.lib`...). It looks in the working directory,
then the `/LIBPATH:` directories, then `LIB`'s. A developer prompt's `LIB` names hundreds of
megabytes of libraries; a link reads a few.

So ninja-rs follows the linker's search: it reads the directives of each object and library,
transitively, finds each library as the linker would, and sends the files found in `LIB`'s
directories, not the directories. When it can't tell what the link reads (an object it can't
read, such as LTO bitcode or an output left in the remote CAS, or a library it can't find), it
sends `LIB`'s directories whole. `/LIBPATH:` directories outside the build directory are sent
whole. A link through a compiler driver (clang-cl without `/c`) is read from the driver's dry
run (`-###`), which names the linker and its arguments.

### Drives, subst and shares

- **Spellings are kept.** Paths keep the spelling the build uses: a `subst` drive, a junction or
  a symbolic link isn't resolved (only 8.3 short names are expanded). A toolchain reached through
  a junction inside the checkout stays inside the exec root.
- **Several drives.** An action whose files span drives (sources on `D:`, the toolchain on `C:`;
  a `subst` drive `S:` and `C:`) gets one top-level directory per drive in its input tree (`C/`,
  `D/`), and UNC paths one per share (`UNC/server/share`). Its local runs name the other drives'
  files by their host paths. Such keys hold the drive letters: only machines with the same
  layout share them. Keep the sources, the build directory and the toolchain on one drive.
- **Shares.** A build directory on a share (`\\server\share\...`) works.

### Case

Windows compares names ignoring case, and so does ninja-rs: the toolchains file, the exec root,
and the input tree (where the first spelling seen of a directory wins, so `C:\Proj\Src` and
`c:\proj\src` don't become two directories). VFS overlays are read case-insensitively. The
include scanners and the daemon compare names with ASCII letters folded.

### Settings files

A workspace `.ninjars` is skipped, with a warning, when others could have changed it. On Windows,
directories made outside your profile usually inherit `Authenticated Users: Modify` from the
drive's root (`C:\src`, `D:\work`): a `.ninjars` there is skipped until that entry goes, for
example with `icacls D:\work /inheritance:d` then `icacls D:\work /remove:g *S-1-5-11`. See
[settings](settings.md).

### The daemon

The daemon (see [the daemon](daemon.md)) watches with `ReadDirectoryChangesW`, talks over a named
pipe, and runs a copy of the ninja-rs binary, so that the binary can be rebuilt or replaced
while it runs. Its files are in `%LOCALAPPDATA%\ninja-rs\daemon`. A watched directory named by
an 8.3 short name is refused.

## WSL

Under WSL, ninja-rs is a Linux program (everything in [Linux](#linux) applies), with two kinds of
file systems: Linux's own (`~`, ext4) and the Windows drives (`/mnt/c`).

### Settings files on Windows drives

Windows drives are mounted without the `metadata` option by default. Their files then have no
Unix permissions: they all show as yours, writable by everyone, and Windows' own permissions,
which decide who may change them, can't be read from WSL. So a workspace or system `.ninjars` on
a Windows drive is only trusted in your Windows profile on the drive Windows is on
(`/mnt/c/Users/<you>`), which Windows keeps yours; elsewhere it is skipped with a warning
explaining why.

- **Your profile** is found by your Linux user name, or the last part of `USERPROFILE` when
  `WSLENV` passes it.
- **The drive Windows is on** is `SystemDrive`'s (when `WSLENV` passes it), else the drive of the
  `Windows\System32` directory on `PATH`, else `C:`.
- Other drives (`D:`) and mounts of a single directory (Docker's) aren't trusted.
- A drive mounted with `metadata` keeps Unix permissions, and the usual checks apply.

To use a `.ninjars` elsewhere: keep the workspace in your profile, mount the drive with
`metadata`, or give the file with `--ninjars=FILE` (which isn't checked). See
[settings](settings.md).

### Case-insensitive directories

Directories Windows creates on its drives are case-insensitive, also seen from WSL. ninja-rs on
Linux compares names exactly: an `#include` spelled with another case than the file isn't found
by the include scanners. The preprocessor scanner then takes the compiler's list (right, but a
compiler run per compile); the conservative scanner may send the wrong file (see
[known limitations](#known-limitations)). Spell includes as the files are named, or build in
WSL's own file system.

### Speed and the daemon

Files on Windows drives are much slower to reach from WSL than WSL's own: build there when you
can (`~/src`). The daemon doesn't watch Windows drives, whose changes made from Windows it
wouldn't see: it checks what it knows of them once per build.

Use separate build directories for Windows and WSL builds of one checkout.

## Linux

- **Commands** run through `/bin/sh -c`, as in ninja, or directly when they have no shell syntax
  (see [getting started](getting-started.md#differences-from-ninja)). Each runs in a process
  group of its own, which a stop signals.
- **Signals.** SIGINT, SIGTERM and SIGHUP (a terminal closed) interrupt the build, which exits
  with 130.
- **In a container**, as PID 1 (or a subreaper), ninja-rs reaps the orphaned processes commands
  leave, so that they don't pile up as zombies.
- **The jobserver.** ninja-rs takes tokens from GNU make's FIFO (`--jobserver-auth=fifo:PATH`,
  make 4.4) and pipe (`--jobserver-auth=R,W`, make 4.3) pools, and creates FIFO pools.
- **Open files.** ninja-rs doesn't raise its open-file limit. A build with many jobs reading large
  toolchains may run out (`Too many open files`): raise it (`ulimit -n 65536`). The daemon raises
  its own, up to the hard limit (at most 65,536).
- **Locks on NFS.** File locks on NFS are POSIX locks, which a read-only file can't take: ninja-rs
  opens lock files read-write first. Builds in different PID namespaces (containers) sharing a
  build directory tell each other's lock holders apart.
- **The daemon** watches with inotify. Its watches count against `fs.inotify.max_user_watches`
  and its instances against `fs.inotify.max_user_instances`: an error naming them means raising
  them (`sysctl`). It only watches local file systems (ext4, XFS, Btrfs, tmpfs, F2FS, ZFS,
  bcachefs, overlayfs and a few others); on others (NFS, SMB, WSL's Windows drives) it checks
  once per build. Files in a case-folding directory (`chattr +F`) are checked at each build,
  never taken on trust. Its files are in
  `$XDG_RUNTIME_DIR/ninja-rs`, or `/tmp/ninja-rs-<uid>`.

## macOS

The project doesn't build or test ninja-rs on macOS. The code has macOS paths, which have never
run:

- the lock holder's process identity (`proc_pidinfo`);
- the daemon assumes case-insensitive volumes (it folds ASCII case), while the include scanners
  running in the build compare names exactly, as on Linux;
- Apple's variables are carried into actions: `SDKROOT`, `DEVELOPER_DIR`, the deployment targets
  (`MACOSX_DEPLOYMENT_TARGET`...), `ZERO_AR_DATE`.

The daemon can't watch files there (there is no macOS backend), so it saves little. macOS's
default open-file limit (256) is low for a build with many jobs: raise it (`ulimit -n`).

## Known limitations

- **MSVC links fail from deep build directories** when they run locally (a cache miss, a
  fallback): `LNK1181: cannot open input file 'kernel32.lib'`. The local run uses relative paths
  (`..\..\..`) into the toolchain, which push `link.exe`, not long-path aware, past 260
  characters. Use a shorter build directory path.
- **ASan links on Windows workers fail.** Objects built with cl's `/fsanitize=address` make the
  linker add the ASan runtime libraries from `LIB`, which ninja-rs doesn't send: `LNK1104`.
  Link those targets locally (`--remote_mnemonics=Compile,Archive`).
- **Libraries named by directives, found in the working directory or by an absolute path**, aren't
  sent with a remote MSVC link, which then fails. Make them declared inputs of the link (link the
  library explicitly), or link locally.
- **Case-sensitive directories on case-insensitive systems** (Windows' per-directory case
  sensitivity, WSL directories with `case=dir`, case-sensitive APFS volumes): the daemon folds
  case, so `Foo.h` and `foo.h` in one directory can be taken for each other, and an action sent
  with the other's contents. Don't use `--daemon` on such trees.
- **8.3 names under a watched directory**: a header named by its short name (`AVERYL~1.H`) can be
  stale with the daemon. Name files by their long names.
- **The daemon's cookie files.** On Windows, a watched directory that isn't a git checkout gets
  short-lived `.ninja-rs-cookie-*` files at its top while builds run commands: they may show in
  `git status` or a glob for a moment.
- **Case-mismatched includes on case-insensitive file systems** (WSL's Windows drives, macOS,
  ext4 case folding), with the conservative scanner: the scan may send a later `Foo.h` for
  `#include <foo.h>`, or nothing. Use the default preprocessor scanner, and spell includes as the
  files are named.
