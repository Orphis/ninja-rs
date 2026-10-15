//! Gives the ninja-rs version (`server::version`) the commit the sources
//! are at, `NINJA_RS_COMMIT`, when they're a git checkout. Bazel builds
//! don't run this: they're stamped instead.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).current_dir(&dir).output().ok().filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
    };
    let Some(commit) = git(&["rev-parse", "--short=12", "HEAD"]) else { return };
    // Run again after a commit or a checkout: HEAD, and the branch's ref
    // (or the packed ones). A file that doesn't exist would run it always.
    let mut watched = vec!["HEAD".to_string()];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.extend([branch, "packed-refs".to_string()]);
    }
    for name in watched {
        if let Some(path) = git(&["rev-parse", "--git-path", &name]).map(|p| dir.join(p))
            && path.is_file()
        {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rustc-env=NINJA_RS_COMMIT={commit}");
}
