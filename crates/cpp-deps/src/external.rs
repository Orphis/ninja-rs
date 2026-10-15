//! Alternatives to the scanner, asking external tools for a compile's
//! dependencies: the toolchain's `clang-scan-deps`, or the compiler's own
//! preprocessor (`-M`, or `/E /showIncludes` for `cl`-style compilers).
//! Exact by construction (it's the compiler's preprocessor), at the cost
//! of a process per compile and, for the compiler, a full preprocess.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::probe::{TIMEOUT, make_deps, output};
use crate::scan::normalize;

/// Whether a compiler takes `cl`-style flags (`cl.exe`, `clang-cl`, or a
/// clang driver in `cl` mode).
pub fn is_cl_style(argv: &[String]) -> bool {
    let Some(program) = argv.first() else { return false };
    let base = Path::new(&program.replace('\\', "/")).file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    base == "cl" || base.ends_with("clang-cl") || argv.iter().any(|a| a == "--driver-mode=cl")
}

/// Runs a scanner (`clang-scan-deps`, the compiler) for its output, killed
/// when it takes too long (see `probe::TIMEOUT`).
fn run(mut cmd: Command, what: &str) -> Result<std::process::Output, String> {
    cmd.stdin(Stdio::null());
    let out = output(cmd, None, TIMEOUT).map_err(|e| format!("running {what}: {e}"))?;
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
    let mut cmd = Command::new(&tool);
    cmd.args(["-format=make", "--"]).args(argv).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = run(cmd, "clang-scan-deps")?;
    Ok(absolute(make_deps(&String::from_utf8_lossy(&out.stdout)), cwd))
}

/// Flags with a separate argument that name outputs.
const OUTPUTS: &[&str] = &["-o", "-MF", "-MT", "-MQ", "-MJ"];

/// A `-Wp,` flag without its dependency file options (Linux's
/// `-Wp,-MMD,file`), which would write the dependencies there; None when
/// nothing is left.
fn without_wp_deps(a: &str) -> Option<String> {
    let Some(opts) = a.strip_prefix("-Wp,") else { return Some(a.to_string()) };
    let mut kept = Vec::new();
    let mut opts = opts.split(',');
    while let Some(o) = opts.next() {
        match o {
            "-MD" | "-MMD" | "-MF" | "-MT" | "-MQ" => {
                opts.next();
            }
            "-M" | "-MM" | "-MP" | "-MG" => {}
            o => kept.push(o),
        }
    }
    (!kept.is_empty()).then(|| format!("-Wp,{}", kept.join(",")))
}

/// The files a compile reads, from the compiler's own preprocessor: the
/// command without its outputs, preprocessing with its dependencies
/// written as it goes (GCC and Clang: `-E -MD`; not `-M`, with which GCC
/// expands no macros in text, so `__COUNTER__` counts differently), or
/// `/E /showIncludes` (`cl`, `clang-cl`; `deps_prefix` is the localized
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
                // clang-cl's dependency file (CMake's `-clang:-MD
                // -clang:-MT<obj> -clang:-MF<file>`), which the scan would
                // write: the edge's.
                if let Some(m) = f.strip_prefix("clang:-M") {
                    if matches!(m, "F" | "T" | "Q" | "J") && argv.get(i).is_some_and(|v| v.starts_with("/clang:") || v.starts_with("-clang:")) {
                        i += 1;
                    }
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
            if matches!(a.as_str(), "-c" | "-M" | "-MM" | "-MD" | "-MMD" | "-MP" | "-MG")
                || a.starts_with("-MF")
                || a.starts_with("-MT")
                || a.starts_with("-MQ")
                || a.starts_with("-MJ")
                || (a.starts_with("-o") && a.len() > 2)
            {
                continue;
            }
            args.extend(without_wp_deps(a));
            continue;
        }
        args.push(a.clone());
    }
    // Flags go before an end of the options (`--`, as CMake writes before
    // the source).
    let options_end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    if cl {
        args.splice(options_end..options_end, ["/E".to_string(), "/showIncludes".to_string()]);
        // The preprocessed text (on stdout) isn't read.
        let mut cmd = Command::new(program);
        cmd.args(&args).current_dir(cwd).stdout(Stdio::null()).stderr(Stdio::piped());
        let out = run(cmd, program)?;
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
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    args.splice(options_end..options_end, ["-E", "-MD", "-MF", "-", "-o", null].map(String::from));
    let mut cmd = Command::new(program);
    cmd.args(&args).current_dir(cwd).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = run(cmd, program)?;
    let deps = absolute(make_deps(&String::from_utf8_lossy(&out.stdout)), cwd);
    // (A compile reads its source at least: one that lists nothing didn't
    // run as asked.)
    if deps.is_empty() {
        return Err(format!("{program} listed no dependencies"));
    }
    Ok(deps)
}

/// Splits a Windows command line into arguments following the
/// `CommandLineToArgvW` / MSVC CRT rules. `first_is_program` applies the
/// special rule for argv[0] (no backslash escapes, quotes just toggle).
pub fn split_windows(cmd: &str, first_is_program: bool) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut args = Vec::new();
    let mut i = 0;
    let n = chars.len();
    let skip_ws = |i: &mut usize| {
        while *i < n && (chars[*i] == ' ' || chars[*i] == '\t' || chars[*i] == '\n' || chars[*i] == '\r') {
            *i += 1;
        }
    };
    skip_ws(&mut i);
    if first_is_program && i < n {
        let mut arg = String::new();
        let mut in_quotes = false;
        while i < n {
            let c = chars[i];
            if c == '"' {
                in_quotes = !in_quotes;
            } else if !in_quotes && (c == ' ' || c == '\t') {
                break;
            } else {
                arg.push(c);
            }
            i += 1;
        }
        args.push(arg);
        skip_ws(&mut i);
    }
    while i < n {
        let mut arg = String::new();
        let mut in_quotes = false;
        while i < n {
            let c = chars[i];
            if c == '\\' {
                let mut backslashes = 0;
                while i < n && chars[i] == '\\' {
                    backslashes += 1;
                    i += 1;
                }
                if i < n && chars[i] == '"' {
                    for _ in 0..backslashes / 2 {
                        arg.push('\\');
                    }
                    if backslashes % 2 == 1 {
                        arg.push('"');
                        i += 1;
                    }
                } else {
                    for _ in 0..backslashes {
                        arg.push('\\');
                    }
                }
                continue;
            }
            if c == '"' {
                if in_quotes && i + 1 < n && chars[i + 1] == '"' {
                    // "" inside quotes is a literal quote (post-2008 CRT rule).
                    arg.push('"');
                    i += 2;
                    continue;
                }
                in_quotes = !in_quotes;
                i += 1;
                continue;
            }
            if !in_quotes && (c == ' ' || c == '\t' || c == '\n' || c == '\r') {
                break;
            }
            arg.push(c);
            i += 1;
        }
        args.push(arg);
        skip_ws(&mut i);
    }
    args
}

/// Characters that make a POSIX command need a real shell.
fn is_shell_meta(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '*' | '?' | '[' | ']' | '{' | '}' | '~' | '#' | '!' | '\n')
}

/// Splits a POSIX shell command into words. Returns `None` if the command
/// uses shell features (pipes, redirections, expansions, globs, ...) that
/// would need a real shell.
pub fn split_posix(cmd: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if have {
                    args.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            '\'' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return None,
                    }
                }
            }
            '"' => {
                have = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => cur.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                cur.push('\\');
                                cur.push(c);
                            }
                            None => return None,
                        },
                        Some('$') | Some('`') => return None,
                        Some(c) => cur.push(c),
                        None => return None,
                    }
                }
            }
            '\\' => {
                have = true;
                match chars.next() {
                    Some('\n') => {}
                    Some(c) => cur.push(c),
                    None => return None,
                }
            }
            c if is_shell_meta(c) => {
                // '#' only starts a comment at the beginning of a word; '~'
                // only expands at the beginning; '=' etc. are fine.
                if (c == '#' || c == '~') && have {
                    cur.push(c);
                } else {
                    return None;
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        args.push(cur);
    }
    Some(args)
}

/// A command line (a compilation database's `command`) split as builds
/// split a command's (ninja-remote's `cmdline`): empty when it needs a
/// shell.
pub fn split_command(cmd: &str) -> Vec<String> {
    if cfg!(windows) { split_windows(cmd, true) } else { split_posix(cmd).unwrap_or_default() }
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
        assert_eq!(split_windows(r#"cl /DA=\"x y\" "C:\dir b\c.cpp" -Dz\\"#, false), s(&["cl", r#"/DA="x"#, r#"y""#, r"C:\dir b\c.cpp", r"-Dz\\"]));
        assert_eq!(split_posix(r#"cc -DA=\"x\" 'a b.c' "c d" e\ f"#), Some(s(&["cc", r#"-DA="x""#, "a b.c", "c d", "e f"])));
    }

    #[test]
    fn windows_split() {
        assert_eq!(split_windows(r#""C:\Program Files\x.exe" -a "b c" d\"e"#, true), s(&[r"C:\Program Files\x.exe", "-a", "b c", "d\"e"]));
        assert_eq!(split_windows(r#"a\\\"b "c\\" d"#, false), s(&[r#"a\"b"#, r"c\", "d"]));
        assert_eq!(split_windows(r"C:\a\b.exe /FoC:\x\y.obj", true), s(&[r"C:\a\b.exe", r"/FoC:\x\y.obj"]));
        assert_eq!(split_windows(r#"x "a""b""#, true), s(&["x", "a\"b"]));
    }

    #[test]
    fn posix_split() {
        assert_eq!(split_posix("cc -c 'a b.c' -o \"x y.o\" -DX=\\\"1\\\""), Some(s(&["cc", "-c", "a b.c", "-o", "x y.o", "-DX=\"1\""])));
        assert_eq!(split_posix("cc a.c > out"), None);
        assert_eq!(split_posix("cc $FLAGS"), None);
        assert_eq!(split_posix("cc -o a#b x~y"), Some(s(&["cc", "-o", "a#b", "x~y"])));
    }

    #[test]
    fn preprocessor_dependency_options() {
        assert_eq!(without_wp_deps("-Wp,-MD,x.d"), None);
        assert_eq!(without_wp_deps("-Wp,-MMD,x.d,-DX,-MP"), Some("-Wp,-DX".to_string()));
        assert_eq!(without_wp_deps("-O2"), Some("-O2".to_string()));
    }

    #[test]
    fn cl_style() {
        assert!(is_cl_style(&s(&["C:\\VC\\bin\\cl.exe", "/c", "a.cpp"])));
        assert!(is_cl_style(&s(&["clang-cl", "/c", "a.cpp"])));
        assert!(is_cl_style(&s(&["clang", "--driver-mode=cl", "/c", "a.cpp"])));
        assert!(!is_cl_style(&s(&["clang++", "-c", "a.cpp"])));
    }

    /// clang-cl's dependency file flags (CMake's) aren't given to the
    /// scanning run, which would write the edge's dependency file.
    #[test]
    fn cl_dependency_file_flags() {
        let dir = test_dirs::TestDir::new("cpp_deps_external_cl_flags");
        let compiler = crate::test_tool(&dir, "clang-cl", "@echo %*> \"%~dp0args.txt\"\r\n", "echo \"$@\" > \"$(dirname \"$0\")/args.txt\"\n");
        std::fs::write(dir.join("main.cc"), "").unwrap();
        let argv =
            s(&[&compiler.to_string_lossy(), "/nologo", "-clang:-MD", "-clang:-MTx.obj", "/clang:-MF", "/clang:x.obj.d", "/Fox.obj", "-c", "--", "main.cc"]);
        assert_eq!(compiler_deps(&argv, &dir, None).unwrap(), vec![dir.join("main.cc")]);
        let args = std::fs::read_to_string(dir.join("args.txt")).unwrap();
        assert_eq!(args.split_whitespace().collect::<Vec<_>>(), ["/nologo", "/E", "/showIncludes", "--", "main.cc"]);
    }

    /// A compiler that runs fine but lists nothing (one that ignores `-M`):
    /// no list, rather than one without the compile's headers.
    #[test]
    fn nothing_listed() {
        let dir = test_dirs::TestDir::new("cpp_deps_external_nothing");
        let compiler = crate::test_tool(&dir, "cc", "@exit /b 0\r\n", "exit 0\n");
        std::fs::write(dir.join("main.c"), "#include \"a.h\"\n").unwrap();
        let e = compiler_deps(&s(&[&compiler.to_string_lossy(), "-c", "main.c"]), &dir, None).unwrap_err();
        assert!(e.contains("listed no dependencies"), "{e}");
    }

    /// Against real compilers: `CPP_DEPS_TEST_CXX`, a GCC or Clang (with
    /// clang-scan-deps next to it).
    #[test]
    #[cfg_attr(not(cpp_deps_test_cxx), ignore = "needs CPP_DEPS_TEST_CXX, a GCC or Clang")]
    fn real_compiler() {
        let cxx = crate::test_compiler("CPP_DEPS_TEST_CXX");
        let dir = test_dirs::TestDir::new("cpp_deps_external");
        std::fs::write(dir.join("a.h"), "#define A 1\n").unwrap();
        std::fs::write(dir.join("main.cc"), "#include \"a.h\"\nint main() { return A; }\n").unwrap();
        let argv = vec![cxx.to_string_lossy().into_owned(), "-MD".into(), "-MF".into(), "x.d".into(), "-c".into(), "main.cc".into(), "-o".into(), "x.o".into()];
        let want = vec![dir.join("main.cc"), dir.join("a.h")];
        // (Without what GCC reads first, `stdc-predef.h`.)
        let deps = |argv: &[String]| compiler_deps(argv, &dir, None).unwrap().into_iter().filter(|f| f.starts_with(&dir)).collect::<Vec<_>>();
        assert_eq!(deps(&argv), want);
        if Path::new(&cxx).with_file_name(if cfg!(windows) { "clang-scan-deps.exe" } else { "clang-scan-deps" }).exists() {
            assert_eq!(clang_scan_deps(&argv, &dir).unwrap(), want);
        }
        assert!(!dir.join("x.d").exists() && !dir.join("x.o").exists());
        // Linux's dependency files; macros expanded in text as the compile
        // does (`__COUNTER__`).
        std::fs::write(dir.join("counter.cc"), "int x = __COUNTER__;\n#if __COUNTER__ == 1\n#include \"a.h\"\n#endif\n").unwrap();
        let argv = vec![cxx.to_string_lossy().into_owned(), "-Wp,-MMD,y.d".into(), "-c".into(), "counter.cc".into(), "-o".into(), "y.o".into()];
        assert_eq!(deps(&argv), vec![dir.join("counter.cc"), dir.join("a.h")]);
        assert!(!dir.join("y.d").exists() && !dir.join("y.o").exists());
    }

    /// Against cl.exe: `CPP_DEPS_TEST_CL`.
    #[test]
    #[cfg_attr(not(cpp_deps_test_cl), ignore = "needs CPP_DEPS_TEST_CL, a cl.exe with INCLUDE set")]
    fn real_cl() {
        let cl = crate::test_compiler("CPP_DEPS_TEST_CL");
        let dir = test_dirs::TestDir::new("cpp_deps_external_cl");
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
    }
}
