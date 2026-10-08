//! Escaping of paths for `$in`/`$out`, ported from ninja's util.cc.

fn is_known_shell_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | '/')
}

fn is_known_win32_safe(c: char) -> bool {
    !matches!(c, ' ' | '"')
}

/// POSIX shell escaping: wraps in single quotes when needed.
pub fn shell_escape(input: &str, out: &mut String) {
    if input.chars().all(is_known_shell_safe) {
        out.push_str(input);
        return;
    }
    out.push('\'');
    for c in input.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
}

/// Windows escaping following `CommandLineToArgvW` rules.
pub fn win32_escape(input: &str, out: &mut String) {
    if input.chars().all(is_known_win32_safe) {
        out.push_str(input);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in input.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push(c);
            }
            '"' => {
                for _ in 0..backslashes + 1 {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(i: &str) -> String {
        let mut o = String::new();
        shell_escape(i, &mut o);
        o
    }
    fn w(i: &str) -> String {
        let mut o = String::new();
        win32_escape(i, &mut o);
        o
    }

    #[test]
    fn shell_escaping() {
        assert_eq!(s("foo/bar.c"), "foo/bar.c");
        assert_eq!(s("foo bar"), "'foo bar'");
        assert_eq!(s("it's"), "'it'\\''s'");
    }

    #[test]
    fn win32_escaping() {
        assert_eq!(w("foo\\bar.c"), "foo\\bar.c");
        assert_eq!(w("foo bar"), "\"foo bar\"");
        assert_eq!(w("a\"b"), "\"a\\\"b\"");
        assert_eq!(w("c:\\a b\\"), "\"c:\\a b\\\\\"");
        assert_eq!(w("a\\\"b"), "\"a\\\\\\\"b\"");
    }
}
