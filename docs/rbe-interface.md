# RBE interface for ninja files (v1)

This is the contract between a build system generator that writes
`build.ninja` files and an executor that runs their edges with remote build
execution (REAPI v2) and reports them in build event streams, such as
`ninja-rs`. It is not specific to one generator: CMake's Ninja generators
write it with `-DCMAKE_NINJA_RBE=ON`, and any other generator (or a
hand-written file) can. Design doc: "CMake Ninja RBE Mode: Design"
(https://claude.ai/code/artifact/f5fd7431-9c04-4cf4-88ce-8c7655c0fd4e).

Everything here is plain ninja syntax: stock ninja ignores it and still builds
the project locally. All names start with `rbe_`, so that they don't collide
with a generator's own variables or future ninja bindings.

## Activation

At file scope:

```ninja
rbe_version = 1
rbe_toolchains = CMakeFiles/rbe_toolchains.json
rbe_targets = CMakeFiles/rbe_targets.json
```

- `rbe_version`: the version of this interface the file follows. **Without it
  the executor runs every edge locally.** An executor that does not implement
  the version runs every edge locally too (and should say so).
- `rbe_toolchains` (optional): the toolchains file (see below).
- `rbe_targets` (optional): the targets file (see below).

Paths are relative to the build directory (the directory holding
`build.ninja`). The variables must be visible from every edge's scope (CMake's
Ninja Multi-Config generator writes them in each `build-<Config>.ninja` and in
`build.ninja`).

## Edge bindings

Bindings are written on `build` edges only (ninja rejects unknown variables on
`rule` blocks). All are optional.

| Binding           | Values           | Meaning |
|-------------------|------------------|---------|
| `rbe_target`      | target name      | The target owning the edge (see the targets file). |
| `rbe_mnemonic`    | free-form        | Action kind, such as `CXXCompile`; used for REAPI `action_mnemonic`, build events and the executor's policy. |
| `rbe_local`       | `1` / absent     | The edge must run locally as a whole. Unmarked edges may run remotely. |
| `rbe_deliverable` | `1` / absent     | The edge's explicit outputs are final artifacts (downloaded when only top-level outputs are). |

Edges with `generator = 1` (manifest regeneration) and edges in the `console`
pool always run locally.

Example:

```ninja
build src/foo/CMakeFiles/foo.dir/a.cpp.obj: CXX_COMPILER__foo_Release ../src/foo/a.cpp
  rbe_target = foo
  rbe_mnemonic = CXXCompile
```

## Commands and local tools

The executor runs an edge's `command` remotely only if it is a list of plain
commands: one command, or several joined with `&&`, optionally wrapped in
`cmd.exe /C "..."` on Windows. Shell no-ops (`:`, `true`, `cd .`) are
dropped. Commands needing a shell otherwise (pipes, redirections, variable
expansions, `||`, `;`) run locally.

Steps whose program is a **local tool** (declared in the toolchains file) run
on the host, in the build directory; the others form the remote action. Local
steps may come before or after the remote ones, not between them. For
example, with `cmake` declared local, CMake's archive command

```
: && cmake -E rm -f libfoo.a && ar qc libfoo.a a.o b.o && :
```

deletes the old archive locally (archivers append to existing archives), then
creates it remotely, in an empty directory. An edge whose steps all use local
tools runs locally.

Local steps before the remote action run after it was prepared: files they
create are not inputs of the action. Local steps after it see its outputs.

A remote action with several steps runs through `/bin/sh -c` on the worker;
on Windows hosts such edges run locally (cmd.exe's quoting does not survive a
worker's own argument quoting), so prefer single-step commands (e.g.
`llvm-ar qc`, which also writes the archive index, instead of `ar` then
`ranlib`).

## Toolchains file

```json
{
  "version": 1,
  "toolchains": [
    {
      "language": "CXX",
      "tool": "C:/llvm/bin/clang-cl.exe",
      "inputs": ["C:/llvm/bin", "C:/llvm/lib/clang"],
      "platform": { "OSFamily": "Windows", "container-image": "docker://…" }
    },
    { "tool": "C:/Program Files/CMake/bin/cmake.exe", "local": true }
  ]
}
```

- `tool`: absolute, normalized path of an executable.
- `local` (default `false`): the tool only runs on the host (see above).
- `language` (optional, informative).
- `inputs`: absolute files or directories the tool needs remotely: its own
  binaries, runtime libraries and resource directory. Sysroots/SDKs are **not**
  listed; the executor's include scanner finds the headers a compile reads.
  If empty, only the executable is sent.
- `platform`: REAPI platform properties for actions running the tool.

The executor identifies a step's tool from its first token, resolved to an
absolute path (relative to the build directory, or through `PATH`), and looks
it up by `tool` (case-insensitively on Windows). An executable with no
declaration is sent as an ordinary input with the default platform.

CMake writes one entry per enabled language (`CMAKE_<LANG>_COMPILER`, with
`CMAKE_<LANG>_COMPILER_REMOTE_INPUTS` and `CMAKE_REMOTE_PLATFORM` from the
toolchain file) and declares itself a local tool.

## Targets file

```json
{
  "version": 1,
  "targets": [
    { "name": "foo", "kind": "static_library" },
    { "name": "app", "kind": "executable" }
  ]
}
```

The targets named by `rbe_target`, with a free-form `kind`. Build event
streams report them (`TargetConfigured` / `TargetComplete`, with the
`rbe_deliverable` outputs). CMake writes its targets with their lowercased
type: `executable`, `static_library`, `shared_library`, `module_library`,
`object_library`, `utility`.

## CMake specifics

Mnemonics (`<LANG>` is the CMake language name, such as `C`, `CXX`, `RC`,
`CUDA`):

| Mnemonic           | Edge |
|--------------------|------|
| `<LANG>Compile`    | Object compilation |
| `<LANG>Scan`       | Dependency scanning for C++20 modules / Fortran |
| `<LANG>Dyndep`     | Dyndep collation (`rbe_local = 1`) |
| `<LANG>Link`       | Executable, shared library or module library link |
| `<LANG>Archive`    | Static library |
| `CustomCommand`    | `add_custom_command` / `add_custom_target` edges |
| `CMakeRegenerate`  | The `build.ninja` regeneration edge (`rbe_local = 1`) |
| `CMakeHelper`      | Other edges running `cmake` itself (copy, symlink, clean, …) |

CMake marks `rbe_local = 1` on custom commands, dyndep collation,
regeneration and its other utilities (clean, help), edges running `PRE_LINK`
or `POST_BUILD` commands, and compiles wrapped by a compiler launcher or a
code check (clang-tidy, …). Steps running `cmake` itself inside other edges
(deleting an archive, `vs_link_exe` wrappers) are left to the local tool
rule.
