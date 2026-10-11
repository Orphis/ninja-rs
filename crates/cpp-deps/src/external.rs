//! Alternatives to the scanner, asking external tools for a compile's
//! dependencies: the toolchain's `clang-scan-deps`, or the compiler's own
//! preprocessor (`-M`, or `/E /showIncludes` for `cl`-style compilers).
//! Exact by construction (it's the compiler's preprocessor), at the cost
//! of a process per compile and, for the compiler, a full preprocess.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::probe::make_deps;
use crate::scan::normalize;

/// Whether a compiler takes `cl`-style flags (`cl.exe`, `clang-cl`, or a
/// clang driver in `cl` mode).
pub fn is_cl_style(argv: &[String]) -> bool {
    let Some(program) = argv.first() else { return false };
    let base = Path::new(&program.replace('\\', "/")).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    base == "cl" || base.ends_with("clang-cl") || argv.iter().any(|a| a == "--driver-mode=cl")
}

fn run(cmd: &mut Command, what: &str) -> Result<std::process::Output, String> {
    let out = cmd.output().map_err(|e| format!("running {what}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().find(|l| l.contains("error")).or_else(|| err.lines().next()).unwrap_or("");
        return Err(format!("{what} failed: {first}"));
    }
    Ok(out)
}

fn absolute(files: Vec<String>, cwd: &Path) -> Vec<PathBuf> {
    files.into_iter().map(|f| normalize(&cwd.join(f))).collect()
}

/// The files a compile reads, from the `clang-scan-deps` next to its
/// compiler (Clang only; `clang-cl` commands too).
pub fn clang_scan_deps(argv: &[String], cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let compiler = Path::new(argv.first().ok_or("empty command")?);
    let tool = compiler.with_file_name(if cfg!(windows) { "clang-scan-deps.exe" } else { "clang-scan-deps" });
    if !tool.is_file() {
        return Err(format!("no clang-scan-deps next to {}", compiler.display()));
    }
    let out = run(Command::new(&tool).args(["-format=make", "--"]).args(argv).current_dir(cwd), "clang-scan-deps")?;
    Ok(absolute(make_deps(&String::from_utf8_lossy(&out.stdout)), cwd))
}

/// Flags with a separate argument that name outputs.
const OUTPUTS: &[&str] = &["-o", "-MF", "-MT", "-MQ"];

/// The files a compile reads, from the compiler's own preprocessor: the
/// command without its outputs, with `-M` (GCC, Clang), or `/E
/// /showIncludes` (`cl`, `clang-cl`; `deps_prefix` is the localized
/// prefix of its notes, `Note: including file:` by default). GCC doesn't
/// list the files `__has_include` finds, which may then be missing.
pub fn compiler_deps(argv: &[String], cwd: &Path, deps_prefix: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let program = argv.first().ok_or("empty command")?;
    let cl = is_cl_style(argv);
    let mut args: Vec<String> = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if cl {
            // cl's flags are case-sensitive: /FI is a forced include, /Fi
            // an output.
            if let Some(f) = a.strip_prefix('/').or_else(|| a.strip_prefix('-')) {
                let output = ["Fo", "Fd", "Fe", "Fa", "Fi", "Fm", "Fp", "FR", "Fr", "Yc", "Yu"].iter().any(|p| f.starts_with(p));
                if f == "c" || f == "showIncludes" || output {
                    continue;
                }
                if f.starts_with("sourceDependencies") {
                    if f == "sourceDependencies" {
                        i += 1;
                    }
                    continue;
                }
            }
        } else {
            if OUTPUTS.contains(&a.as_str()) {
                i += 1;
                continue;
            }
            if a == "-c"
                || a == "-MD"
                || a == "-MMD"
                || a == "-MP"
                || a.starts_with("-MF")
                || a.starts_with("-MT")
                || a.starts_with("-MQ")
                || (a.starts_with("-o") && a.len() > 2)
            {
                continue;
            }
        }
        args.push(a.clone());
    }
    // Flags go before an end of the options (`--`, as CMake writes before
    // the source).
    let options_end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    if cl {
        args.splice(options_end..options_end, ["/E".to_string(), "/showIncludes".to_string()]);
        let out = run(Command::new(program).args(&args).current_dir(cwd), program)?;
        let prefix = deps_prefix.unwrap_or("Note: including file:");
        // The notes are on stderr when the preprocessed text goes to stdout.
        let text = String::from_utf8_lossy(&out.stderr);
        let mut files = Vec::new();
        for line in text.lines() {
            if let Some(f) = line.strip_prefix(prefix) {
                files.push(f.trim().to_string());
            }
        }
        let mut deps = absolute(files, cwd);
        // The notes don't name the source itself.
        let source_ext = |a: &str| ["c", "cc", "cpp", "cxx", "c++"].iter().any(|e| a.to_ascii_lowercase().ends_with(&format!(".{e}")));
        if let Some(source) = argv.iter().skip(1).rev().find(|a| !a.starts_with('/') && !a.starts_with('-') && source_ext(a)) {
            deps.insert(0, normalize(&cwd.join(source)));
        }
        deps.dedup();
        return Ok(deps);
    }
    args.insert(options_end, "-M".into());
    let out = run(Command::new(program).args(&args).current_dir(cwd), program)?;
    Ok(absolute(make_deps(&String::from_utf8_lossy(&out.stdout)), cwd))
}

/// A command line as Windows programs split it (CommandLineToArgvW).
pub fn split_windows(cmd: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut backslashes = 0;
    for c in cmd.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                in_arg = true;
            }
            '"' => {
                cur.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 == 1 {
                    cur.push('"');
                } else {
                    quoted = !quoted;
                }
                backslashes = 0;
                in_arg = true;
            }
            ' ' | '\t' if !quoted => {
                cur.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            c => {
                cur.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                cur.push(c);
                in_arg = true;
            }
        }
    }
    cur.extend(std::iter::repeat_n('\\', backslashes));
    if in_arg {
        args.push(cur);
    }
    args
}

/// A command line as a POSIX shell splits it (quotes and backslashes).
pub fn split_posix(cmd: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_arg = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_arg = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => cur.extend(chars.next()),
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_arg = true;
                cur.extend(chars.next());
            }
            ' ' | '\t' | '\n' => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            c => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(cur);
    }
    args
}

/// A command line (a compilation database's `command`) split as the host's
/// programs do.
pub fn split_command(cmd: &str) -> Vec<String> {
    if cfg!(windows) { split_windows(cmd) } else { split_posix(cmd) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splitting() {
        // Escaped quotes don't quote; backslashes are literal unless before
        // a quote.
        assert_eq!(split_windows(r#"cl /DA=\"x y\" "C:\dir b\c.cpp" -Dz\\"#), s(&["cl", r#"/DA="x"#, r#"y""#, r"C:\dir b\c.cpp", r"-Dz\\"]));
        assert_eq!(split_posix(r#"cc -DA=\"x\" 'a b.c' "c d" e\ f"#), s(&["cc", r#"-DA="x""#, "a b.c", "c d", "e f"]));
    }

    #[test]
    fn cl_style() {
        assert!(is_cl_style(&s(&["C:\\VC\\bin\\cl.exe", "/c", "a.cpp"])));
        assert!(is_cl_style(&s(&["clang-cl", "/c", "a.cpp"])));
        assert!(is_cl_style(&s(&["clang", "--driver-mode=cl", "/c", "a.cpp"])));
        assert!(!is_cl_style(&s(&["clang++", "-c", "a.cpp"])));
    }

    /// Against real compilers, when `CPP_DEPS_TEST_CXX` names a Clang
    /// (with clang-scan-deps next to it).
    #[test]
    fn real_compiler() {
        let Some(cxx) = std::env::var_os("CPP_DEPS_TEST_CXX") else { return };
        let dir = std::env::temp_dir().join(format!("cpp_deps_external_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.h"), "#define A 1\n").unwrap();
        std::fs::write(dir.join("main.cc"), "#include \"a.h\"\nint main() { return A; }\n").unwrap();
        let argv = vec![cxx.to_string_lossy().into_owned(), "-MD".into(), "-MF".into(), "x.d".into(), "-c".into(), "main.cc".into(), "-o".into(), "x.o".into()];
        let want = vec![dir.join("main.cc"), dir.join("a.h")];
        assert_eq!(compiler_deps(&argv, &dir, None).unwrap(), want);
        if Path::new(&cxx).with_file_name(if cfg!(windows) { "clang-scan-deps.exe" } else { "clang-scan-deps" }).exists() {
            assert_eq!(clang_scan_deps(&argv, &dir).unwrap(), want);
        }
        assert!(!dir.join("x.d").exists() && !dir.join("x.o").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Against cl.exe, when `CPP_DEPS_TEST_CL` names one.
    #[test]
    fn real_cl() {
        let Some(cl) = std::env::var_os("CPP_DEPS_TEST_CL") else { return };
        let dir = std::env::temp_dir().join(format!("cpp_deps_external_cl_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inc")).unwrap();
        std::fs::write(dir.join("inc/a.h"), "#define A 1\n").unwrap();
        std::fs::write(dir.join("forced.h"), "#define F 1\n").unwrap();
        std::fs::write(dir.join("main.cc"), "#include <a.h>\nint main() { return A + F; }\n").unwrap();
        let argv: Vec<String> = [cl.to_string_lossy().as_ref(), "/nologo", "/showIncludes", "/Iinc", "/FIforced.h", "/Fox.obj", "/Fdx.pdb", "/c", "main.cc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let deps = compiler_deps(&argv, &dir, None).unwrap();
        assert_eq!(deps, vec![dir.join("main.cc"), dir.join("forced.h"), dir.join("inc").join("a.h")]);
        assert!(!dir.join("x.obj").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
