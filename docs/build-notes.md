# Build notes

How ninja-rs itself is built: with Bazel (Bzlmod) and the hermetic LLVM toolchain for releases
and CI, with Cargo for development. Testing is in [dev/testing.md](dev/testing.md), the crates in
[dev/architecture.md](dev/architecture.md).

## Quick reference

```sh
# Linux x86_64 (also from WSL): the linux config applies by itself on Linux hosts.
bazelisk build //crates/ninja-rs          # bazel-bin/crates/ninja-rs/ninja-rs (alias //:ninja-rs)
bazelisk test //...

# Windows x86_64, MSVC ABI: needs the Microsoft licenses accepted in user.bazelrc (below).
bazelisk build //crates/ninja-rs
bazelisk test //...

# A Windows GNU ABI binary, cross-built from Linux (no Microsoft downloads).
bazelisk build --config=cross_windows_gnullvm //crates/ninja-rs

# The commit in the version (ninja-rs -v --version).
bazelisk build --config=stamp //crates/ninja-rs

# Clippy on every Rust target, warnings as errors; run it on Windows and on Linux.
bazelisk build --config=clippy //...

# Development builds with your own Rust toolchain.
cargo build
cargo test
```

## Versions

| Component | Version | Notes |
|---|---|---|
| Bazel | 9.3.0 | `.bazelversion`; Bzlmod only |
| `llvm` (hermetic-llvm) | 0.8.24 | from the BCR, patched (see [Patches](#patches)); registers `@llvm//toolchain:all` |
| `rules_rs` | 0.1.0 | from the BCR, patched; wraps hermeticbuild's rules_rust, also patched |
| Rust under Bazel | 1.99.0 | edition 2024, through `rules_rs` toolchains |
| Rust under Cargo | 1.89 or later | `rust-version` in `Cargo.toml`; `Cargo.lock` was written by cargo 1.91 |
| `platforms`, `bazel_skylib`, `rules_cc` | 1.1.0, 1.9.2, 0.2.26 | `MODULE.bazel` |
| `zstd` (C library) | 1.5.7.bcr.1 | the BCR module, linked into `zstd-sys` instead of its build script |
| tonic / prost | 0.14.6 / 0.14.4 | from `Cargo.lock`, as are the rows below |
| tokio | 1.53.2 | |
| rustls / tokio-rustls | 0.23.45 / 0.26.6 | `ring` provider; rbe-client's own TLS connector |
| hyper-util / h2 | 0.1.21 / 0.4.20 | h2 for fake-reapi's misbehaving servers |
| windows-sys | 0.61.2 | features named per crate |
| REAPI protos | `bazel-remote-apis` 0.30.0 | pre-generated prost and tonic code, no `protoc` |
| sha2 / fastcdc | 0.10.9 / 3.2.1 | sha2 always built with `-Copt-level=3` |

`Cargo.toml` and `Cargo.lock` are the source of truth for crates: Bazel reads them through
`rules_rs`' `crate.from_cargo` (the `@crates` hub) for three triples,
`x86_64-pc-windows-msvc`, `x86_64-pc-windows-gnullvm` and `x86_64-unknown-linux-gnu`. Two
annotations in `MODULE.bazel` adjust crates: `zstd-sys` takes the C library from the `zstd`
module (its build script would compile it outside Bazel's toolchain), and `sha2` is optimized in
every build (`[profile.dev.package.sha2]` does the same for Cargo): debug builds hash
toolchains of hundreds of MB, the tests' included, in seconds rather than minutes.

The version is one number for both build systems: `[workspace.package] version` in `Cargo.toml`
and `module(version = ...)` in `MODULE.bazel`, which the BUILD files pass to the crates
(`module_version()`). `options::tests::version_is_the_workspace_one` checks that they agree.

## Why rules_rs

hermetic-llvm's README recommends rules_rs for Rust and C/C++ together: it registers Rust
toolchains whose linker is `@llvm`'s clang or lld-link, provides target triple platforms, and
resolves crates from `Cargo.lock`. Linux and gnullvm builds work unpatched; Windows with an MSVC
host and target needs the patches below.

## Patches

Upstream exercises Rust on Windows only through gnullvm. With an MSVC host and target, five
problems needed patches, in `third_party/patches/` and applied from `MODULE.bazel`. Each changes
behaviour for MSVC targets or Windows hosts only, and each should go upstream:

| Patch | Module | Problem | Fix |
|---|---|---|---|
| `llvm-msvc-exec-overlay-cycle.patch` | `llvm` (`single_version_override`) | Every MSVC compile and link uses a case-insensitive SDK overlay made by a tool built for the exec platform; with an MSVC exec platform the tool depends on itself (an analysis-time cycle). | When Bazel runs on Windows (case-insensitive file systems), the runtime stages that build the tool (`stage0`, `stage1`, `stage1_hosted`) skip the overlays; user code (`complete`) keeps them. |
| `rules_rs-msvc-lld-link.patch` | `rules_rs` (`single_version_override`) | rules_rs gives rustc the cc toolchain's link tool, the clang-cl driver, but rustc passes link.exe-style arguments for MSVC targets. | MSVC Rust toolchains link with `@llvm//tools:lld-link` directly (`linker_preference = "rust"`). Not `rust-lld`: its path inside the generated sysroot exceeds `MAX_PATH`, which LLVM tools need to locate themselves. |
| `rules_rust-linker-preference-unset.patch` | rules_rust (`rules_rust.patch`) | `linker_preference` rejects `""`, so a `select()` can't leave it unset for non-MSVC triples. | Allow `""` (already treated as unset). |
| `rules_rust-msvc-link-search-args.patch` | rules_rust | With a direct linker, rules_rust keeps only the Unix search options of the cc toolchain's link arguments; MSVC's are dropped, so the SDK and CRT libraries aren't found. | Keep `/LIBPATH:` and `/vfsoverlay:`, including clang-cl's forwarded `/clang:-Xlinker /clang:<option>` pairs. |
| `rules_rust-msvc-build-script-no-vs-probe.patch` | rules_rust | cc-rs, in build scripts, finds a locally installed Visual Studio through the registry and merges its `LIB`, `INCLUDE` and `PATH` into the hermetic clang-cl, leaking host paths (which the build-script runner rejects). | For MSVC targets, set `VCToolsVersion` to a version that can't exist, which find-msvc-tools documents as restricting its search. |

The rules_rust patches are one file each: Bazel's patcher misplaces hunks of multi-file patches.
`*.patch` files are `-text` in `.gitattributes`, so they apply byte for byte.

## Hosts and platforms

`platforms/BUILD.bazel` defines the host platforms with the constraints rules_rs and `@llvm`
need; they serve as target platforms for native builds too, so that tests run on the host.
`--enable_platform_specific_config` applies the `linux` or `windows` config by itself.

- **Linux x86_64** (verified in WSL, Ubuntu 22.04): `//platforms:linux_x86_64_gnu_host`
  (glibc 2.28 constraint). Toolchain resolution picks `@llvm//toolchain:linux_x86_64_cc_toolchain`
  and the rules_rs `default_linux_x86_64_1_99_0_rust_toolchain`; rustc links with `@llvm`'s
  `clang++`.
- **Windows, MSVC ABI** (`x86_64-pc-windows-msvc`): `//platforms:win_msvc`, both exec and target
  platform, because rules_rs only ships an MSVC `rustc` for Windows, which only loads proc macros
  built for MSVC. It links against the Microsoft VC runtime and Windows SDK, which the `llvm`
  module downloads once their licenses are accepted (below). No local Visual Studio or clang is
  used. The platform's name is short on purpose: with `--experimental_platform_in_output_dir` it
  is in every output path, and deep libc++ header paths otherwise exceed `MAX_PATH` in `@llvm`'s
  tools. The binary imports `VCRUNTIME140.dll` and the UCRT.
- **Windows, GNU ABI** (`--config=cross_windows_gnullvm`, from Linux): a PE32+ `ninja-rs.exe`
  with the MinGW-w64 CRT built from source by the `llvm` module, no Microsoft downloads. It runs on
  Windows. A gnullvm host build isn't possible: rules_rs has no gnullvm `rustc`.
- **Windows, MSVC ABI cross-built from Linux** (`--config=cross_windows_msvc`): configured, and
  needs the licenses; not verified, since it needs the MSVC Rust standard library and the
  Microsoft packages in the Linux output base.

## The Microsoft licenses (user.bazelrc)

The `llvm` module downloads the Microsoft VC runtime and the Windows SDK only after their
licenses are accepted, which is the user's decision. `.bazelrc` ends with
`try-import %workspace%/user.bazelrc`, and `user.bazelrc` is git-ignored: to accept, create it
with

```text
common --repo_env=BAZEL_MSVC_RUNTIME_VISUAL_STUDIO_EULA=1
common --repo_env=BAZEL_WINDOWS_SDK_EULA=1
```

Builds for Linux and the gnullvm cross build don't need them. The hermetic-llvm-cmake toolchain,
used to build C and C++ projects with ninja-rs (and by `tests/nativelink/run.sh`), has its own
setting for the same licenses: `HERMETIC_ACCEPT_MICROSOFT_EULA=1`, as a CMake variable or in
the environment, for its Windows MSVC targets.

## The version and the commit

`ninja_rs::version()` is the package version, with the commit it was built from when the build
knew it: `0.1.0+1a2b3c4d5e6f`, with `.dirty` after it when tracked files differed from the
commit. `ninja-rs -v --version` prints it on a second line; build events, remote requests
(`ToolDetails`) and the daemon report it.

- **Bazel** builds don't run git unless stamped: `--config=stamp` adds `--stamp` and
  `--workspace_status_command=tools/workspace_status` (cmd runs `tools/workspace_status.cmd` for
  that name on Windows), which prints `STABLE_GIT_COMMIT`. `crates/ninja-rs/commit.env` passes
  it to the library as `NINJA_RS_COMMIT` (`rustc_env_files`, `stamp = -1`). An unstamped build
  says `0.1.0`. Running git on every Bazel invocation cost seconds in a WSL checkout of a Windows
  tree, which is why it is opt-in.
- **Cargo** builds ask git in `crates/ninja-rs/build.rs`, only when the workspace is the top of a
  git checkout tracking `crates/ninja-rs/build.rs` (not sources copied into another repository).
  The script is in the top crate so that a new commit rebuilds that crate alone. It reruns after
  a commit or a checkout (`HEAD`, the branch's ref), at the first edit of a clean tree (it watches
  the tracked files, not their directories, which untracked files change), and, once dirty, when
  git's index changes (a commit, a stash, a checkout of the files). Edits undone without git
  leave `.dirty` until then. In a checkout Windows and WSL share, a `git status` on one side
  rewrites the index and rebuilds the crate on the other while the tree is dirty.
- The same script sets `NINJA_RS_SOURCE_DIR` for Cargo builds, so tests run without Cargo (in a
  debugger, an IDE) find the sources (see [dev/testing.md](dev/testing.md)).

## The Windows manifest (long paths)

`ninja-rs.exe` embeds an application manifest declaring `longPathAware`, as upstream ninja does:
without it Windows keeps the 260-character `MAX_PATH` limit for the process even with long
paths enabled system-wide (`-C` into a deep directory fails, and so do commands' paths).

`crates/ninja-rs/build.rs` writes the manifest directly as a COFF object whose `.rsrc` section
holds the resource directory and the manifest (`ninja-rs-manifest.o`, what windres would make,
for x86_64, x86 and aarch64), so that no resource compiler and no `mt.exe` is needed: the
hermetic toolchain has no working `mt.exe`, and lld-link's own manifest embedding
(`/MANIFEST:EMBED /MANIFESTINPUT`) runs `mt.exe` from `PATH` unless it was built with libxml2.
Every Windows linker takes such an object: link.exe and lld-link for MSVC targets, the MinGW
linkers and LLVM's for GNU targets. It is found through the library search path, not named by
its path (Bazel's sandbox moves `OUT_DIR` between the script and the link and fixes up library
paths only): `ninja-rs-manifest.o` as a link argument for MSVC, `-l:ninja-rs-manifest.o` for GNU.
The same script serves Cargo and Bazel (Bazel sets `NINJA_RS_BAZEL=1` for it, which skips git).

Windows still limits the working directory of a new process to `MAX_PATH`, manifest or not
(ninja behaves the same): commands fail to start from deeper directories.

## Clippy and formatting

`--config=clippy` runs `rust_clippy_aspect` on every Rust target with the workspace's
`clippy.toml` (exported from the root `BUILD.bazel`), warnings as errors, `--keep_going` so every
target is reported. Run it on Windows and on Linux: each lints only the code its own system
builds (`cfg(windows)`, `cfg(unix)`), with the module's Rust, which Cargo's may not be.
`clippy.toml` forbids std's printing macros and `std::env::temp_dir` outside the places that
allow them with a reason (see [dev/testing.md](dev/testing.md#clippy-rules)).

Rust code is formatted with `cargo fmt --all` (`rustfmt.toml`: edition 2024, width 160); the code
copied from cargo-rbe keeps its own formatting. A pre-commit hook refuses commits whose staged
Rust files aren't formatted; enable it once per clone with `git config core.hooksPath .githooks`.

## Line endings

`.gitattributes` gives text files LF in every checkout, Windows' too (`* text=auto eol=lf`), but
`*.cmd` files, which cmd reads, CRLF. A checkout Windows and WSL share is then clean in both. A
checkout made before this rule can be renormalized once, when it has no uncommitted changes
(the second command discards them): `git rm -r --cached -q .` then `git reset --hard -q`.

## WSL notes

- Running Bazel from WSL on the Windows checkout works; pass `--symlink_prefix=bazel-linux-` so
  the convenience symlinks don't clash with Windows' (they point into the WSL output base).
  `.gitignore` lists the `bazel-linux-*` links.
- Wrapper scripts are easier than inline `wsl.exe -- bash -c '...'`: Git Bash rewrites `/mnt/...`
  arguments unless `MSYS_NO_PATHCONV=1` is set.
- WSL and Windows keep separate output bases; caches aren't shared between them.

## Offline builds

A fresh output base can't build fully offline: rules_rs downloads its toolchain manifests
without checksums, and the crates.io registry configuration repository. An output base that has
built once (`MODULE.bazel.lock` and the repositories in place) builds and tests with
`--repository_disable_download`. `bazelisk build --lockfile_mode=error` checks that
`MODULE.bazel.lock` is up to date.
