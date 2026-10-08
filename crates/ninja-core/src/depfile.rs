//! Makefile-style depfile parser, ported from ninja's `DepfileParser`
//! (`depfile_parser.in.cc`), with the same escaping rules as GCC/Clang.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Depfile {
    pub outs: Vec<String>,
    pub ins: Vec<String>,
}

fn is_plain(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            b'+' | b'?' | b'"' | b'\'' | b'&' | b',' | b'/' | b'_' | b':' | b'.' | b'~' | b'(' | b')' | b'}' | b'{' | b'%'
                | b'=' | b'@' | b'[' | b']' | b'!' | b'-'
        )
        || c >= 0x80
}

pub fn parse_depfile(content: &[u8]) -> Result<Depfile, String> {
    let at = |i: usize| content.get(i).copied().unwrap_or(0);
    let end = content.len();
    let mut i = 0usize;
    let mut have_target = false;
    let mut parsing_targets = true;
    let mut poisoned_input = false;
    let mut is_empty = true;
    let mut result = Depfile::default();
    while i < end {
        let mut have_newline = false;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let start = i;
            let c = at(i);
            if c == b'\\' {
                // Count the run of backslashes.
                let mut j = i;
                while at(j) == b'\\' {
                    j += 1;
                }
                let n = j - i;
                // Past the end reads as NUL, like ninja's NUL-terminated buffer.
                let next = if j < end { at(j) } else { 0 };
                let eof_next = j >= end;
                if next == b' ' {
                    if n % 2 == 1 {
                        // 2N+1 backslashes plus space -> N backslashes plus space.
                        out.extend(std::iter::repeat_n(b'\\', n / 2));
                        out.push(b' ');
                        i = j + 1;
                        continue;
                    }
                    // 2N backslashes plus space -> 2N backslashes, end of filename.
                    out.extend(std::iter::repeat_n(b'\\', n));
                    i = j + 1;
                    break;
                }
                if next == b'#' {
                    // De-escape hash sign, but preserve other leading backslashes.
                    out.extend(std::iter::repeat_n(b'\\', n - 1));
                    out.push(b'#');
                    i = j + 1;
                    continue;
                }
                if next == b':' {
                    let after = if j + 1 < end { at(j + 1) } else { 0 };
                    if matches!(after, 0 | b' ' | b'\r' | b'\n' | b'\t') {
                        // Backslashes and a colon followed by whitespace: normal
                        // text (not an escaped colon), ending the filename.
                        out.extend_from_slice(&content[start..j + 1]);
                        i = (j + 2).min(end);
                        if after == b'\n' {
                            have_newline = true;
                        }
                        break;
                    }
                    // De-escape colon sign, but preserve other leading backslashes.
                    out.extend(std::iter::repeat_n(b'\\', n - 1));
                    out.push(b':');
                    i = j + 1;
                    continue;
                }
                let newline_len = if next == b'\n' {
                    1
                } else if next == b'\r' && j + 1 < end && at(j + 1) == b'\n' {
                    2
                } else {
                    0
                };
                if n == 1 && newline_len > 0 {
                    // A line continuation ends the current file name.
                    i = j + newline_len;
                    break;
                }
                if n >= 2 && (eof_next || next == b'\r' || next == b'\n') {
                    // `\\+ [^\0\r\n]` matches all backslashes as plain text.
                    out.extend(std::iter::repeat_n(b'\\', n));
                    i = j;
                    continue;
                }
                if n == 1 && (eof_next || next == b'\r') {
                    // A lone backslash before EOF or a bare CR is swallowed.
                    i = j;
                    break;
                }
                // Backslashes followed by any other character: plain text.
                out.extend_from_slice(&content[start..j + 1]);
                i = j + 1;
                continue;
            } else if c == b'$' && at(i + 1) == b'$' && i + 1 < end {
                out.push(b'$');
                i += 2;
                continue;
            } else if is_plain(c) && i < end {
                let mut j = i;
                while j < end && is_plain(at(j)) {
                    j += 1;
                }
                out.extend_from_slice(&content[i..j]);
                i = j;
                continue;
            } else if i >= end {
                break;
            } else if c == b'\n' {
                have_newline = true;
                i += 1;
                break;
            } else if c == b'\r' && at(i + 1) == b'\n' {
                have_newline = true;
                i += 2;
                break;
            } else {
                // Any other character (e.g. whitespace): swallow it.
                i += 1;
                break;
            }
        }

        let mut len = out.len();
        let is_dependency = !parsing_targets;
        if len > 0 && out[len - 1] == b':' {
            len -= 1;
            parsing_targets = false;
            have_target = true;
        }
        if len > 0 {
            is_empty = false;
            let piece = String::from_utf8_lossy(&out[..len]).into_owned();
            if !result.ins.contains(&piece) {
                if is_dependency {
                    if poisoned_input {
                        return Err("inputs may not also have inputs".into());
                    }
                    result.ins.push(piece);
                } else if !result.outs.contains(&piece) {
                    result.outs.push(piece);
                }
            } else if !is_dependency {
                // We've passed an input on the left side; reject new inputs.
                poisoned_input = true;
            }
        }
        if have_newline {
            parsing_targets = true;
            poisoned_input = false;
        }
    }
    if !have_target && !is_empty {
        return Err("expected ':' in depfile".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Depfile {
        parse_depfile(s.as_bytes()).unwrap()
    }

    #[test]
    fn basic() {
        let d = p("build/ninja.o: ninja.cc ninja.h eval_env.h manifest_parser.h\n");
        assert_eq!(d.outs, vec!["build/ninja.o"]);
        assert_eq!(d.ins.len(), 4);
    }

    #[test]
    fn early_newline_and_whitespace() {
        let d = p(" \\\n  out: in\n");
        assert_eq!(d.outs, vec!["out"]);
        assert_eq!(d.ins, vec!["in"]);
    }

    #[test]
    fn continuation() {
        let d = p("foo.o: \\\n  bar.h baz.h\n");
        assert_eq!(d.outs, vec!["foo.o"]);
        assert_eq!(d.ins, vec!["bar.h", "baz.h"]);
    }

    #[test]
    fn windows_drive_paths() {
        let d = p("foo.o: //?/c:/bar.h\n");
        assert_eq!(d.ins, vec!["//?/c:/bar.h"]);
        let d = p("c:/foo.o: c:/bar.h\n");
        assert_eq!(d.outs, vec!["c:/foo.o"]);
        assert_eq!(d.ins, vec!["c:/bar.h"]);
    }

    #[test]
    fn carriage_return_continuation() {
        let d = p("foo.o: \\\r\n  bar.h baz.h\r\n");
        assert_eq!(d.outs, vec!["foo.o"]);
        assert_eq!(d.ins, vec!["bar.h", "baz.h"]);
    }

    #[test]
    fn back_slashes() {
        let d = p("Project\\Dir\\Build\\Release8\\Foo\\Foo.res : \\\n  Dir\\Library\\Foo.rc \\\n  Dir\\Library\\Version\\Bar.h \\\n  Dir\\Library\\Foo.ico \\\n  Project\\Thing\\Bar.tlb \\\n");
        assert_eq!(d.outs, vec!["Project\\Dir\\Build\\Release8\\Foo\\Foo.res"]);
        assert_eq!(d.ins.len(), 4);
    }

    #[test]
    fn spaces() {
        let d = p("a\\ bc\\ def:   a\\ b c d");
        assert_eq!(d.outs, vec!["a bc def"]);
        assert_eq!(d.ins, vec!["a b", "c", "d"]);
    }

    #[test]
    fn multiple_backslashes() {
        // Successive 2N+1 backslashes followed by space (' ') are replaced by
        // N >= 0 backslashes and the space. A single backslash before hash
        // sign is removed. Other backslashes remain untouched (including 2N
        // backslashes followed by space).
        let d = p("a\\ b\\#c.h: \\\\\\\\\\  \\\\\\\\ \\\\share\\info\\\\#1");
        assert_eq!(d.outs, vec!["a b#c.h"]);
        assert_eq!(d.ins, vec!["\\\\ ", "\\\\\\\\", "\\\\share\\info\\#1"]);
    }

    #[test]
    fn escapes() {
        // Put backslashes before a variety of characters, see which ones make
        // it through.
        let d = p("\\!\\@\\#$$\\%\\^\\&\\[\\]\\\\:");
        assert_eq!(d.outs, vec!["\\!\\@#$\\%\\^\\&\\[\\]\\\\"]);
        assert!(d.ins.is_empty());
    }

    #[test]
    fn escaped_colons() {
        let d = p("c\\:\\gcc\\x86_64-w64-mingw32\\include\\stddef.o: \\\n c:\\gcc\\x86_64-w64-mingw32\\include\\stddef.h \n");
        assert_eq!(d.outs, vec!["c:\\gcc\\x86_64-w64-mingw32\\include\\stddef.o"]);
        assert_eq!(d.ins, vec!["c:\\gcc\\x86_64-w64-mingw32\\include\\stddef.h"]);
    }

    #[test]
    fn escaped_target_colon() {
        let d = p("foo1\\: x\nfoo1\\:\nfoo1\\:\r\nfoo1\\:\t\nfoo1\\:");
        assert_eq!(d.outs, vec!["foo1\\"]);
        assert_eq!(d.ins, vec!["x"]);
    }

    #[test]
    fn special_chars() {
        let d = p("C:/Program\\ Files\\ (x86)/Microsoft\\ crtdefs.h: \\\n en@quot.header~ t+t-x!=1 \\\n openldap/slapd.d/cn=config/cn=schema/cn={0}core.ldif\\\n Fu\u{e4}ball\\\n a[1]b@2%c");
        assert_eq!(d.outs, vec!["C:/Program Files (x86)/Microsoft crtdefs.h"]);
        assert_eq!(
            d.ins,
            vec!["en@quot.header~", "t+t-x!=1", "openldap/slapd.d/cn=config/cn=schema/cn={0}core.ldif", "Fu\u{e4}ball", "a[1]b@2%c"]
        );
    }

    #[test]
    fn unify_multiple_outputs() {
        // check that multiple duplicate targets are properly unified
        let d = p("foo foo: x y z");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn multiple_different_outputs() {
        let d = p("foo bar: x y z");
        assert_eq!(d.outs, vec!["foo", "bar"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn multiple_empty_rules() {
        let d = p("foo: x\nfoo: \nfoo:\n");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x"]);
    }

    #[test]
    fn unify_multiple_rules_lf() {
        let d = p("foo: x\nfoo: y\nfoo \\\nfoo: z\n");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn unify_multiple_rules_crlf() {
        let d = p("foo: x\r\nfoo: y\r\nfoo \\\r\nfoo: z\r\n");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn unify_mixed_rules() {
        let d = p("foo: x\\\n     y\nfoo \\\nfoo: z\n");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn indented_rules_lf() {
        let d = p(" foo: x\n foo: y\n foo: z\n");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn tolerate_mp() {
        let d = p("foo: x y z\nx:\ny:\nz:\n");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn multiple_rules_tolerate_mp() {
        let d = p("foo: x\nx:\nfoo: y\ny:\nfoo: z\nz:\n");
        assert_eq!(d.outs, vec!["foo"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn multiple_rules_different_outputs() {
        // check that multiple different outputs are accepted by the parser
        // when spread across multiple rules
        let d = p("foo: x y\nbar: y z\n");
        assert_eq!(d.outs, vec!["foo", "bar"]);
        assert_eq!(d.ins, vec!["x", "y", "z"]);
    }

    #[test]
    fn buggy_mp() {
        assert_eq!(parse_depfile(b"foo: x y z\nx: alsoin\ny:\nz:\n").unwrap_err(), "inputs may not also have inputs");
    }

    #[test]
    fn empty_file() {
        assert_eq!(p(""), Depfile::default());
    }

    #[test]
    fn empty_lines() {
        assert_eq!(p("\n\n"), Depfile::default());
    }

    #[test]
    fn missing_colon() {
        // The file is not empty but is missing a colon separator.
        assert_eq!(parse_depfile(b"foo.o foo.c\n").unwrap_err(), "expected ':' in depfile");
    }
}
