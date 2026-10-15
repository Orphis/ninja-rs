//! Gives the ninja-rs version (`server::version`) the commit the sources
//! are at, `NINJA_RS_COMMIT`, when the workspace is a git checkout of its
//! own (not sources copied into another repository), with `.dirty` after it
//! when tracked files differ from it. Bazel builds don't run this: they're
//! stamped instead (`tools/workspace_status.*`).

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let Some(root) = manifest.parent().and_then(Path::parent) else { return };
    // What git says in the workspace root, empty or not.
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").arg("--no-optional-locks").args(args).current_dir(root).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // The top of a checkout holding this crate: not one the sources were
    // copied into.
    let ours =
        git(&["rev-parse", "--show-cdup"]).is_some_and(|up| up.is_empty()) && git(&["ls-files", "--error-unmatch", "crates/ninja-daemon/build.rs"]).is_some();
    let Some(commit) = git(&["rev-parse", "--short=12", "HEAD"]).filter(|c| ours && !c.is_empty()) else { return };
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_none_or(|s| !s.is_empty());
    // Run again after a commit, a checkout or a change to the index: HEAD,
    // the branch's ref (or the packed ones), the index. A file that doesn't
    // exist would run it always.
    let mut watched = vec!["HEAD".to_string(), "index".to_string()];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]).filter(|b| !b.is_empty()) {
        watched.extend([branch, "packed-refs".to_string()]);
    }
    let mut paths: Vec<PathBuf> = watched.iter().filter_map(|name| git(&["rev-parse", "--git-path", name])).map(|p| root.join(p)).collect();
    // And after the first edit of a clean tree, which makes it dirty: the
    // tracked files and directories (rather than every edit, which would
    // build this crate again each time).
    if !dirty {
        paths.extend(git(&["ls-tree", "--name-only", "HEAD"]).unwrap_or_default().lines().map(|name| root.join(name)));
    }
    for path in paths.iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let dirty = if dirty { ".dirty" } else { "" };
    println!("cargo:rustc-env=NINJA_RS_COMMIT={commit}{dirty}");
}
