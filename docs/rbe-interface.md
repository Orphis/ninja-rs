> **Note:** this is a copy. The canonical version of this contract lives in
> `Dev/ninja-rbe/docs/cmake-interface.md` (the C++ executor project) and is
> shared with the modified CMake. Do not edit this copy; propose changes there.
> Copied on 2026-10-08.

# CMake ⇄ ninja-rbe interface (v1)

This is the contract between CMake's Ninja generator in RBE mode and the
`ninja-rbe` executor. Design doc: "CMake Ninja RBE Mode: Design"
(https://claude.ai/code/artifact/f5fd7431-9c04-4cf4-88ce-8c7655c0fd4e).

Everything here is plain ninja syntax: stock ninja ignores it and still builds
the project locally.

## Activation

CMake: configure with `-DCMAKE_NINJA_RBE=ON` (Ninja or Ninja Multi-Config
generator).

`build.ninja` then contains, at file scope:

```ninja
cmake_rbe = 1
cmake_rbe_toolchains = CMakeFiles/rbe_toolchains.json
```

`cmake_rbe_toolchains` is relative to the build directory (the directory
holding `build.ninja`). For Ninja Multi-Config, both variables appear in every
`build-<Config>.ninja` and in `build.ninja`.

**Without `cmake_rbe = 1` the executor must run every edge locally.** A
`build.ninja` from the normal generator is never executed remotely.

## Edge bindings

Bindings are written on `build` edges only (ninja rejects unknown variables on
`rule` blocks).

| Binding        | Values                          | Meaning |
|----------------|---------------------------------|---------|
| `cmake_target` | CMake target name               | Owning target. Absent for edges with no owning target (regeneration, global utilities). |
| `mnemonic`     | see below                       | Action kind; used for RE `action_mnemonic` and BES. |
| `local`        | `1` / absent                    | Edge must run locally. Unmarked edges may run remotely. |
| `deliverable`  | `1` / absent                    | The edge's explicit outputs are final artifacts (downloaded in "toplevel" mode). |

Example:

```ninja
build src/foo/CMakeFiles/foo.dir/a.cpp.obj: CXX_COMPILER__foo_Release ../src/foo/a.cpp
  cmake_target = foo
  mnemonic = CXXCompile
```

### Mnemonics

`<LANG>` is the CMake language name verbatim (`C`, `CXX`, `RC`, `ASM`,
`Fortran`, `CUDA`, `HIP`, `Swift`, …).

| Mnemonic           | Edge |
|--------------------|------|
| `<LANG>Compile`    | Object compilation |
| `<LANG>Scan`       | Dependency scanning for C++20 modules / Fortran |
| `<LANG>Dyndep`     | Dyndep collation (`cmake -E cmake_ninja_dyndep`) — always `local = 1` |
| `<LANG>Link`       | Executable, shared library or module library link (link language) |
| `<LANG>Archive`    | Static library |
| `CustomCommand`    | `add_custom_command` / `add_custom_target` edges, PRE/POST_BUILD-free |
| `CMakeRegenerate`  | The `build.ninja` regeneration edge — always `local = 1` |
| `CMakeHelper`      | Any other edge whose command runs `cmake` itself (copy, symlink, clean, …) |

### What CMake marks `local = 1`

- every edge whose command runs `${CMAKE_COMMAND}` (cmake -E helpers, regeneration,
  dyndep collation, Windows `vs_link_exe`/`vs_link_dll`/`cmcldeps` wrappers);
- all `CustomCommand` edges (a future opt-in property will let them run remotely);
- link edges running `PRE_LINK` / `POST_BUILD` commands (until those get their own edges);
- compile edges wrapped by a compiler launcher or a code check (clang-tidy, …);
- ExternalProject steps (they are custom commands).

The executor may apply its own, stricter policy on top (phase 1: only
`*Compile` edges run remotely).

**Exception — archive pre-delete:** `<LANG>Archive` edges are *not* marked
local just because their command starts with `<cmake> -E rm -f <archive>`
(CMake deletes the old archive because `ar qc` appends). When running such an
edge remotely, the executor must drop that `cmake -E rm -f <output>` step
(it is a no-op in a fresh execution root), typically from a chain like
`cmd.exe /C "$PRE_LINK && <cmake> -E rm -f X && ar qc X ... && $POST_BUILD"`
on Windows hosts or `: && <cmake> -E rm -f X && ar qc X ... && :` on Unix
hosts.

## Toolchain declarations

`CMakeFiles/rbe_toolchains.json`, written at generate time:

```json
{
  "version": 1,
  "toolchains": [
    {
      "language": "CXX",
      "compiler": "C:/llvm/bin/clang-cl.exe",
      "inputs": ["C:/llvm/bin", "C:/llvm/lib/clang"],
      "platform": { "OSFamily": "Windows", "container-image": "docker://…" }
    }
  ]
}
```

- `compiler`: absolute, normalized path of `CMAKE_<LANG>_COMPILER`.
- `inputs`: from `CMAKE_<LANG>_COMPILER_REMOTE_INPUTS` (a CMake list of
  absolute files or directories): the compiler's own binaries, runtime
  libraries and resource dir. Sysroots/SDKs are **not** listed; their headers
  and libraries are found by the executor's scanner. If unset, `inputs` is
  `[]` and the executor sends only the compiler executable.
- `platform`: from `CMAKE_REMOTE_PLATFORM`, a CMake list of `KEY=VALUE`
  entries, set in the toolchain file. Same object in every entry.

The executor identifies an action's toolchain from the first token of its
command line, resolved to an absolute path, and looks it up by `compiler`
(case-insensitive on Windows). An executable with no declaration is sent as an
ordinary input with the default platform.

## Target metadata

In RBE mode CMake always writes a File API `codemodel-v2` reply (no query file
needed), under `<build>/.cmake/api/v1/reply/`. The executor finds it through the
newest `index-*.json`. Used for BES in phase 2; phase 1 does not need it.
