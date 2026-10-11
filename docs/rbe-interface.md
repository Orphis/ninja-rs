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
| `rbe_artifact`    | `1` / absent     | The edge creates its target's artifacts (an executable, a library): the outputs build events report for the target. |
| `rbe_deliverable` | `1` / absent     | The edge's outputs are in the minimal set the executor downloads (see Downloads). |
| `rbe_config`      | configuration name | The configuration (in the targets file) the edge builds for. Without it, the file's first configuration. Usually set once at file scope. |

Edges with `generator = 1` (manifest regeneration) and edges in the `console`
pool always run locally.

### Downloads

Outputs of remote actions stay in the CAS unless the executor downloads
them. In ninja-rs (planned modes, all outputs are downloaded today):

- **all**: every output.
- **minimal**: the outputs of the `rbe_deliverable` edges the build runs.

In both modes, what the command line names is downloaded, whatever it is (a
target, a library, an object file): no annotation is needed for that. Local
edges get their inputs downloaded before they run.

CMake marks the artifacts of executables, shared libraries and module
libraries deliverable, not those of static libraries; its `RBE_DELIVERABLE`
target property changes that per target (off for a code generator only used
during the build, on for a static library the project distributes).

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
toolchain file), one per other tool its build statements run (the archiver,
ranlib and a linker run directly: `CMAKE_AR`, `CMAKE_RANLIB`,
`CMAKE_LINKER` and their `CMAKE_<LANG>_COMPILER_*` variants), with the same
platform, and declares itself a local tool. A step whose tool isn't
declared gets the platform of another declared tool of the command, or
none: on a real cluster, an action without platform properties usually
finds no worker.

## Targets file

```json
{
  "version": 1,
  "configurations": [
    {
      "name": "Release",
      "mnemonic": "x86_64-linux-clang-Release",
      "platform": "Linux",
      "cpu": "x86_64",
      "variables": { "CMAKE_BUILD_TYPE": "Release", "CMAKE_CXX_COMPILER_ID": "Clang" }
    }
  ],
  "targets": [
    { "name": "foo", "kind": "static_library", "label": "//src/foo:foo" },
    { "name": "app", "kind": "executable", "label": "//:app" }
  ]
}
```

- `targets`: the targets named by `rbe_target`, with a free-form `kind`
  and the `label` build events use (default `//:<name>`).
- `configurations`: the build configurations edges name in `rbe_config`.
  Everything in an entry describes the configuration: the executor derives
  its id from all of it (a digest), so configurations that differ in any
  setting, the toolchain included, never share an id. Without any, the
  executor uses one default configuration.

Build event streams (BES) report the targets (`TargetConfigured`, then
`TargetCompleted` per configuration, with the `rbe_artifact` outputs) and
the configurations; remote actions carry the same label and configuration
id in their `RequestMetadata`.

CMake writes its targets with their lowercased type (`executable`,
`static_library`, `shared_library`, `module_library`, `object_library`,
`utility`) and the label `//<source dir>:<name>`, the source directory
relative to the top one. Its configurations are the build types, with the
target system, processor and compilers in their variables; the multi-config
generator sets `rbe_config` at the top of each configuration's file.

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

Statements that only exist because of modules (C++20 or Fortran: dependency
scanning, dyndep collation, BMI-only compilations of imported module
targets) belong to an implicit target, `<target>@modules` (kind `modules`,
label `//<source dir>:<target>@modules`), so that a target's own actions
compare between builds with and without modules. Compilations, which also
produce BMIs, stay with their target. The BMIs CMake builds for an imported
module target (such as `std`, for `import std`) belong to
`<imported target>@modules.<digest>` (`::` replaced by `__`), the digest
being of the settings they are built with: the name stays the same from one
build to the next, and differs for each variant.

CMake marks `rbe_local = 1` on custom commands, dyndep collation,
regeneration and its other utilities (clean, help), edges running `PRE_LINK`
or `POST_BUILD` commands, and compiles wrapped by a compiler launcher or a
code check (clang-tidy, …). Steps running `cmake` itself inside other edges
(deleting an archive, `vs_link_exe` wrappers) are left to the local tool
rule.
