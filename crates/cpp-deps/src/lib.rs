//! Finds the files a C/C++ compile reads, with a fast preprocessor that
//! evaluates directives exactly and skips everything else.
//!
//! - [`lex`]: a file down to its directives (cached per file version).
//! - [`macros`]: definitions and expansion.
//! - [`expr`]: `#if` expressions.
//! - [`scan`]: a translation unit: conditionals, includes, guards.
//! - [`probe`]: what a compiler predefines and answers to `__has_*`.
//! - [`driver`]: a GCC or Clang command line scanned, with the compiler
//!   probed and its answers cached.
//! - [`external`]: the alternatives, asking `clang-scan-deps` or the
//!   compiler itself (`-M`, `/showIncludes`).
//!
//! Compiler-specific knowledge comes from outside: the predefined macros,
//! the `__has_*` operators the compiler has, and an [`scan::Oracle`]
//! answering their queries (e.g. from [`probe`]).

pub mod driver;
pub mod expr;
pub mod external;
pub mod hmap;
pub mod intern;
pub mod lex;
pub mod macros;
pub mod path_map;
pub mod probe;
pub mod scan;
pub mod vfs;

pub use driver::Driver;
pub use scan::{Cache, CommandMacro, Config, NoOracle, Oracle, Output, scan};

/// The compiler the variable `var` names, for the tests against real ones
/// (see build.rs): a path relative to where the tests start (Bazel's
/// runfiles), made absolute, or a name found in PATH.
#[cfg(test)]
pub(crate) fn test_compiler(var: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(std::env::var_os(var).unwrap_or_else(|| panic!("{var} names no compiler to test against")));
    if path.components().count() > 1 { std::path::absolute(&path).unwrap() } else { path }
}

/// A test's fake tool in `dir`: `name.cmd` running `batch` on Windows, a
/// script `name` running `sh` elsewhere. The script is run once before it's
/// returned: another test's thread forking while it was open for writing
/// leaves a child holding it so, and running it fails (ETXTBSY) until that
/// child has started its own program.
#[cfg(test)]
pub(crate) fn test_tool(dir: &std::path::Path, name: &str, batch: &str, sh: &str) -> std::path::PathBuf {
    if cfg!(windows) {
        let tool = dir.join(format!("{name}.cmd"));
        std::fs::write(&tool, batch).unwrap();
        return tool;
    }
    let tool = dir.join(name);
    std::fs::write(&tool, format!("#!/bin/sh\n[ \"$1\" = --cpp-deps-ready ] && exit 0\n{sh}")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for _ in 0..1000 {
        match std::process::Command::new(&tool).arg("--cpp-deps-ready").status() {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(std::time::Duration::from_millis(5)),
            r => {
                assert!(r.unwrap().success());
                return tool;
            }
        }
    }
    panic!("{} stays busy", tool.display());
}
