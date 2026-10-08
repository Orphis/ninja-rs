# Code reused from cargo-rbe

Source: the user's `cargo-rbe` project (github.com/Orphis/cargo-rbe), commit
`a22f3610b45522741860a713139c152449c21042`, license MIT OR Apache-2.0.
The cargo-rbe repository itself was not modified.

## rbe-client (copied)

`crates/rbe-client` → `rbe-client/`: REAPI v2 client (Capabilities,
FindMissingBlobs, BatchUpdateBlobs/BatchReadBlobs, ByteStream read/write with
zstd when offered, ActionCache get/update, Execute, `RequestMetadata` per
action) and the Merkle `InputTree` builder.

Local changes:

- `ConnectOptions::headers` / `Client::with_headers`: extra gRPC headers on
  every call (`--remote_header=K=V`).
- The process-wide `static FILE_DIGESTS` cache was removed (ninja-rs has no
  global state). `file_digest` takes an optional `&dyn DigestCache`; the
  cache key is the symlink-resolved path, so many links to one file are
  hashed once. `MemDigestCache` is provided, with `entries()` for
  persistence. `InputTree::add_file_cached` / `add_dir_cached` /
  `add_file_with_digest` / `contains` were added.
- `Cargo.toml` uses the ninja-rs workspace dependencies; a Bazel
  `BUILD.bazel` was added; an unused-variable warning on Windows was fixed.

## Not copied (yet)

- `rbe-bes` (BES client, ordered stream with acks/retries, BEP builder):
  intended for phase 2 (build event streaming). It generates code with
  `protox` + `tonic-prost-build` in `build.rs`, which works under rules_rs'
  build script support but was not needed for phase 1.
- `rbe-exec` (worker-side helper that substitutes an `@EXECROOT@` token and
  scrubs it from outputs): ninja-rs rewrites absolute paths to paths relative
  to the working directory instead, which works for C/C++ compilers without
  a helper binary on the worker. It remains the fallback design for tools
  that require absolute paths (see docs/paths.md).
- `rbe-rustc`: its `Mapper` (local prefix → exec-root mapping with path
  boundary checks) inspired `ninja-remote/src/paths.rs`; the rustc-specific
  layout isn't applicable.
