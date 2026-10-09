//! Command line tokenizing and unwrapping: `cmd.exe /C "..."` wrappers,
//! `&&` chains and the shell no-ops generators put around commands (`:`,
//! `true`, `cd .`), see docs/rbe-interface.md.

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

/// Quotes an argument for a Windows command line.
pub fn quote_windows(arg: &str) -> String {
    let mut out = String::new();
    ninja_core::shell::win32_escape(arg, &mut out);
    if arg.is_empty() {
        out = "\"\"".into();
    }
    out
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
/// otherwise (pipes, redirections, variables) are an error.
pub fn unwrap_command(command: &str, windows: bool) -> Result<Unwrapped, String> {
    let mut cmd = command.trim().to_string();
    if windows && let Some(rest) = strip_cmd_exe(&cmd) {
        cmd = rest;
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
            split_windows(&seg, true)
        } else {
            split_posix(&seg).ok_or_else(|| format!("command needs a shell: {seg}"))?
        };
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
    if windows {
        split_windows(content, false)
    } else {
        split_gnu_rsp(content)
    }
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
            vec![
                s(&[r"C:\Program Files\LLVM\bin\llvm-ar.exe", "qc", "greet.lib", "a.obj"]),
                s(&[r"C:\Program Files\LLVM\bin\llvm-ranlib.exe", "greet.lib"])
            ]
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
