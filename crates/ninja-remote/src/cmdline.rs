//! Command line tokenizing and unwrapping: `cmd.exe /C "..."` wrappers,
//! `&&` chains and the shell no-ops generators put around commands (`:`,
//! `true`, `cd .`), see docs/rbe-interface.md.

/// The scanner's tokenizers: its tools split command lines as builds do.
pub use cpp_deps::external::{split_posix, split_windows};

/// Quotes an argument for a Windows command line.
pub fn quote_windows(arg: &str) -> String {
    let mut out = String::new();
    ninja_core::shell::win32_escape(arg, &mut out);
    if arg.is_empty() {
        out = "\"\"".into();
    }
    out
}

/// Quotes a word for a POSIX shell.
pub fn quote_posix(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "_+-./=:,@%^".contains(c)) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Splits on `&&` outside of quotes.
pub fn split_and_chain(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_double = false;
    let mut in_single = false;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' && !in_single {
            in_double = !in_double;
        } else if c == '\'' && !in_double && !cfg_windows_quotes() {
            in_single = !in_single;
        }
        if c == '&' && !in_double && !in_single && i + 1 < chars.len() && chars[i + 1] == '&' {
            parts.push(cur.trim().to_string());
            cur.clear();
            i += 2;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    parts.push(cur.trim().to_string());
    parts
}

// Single quotes are not quotes for cmd.exe, but Windows command lines rarely
// contain them, so treating them as quotes keeps POSIX behavior correct.
fn cfg_windows_quotes() -> bool {
    false
}

/// Whether a chain segment is a shell no-op (`:`, `true`, `cd .`), which
/// generators put around commands (CMake for empty pre- and post-steps).
fn is_noop(seg: &str) -> bool {
    let s = seg.trim();
    s.is_empty() || s == ":" || s == "cd ." || s == "cd /D ." || s == "true"
}

/// The commands a command line runs, in order: each an argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unwrapped {
    pub steps: Vec<Vec<String>>,
}

/// Turns a command line into the argv of each command it runs: a
/// `cmd.exe /C` wrapper is removed (`windows`, the host's syntax), `&&`
/// chains are split and shell no-ops dropped. Commands that need a shell
/// otherwise (pipes, redirections, variables, a `cd` step) are an error.
pub fn unwrap_command(command: &str, windows: bool) -> Result<Unwrapped, String> {
    let mut cmd = command.trim().to_string();
    // What cmd.exe runs: its other syntax (`&`, `^`, `%VAR%`, groups) too.
    let mut by_cmd = false;
    if windows && let Some(rest) = strip_cmd_exe(&cmd) {
        cmd = rest;
        by_cmd = true;
    }
    let mut steps: Vec<Vec<String>> = Vec::new();
    for seg in split_and_chain(&cmd) {
        if is_noop(&seg) {
            continue;
        }
        let argv = if windows {
            if seg.contains(['|', '<', '>']) && !quoted_only_meta(&seg) {
                return Err(format!("command uses shell redirection: {seg}"));
            }
            if by_cmd && let Some(what) = cmd_syntax(&seg) {
                return Err(format!("command uses cmd.exe's {what}: {seg}"));
            }
            split_windows(&seg, true)
        } else {
            split_posix(&seg).ok_or_else(|| format!("command needs a shell: {seg}"))?
        };
        // The steps after it would run elsewhere.
        if argv.first().is_some_and(|p| p == "cd" || (windows && (p.eq_ignore_ascii_case("cd") || p.eq_ignore_ascii_case("chdir")))) {
            return Err(format!("command changes directory: {seg}"));
        }
        if !argv.is_empty() {
            steps.push(argv);
        }
    }
    if steps.is_empty() {
        return Err("empty command".into());
    }
    Ok(Unwrapped { steps })
}

/// Joins commands into one POSIX shell command line (`a && b`).
pub fn join_posix_chain(steps: &[Vec<String>]) -> String {
    let cmds: Vec<String> = steps.iter().map(|argv| argv.iter().map(|a| quote_posix(a)).collect::<Vec<_>>().join(" ")).collect();
    cmds.join(" && ")
}

/// For `[path\]cmd[.exe] [/S] /C <rest>` (any case), returns `<rest>` with
/// the outer quotes removed the way cmd.exe does.
fn strip_cmd_exe(cmd: &str) -> Option<String> {
    let s = cmd.trim_start();
    let (program, rest) = if let Some(r) = s.strip_prefix('"') {
        let end = r.find('"')?;
        (&r[..end], &r[end + 1..])
    } else {
        let end = s.find([' ', '\t']).unwrap_or(s.len());
        (&s[..end], &s[end..])
    };
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program).to_ascii_lowercase();
    if base != "cmd" && base != "cmd.exe" {
        return None;
    }
    let mut rest = rest.trim_start();
    loop {
        let lower = rest.to_ascii_lowercase();
        if lower.starts_with("/s ") || lower.starts_with("/q ") || lower.starts_with("/d ") {
            rest = rest[3..].trim_start();
        } else if lower.starts_with("/c ") {
            rest = rest[3..].trim();
            break;
        } else {
            return None;
        }
    }
    // cmd strips the outer quotes when the string starts and ends with one.
    let mut out = rest.to_string();
    if out.len() >= 2 && out.starts_with('"') && out.ends_with('"') {
        out = out[1..out.len() - 1].to_string();
    }
    Some(out)
}

/// What of cmd.exe's syntax a segment it runs uses besides `&&`, which
/// splitting it into arguments would pass on literally: `&`, `^` and
/// parentheses outside quotes, variables (`%VAR%`, expanded in quotes
/// too).
fn cmd_syntax(seg: &str) -> Option<&'static str> {
    let mut in_q = false;
    for c in seg.chars() {
        if c == '"' {
            in_q = !in_q;
        } else if !in_q {
            match c {
                '&' => return Some("&"),
                '^' => return Some("escapes"),
                '(' | ')' => return Some("groups"),
                _ => {}
            }
        }
    }
    let mut parts = seg.split('%');
    parts.next();
    let mut parts = parts.collect::<Vec<_>>();
    // `%NAME%`: a name between two signs (the last part has no closing one).
    parts.pop();
    parts.iter().any(|name| !name.is_empty() && !name.contains([' ', '\t', '"'])).then_some("variables")
}

/// Whether every `|`, `<`, `>` in a Windows segment is inside quotes.
fn quoted_only_meta(seg: &str) -> bool {
    let mut in_q = false;
    for c in seg.chars() {
        if c == '"' {
            in_q = !in_q;
        } else if !in_q && matches!(c, '|' | '<' | '>') {
            return false;
        }
    }
    true
}

/// Tokenizes response file contents: Windows rules on Windows hosts, GNU
/// (`clang`/`gcc`) rules otherwise.
pub fn split_rsp(content: &str, windows: bool) -> Vec<String> {
    if windows { split_windows(content, false) } else { split_gnu_rsp(content) }
}

/// GNU response file tokenizing (as in LLVM's `TokenizeGNUCommandLine`).
pub fn split_gnu_rsp(content: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if have {
                    args.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            '\\' => {
                have = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' | '"' => {
                have = true;
                let q = c;
                while let Some(n) = chars.next() {
                    if n == q {
                        break;
                    }
                    if n == '\\' {
                        if let Some(e) = chars.next() {
                            cur.push(e);
                        }
                        continue;
                    }
                    cur.push(n);
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
    args
}

/// Formats arguments as response file contents.
pub fn join_rsp(args: &[String], windows: bool) -> String {
    let quoted: Vec<String> = args.iter().map(|a| if windows { quote_windows(a) } else { quote_posix(a) }).collect();
    quoted.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn steps(cmd: &str, windows: bool) -> Vec<Vec<String>> {
        unwrap_command(cmd, windows).unwrap().steps
    }

    #[test]
    fn unwrap_plain() {
        assert_eq!(steps("/usr/bin/cc -c a.c -o a.o", false), vec![s(&["/usr/bin/cc", "-c", "a.c", "-o", "a.o"])]);
    }

    #[test]
    fn unwrap_unix_chain() {
        assert_eq!(
            steps(": && /usr/bin/ar qc libfoo.a a.o b.o && /usr/bin/ranlib libfoo.a && :", false),
            vec![s(&["/usr/bin/ar", "qc", "libfoo.a", "a.o", "b.o"]), s(&["/usr/bin/ranlib", "libfoo.a"])]
        );
    }

    #[test]
    fn unwrap_windows_chain() {
        // A link as written by CMake's Ninja generator on Windows.
        assert_eq!(
            steps(r#"C:\WINDOWS\system32\cmd.exe /C "cd . && "C:\Program Files\LLVM\bin\clang++.exe" a.obj -o a.exe && cd .""#, true),
            vec![s(&[r"C:\Program Files\LLVM\bin\clang++.exe", "a.obj", "-o", "a.exe"])]
        );
        assert_eq!(
            steps(r#""C:\Program Files\LLVM\bin\llvm-ar.exe" qc greet.lib a.obj && "C:\Program Files\LLVM\bin\llvm-ranlib.exe" greet.lib"#, true),
            vec![s(&[r"C:\Program Files\LLVM\bin\llvm-ar.exe", "qc", "greet.lib", "a.obj"]), s(&[r"C:\Program Files\LLVM\bin\llvm-ranlib.exe", "greet.lib"])]
        );
        // A plain compile.
        let cc = r#""C:\Program Files\LLVM\bin\clang++.exe" -IC:/src -MD -MT a.obj -MF a.obj.d -o a.obj -c C:/src/a.cpp"#;
        assert_eq!(steps(cc, true)[0][0], r"C:\Program Files\LLVM\bin\clang++.exe");
    }

    #[test]
    fn unwrap_rejects_shell_features() {
        assert!(unwrap_command("cc a.c | tee x", false).is_err());
        assert!(unwrap_command(r#"cmd.exe /C "a.exe > x.txt""#, true).is_err());
        let scan = r#"C:\WINDOWS\system32\cmd.exe /C ""C:/LLVM/bin/clang-scan-deps.exe" -format=p1689 -- clang++.exe -c a.cpp > a.ddi.tmp && cmake.exe -E rename a.ddi.tmp a.ddi""#;
        assert!(unwrap_command(scan, true).is_err());
        assert!(unwrap_command(": && :", false).is_err());
        // Quoted metacharacters are fine.
        assert!(unwrap_command(r#"cl.exe "-DX=a>b" a.c"#, true).is_ok());
        // What else cmd.exe reads in what it runs.
        for cmd in [
            r#"cmd.exe /C "a.exe & b.exe""#,
            r#"cmd.exe /C "a.exe ^"x^"""#,
            r#"cmd.exe /C "a.exe %OUT%\x.obj""#,
            r#"cmd.exe /C "(a.exe)""#,
            r#"cmd.exe /C "a.exe "-D%X%"""#,
            r#"cmd.exe /C "cd sub && cl.exe a.c""#,
        ] {
            assert!(unwrap_command(cmd, true).is_err(), "{cmd}");
        }
        // Not when it doesn't run the command; a lone % isn't a variable.
        assert!(unwrap_command(r#"cl.exe -DX=a&b -DFMT="%d" a.c"#, true).is_ok());
        assert!(unwrap_command(r#"cmd.exe /C "cl.exe -DFMT=\"%d\" a.c""#, true).is_ok());
        // The steps after a `cd` would run elsewhere.
        assert!(unwrap_command("cd sub && cc -c a.c", false).is_err());
    }

    #[test]
    fn posix_chain() {
        let chain = vec![s(&["ar", "qc", "lib a.a", "a.o"]), s(&["ranlib", "lib a.a"])];
        assert_eq!(join_posix_chain(&chain), "ar qc 'lib a.a' a.o && ranlib 'lib a.a'");
        assert_eq!(steps(&join_posix_chain(&chain), false), chain);
    }

    #[test]
    fn rsp_split() {
        assert_eq!(split_gnu_rsp("-I'a b' \"c\\\"d\" e\\ f\n-g"), s(&["-Ia b", "c\"d", "e f", "-g"]));
        assert_eq!(split_rsp("/I\"C:\\a b\" /DX", true), s(&["/IC:\\a b", "/DX"]));
        let args = s(&["-Ia b", "x"]);
        assert_eq!(split_gnu_rsp(&join_rsp(&args, false)), args);
        assert_eq!(split_rsp(&join_rsp(&args, true), true), args);
    }
}
