# Exec root and path mapping

Phase-1 CMake writes absolute paths for sources, include directories and the
compiler. A remote action must not contain machine-specific absolute paths,
so ninja-rs maps local paths into the remote input root and rewrites command
lines.

## Exec root

The *exec root* is the local directory that becomes the root of the input
tree. By default it is chosen **per action**: the deepest common ancestor
(case-insensitive on Windows) of the build directory and every input of the
action (sources, scanned headers, toolchain input directories, response
files). With `--exec_root=DIR` a fixed root is used and actions with inputs
outside it run locally.

If paths span several Windows drives there is no common ancestor; the tree
then gets one top-level directory per drive (`C/...`, `D/...`).

Each local path maps to its path relative to the exec root ("tree path").
The **remote working directory** is the tree path of the build directory.

Example: build dir `/home/u/proj/build`, source `/home/u/proj/src/a.cc`,
toolchain `/home/u/.cache/llvm/bin/clang++` → exec root `/home/u`, working
directory `proj/build`, and the action sees `../src/a.cc` and
`../../.cache/llvm/bin/clang++`.

Choosing the deepest common ancestor keeps user-specific prefixes (e.g. the
home directory) out of the action when everything lives below them, which
lets identical checkouts at the same relative layout share cache entries.

## Rewriting command lines

Every argument (and every token of a response file, and `INCLUDE` for
cl-style compilers) is scanned for embedded absolute paths:

- POSIX: a `/` at the start of the argument, after `=`, `,`, `:`, `;`, `@`
  or a quote, or right after flag letters (`-I/x`, `-isystem/x`).
- Windows: a drive letter with `:\` or `:/` at the start, after a
  non-alphanumeric character, or after flag letters (`/FoC:\x`, `-IC:/x`,
  `/external:IC:\x`).

A path ends at `;`, `,`, a quote or `=` (and `:` on POSIX): both sides of a
prefix map (`-ffile-prefix-map=OLD=NEW`, `/pathmap:`) are rewritten.

A path is rewritten to a path **relative to the working directory** when it
lies under the exec root and its first tree component exists in the input
tree (so `/dev/null` or unrelated system paths are left alone). Backslashes
are kept on Windows if the original used them. argv[0] is always replaced by
the relative path of the resolved executable (with `./` if it has no
separator, so the worker doesn't search `PATH`).

Paths keep the spelling the build uses: links and junctions are not
resolved (on Windows, only 8.3 short names are expanded). A toolchain
reached through a link inside the checkout, such as a cache linked as
`<checkout>/.hermetic-cpp`, stays inside the exec root, so two checkouts
at different paths run the same actions. `--execution_log_file` records
each action's key, command and inputs, to find what differs when they
don't.

On Windows, tree paths are case-folded: the first spelling seen of each
directory wins, so `C:\Proj\Src` and `c:\proj\src` don't become two
directories in the tree.

Outputs (`output_paths`) and the depfile are relative to the working
directory; outputs outside the build directory make the action run locally.

## Mapping paths back

Paths reported by the remote compiler (depfiles, `/showIncludes`) are mapped
back:

- relative paths are resolved against the remote working directory;
- absolute paths (the worker's own exec root, e.g. clang's resource
  directory derived from its own location) are matched against the input
  tree by suffix: the longest suffix at a `/` boundary that is a tree path;
- the resulting local path is made relative to the build directory when it
  is inside it (as ninja paths are), otherwise absolute.

So the deps log has the same shape whether an edge ran locally or remotely
(absolute paths for sources and headers outside the build tree, relative
paths for generated headers).

## Known limits

- Compilers embed the remote working directory in debug info
  (`DW_AT_comp_dir`). `-fdebug-compilation-dir=.` (clang) or
  `-ffile-prefix-map` avoids non-deterministic outputs. cargo-rbe's
  `rbe-exec` helper (substitutes an `@EXECROOT@` token on the worker and
  scrubs it from outputs) is the fallback for tools that insist on absolute
  paths; it is not needed for clang/gcc with relative paths.
- Windows workers inherit `MAX_PATH` limits on the working directory: with
  the exec root at a drive root, deep build directories plus the worker's own
  prefix can exceed 260 characters (seen with the fake server; use short
  worker roots).
- Symlinked inputs are sent as regular files (the input tree follows links).
