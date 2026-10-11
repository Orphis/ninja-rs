//! Path canonicalization, ported from ninja's `CanonicalizePath`.
//!
//! Paths are made lexically canonical (`./` and `foo/..` removed, duplicate
//! separators collapsed). On Windows, backslashes are converted to `/` and the
//! positions of the original backslashes are recorded in `slash_bits` so that
//! the original spelling can be restored for command lines.

/// Canonicalizes `path` with the host's rules.
pub fn canonicalize_path(path: &str) -> (String, u64) {
    canonicalize_path_with(path, cfg!(windows))
}

/// Whether canonicalization would return `path` unchanged with no slash bits:
/// only '/' separators, no empty, "." or ".." components and no trailing
/// separator (a leading "/", or "//" on Windows, is kept as a prefix).
/// Most manifest paths are canonical; this check avoids rebuilding them.
fn is_canonical(path: &str, windows: bool) -> bool {
    let b = path.as_bytes();
    let mut start = if windows && b.starts_with(b"//") {
        2
    } else if b[0] == b'/' {
        1
    } else {
        0
    };
    if start == b.len() {
        // "/" or "//": left to the general code.
        return false;
    }
    loop {
        let mut end = start;
        while end < b.len() && b[end] != b'/' {
            if windows && b[end] == b'\\' {
                return false;
            }
            end += 1;
        }
        if matches!(&b[start..end], b"" | b"." | b"..") {
            return false;
        }
        if end == b.len() {
            return true;
        }
        start = end + 1;
    }
}

/// Canonicalizes `path` with Windows (`windows = true`) or POSIX rules.
pub fn canonicalize_path_with(path: &str, windows: bool) -> (String, u64) {
    if path.is_empty() {
        return (String::new(), 0);
    }
    if is_canonical(path, windows) {
        return (path.to_string(), 0);
    }
    canonicalize_general(path, windows)
}

/// The full algorithm, for paths that are not already canonical.
fn canonicalize_general(path: &str, windows: bool) -> (String, u64) {
    let is_sep = |c: u8| c == b'/' || (windows && c == b'\\');
    let b = path.as_bytes();
    let mut idx = 0;
    let mut prefix_len = 0;
    if is_sep(b[0]) {
        if windows && b.len() >= 2 && is_sep(b[1]) {
            prefix_len = 2;
        } else {
            prefix_len = 1;
        }
        idx = prefix_len;
    }
    // Kept components with the separator that followed them in the source.
    let mut stack: Vec<(&str, Option<u8>)> = Vec::new();
    let mut normal = 0usize;
    loop {
        let mut end = idx;
        while end < b.len() && !is_sep(b[end]) {
            end += 1;
        }
        let comp = &path[idx..end];
        let sep = if end < b.len() { Some(b[end]) } else { None };
        match comp {
            "" | "." => {}
            ".." => {
                if normal > 0 {
                    stack.pop();
                    normal -= 1;
                } else {
                    stack.push((comp, sep));
                }
            }
            _ => {
                stack.push((comp, sep));
                normal += 1;
            }
        }
        if end >= b.len() {
            break;
        }
        idx = end + 1;
    }
    let mut out: Vec<u8> = Vec::with_capacity(path.len());
    out.extend_from_slice(&b[..prefix_len]);
    for (i, (comp, sep)) in stack.iter().enumerate() {
        out.extend_from_slice(comp.as_bytes());
        if i + 1 < stack.len() {
            out.push(sep.unwrap_or(b'/'));
        }
    }
    if out.is_empty() {
        out.push(b'.');
    }
    let mut bits = 0u64;
    if windows {
        let mut mask = 1u64;
        for c in out.iter_mut() {
            if *c == b'\\' {
                bits |= mask;
                *c = b'/';
                mask = mask.wrapping_shl(1);
            } else if *c == b'/' {
                mask = mask.wrapping_shl(1);
            }
        }
        if out.len() >= 2 && out[0] == b'/' && out[1] == b'/' && (bits & 3) == 3 {
            out[0] = b'\\';
            out[1] = b'\\';
        }
    }
    // The input was valid UTF-8 and only ASCII bytes were replaced.
    (String::from_utf8(out).expect("utf-8 preserved"), bits)
}

/// Restores the original separators of a canonical path (Windows only).
pub fn decanonicalize(path: &str, slash_bits: u64) -> String {
    decanonicalize_with(path, slash_bits, cfg!(windows))
}

pub fn decanonicalize_with(path: &str, slash_bits: u64, windows: bool) -> String {
    if !windows || slash_bits == 0 {
        return path.to_string();
    }
    let mut mask = 1u64;
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c == '/' {
            out.push(if slash_bits & mask != 0 { '\\' } else { '/' });
            mask = mask.wrapping_shl(1);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(p: &str) -> String {
        canonicalize_path_with(p, false).0
    }
    fn w(p: &str) -> (String, u64) {
        canonicalize_path_with(p, true)
    }

    #[test]
    fn fast_path_agrees_with_general_algorithm() {
        let samples = [
            "foo.h",
            "foo/bar.h",
            "a/bcd/efh/foo.h",
            "/foo",
            "/foo/bar",
            "//foo",
            "//foo/bar",
            "C:/foo/bar.h",
            "foo/.hidden",
            "foo/.._bar",
            "../foo",
            "./foo",
            "foo/./bar",
            "foo//bar",
            "foo/",
            "/",
            "//",
            ".",
            "..",
            "foo\\bar",
            "\\\\foo",
            "C:\\foo",
            "a/b/../c",
            "x.y/z.w",
        ];
        for windows in [false, true] {
            for p in samples {
                if is_canonical(p, windows) {
                    assert_eq!(canonicalize_general(p, windows), (p.to_string(), 0), "{p} (windows={windows})");
                }
            }
        }
        assert!(is_canonical("a/bcd/efh/foo.h", true));
        assert!(!is_canonical("a/./b", true));
        assert!(!is_canonical("a\\b", true));
        assert!(is_canonical("a\\b", false));
    }

    #[test]
    fn path_samples() {
        assert_eq!(c(""), "");
        assert_eq!(c("foo.h"), "foo.h");
        assert_eq!(c("./foo.h"), "foo.h");
        assert_eq!(c("./foo/./bar.h"), "foo/bar.h");
        assert_eq!(c("./x/foo/../bar.h"), "x/bar.h");
        assert_eq!(c("./x/foo/../../bar.h"), "bar.h");
        assert_eq!(c("foo//bar"), "foo/bar");
        assert_eq!(c("foo//.//..///bar"), "bar");
        assert_eq!(c("./x/../foo/../../bar.h"), "../bar.h");
        assert_eq!(c("foo/./."), "foo");
        assert_eq!(c("foo/bar/.."), "foo");
        assert_eq!(c("foo/.hidden_bar"), "foo/.hidden_bar");
        assert_eq!(c("/foo"), "/foo");
        assert_eq!(c("//foo"), "/foo");
        assert_eq!(c(".."), "..");
        assert_eq!(c("../"), "..");
        assert_eq!(c("../foo"), "../foo");
        assert_eq!(c("../foo/"), "../foo");
        assert_eq!(c("../.."), "../..");
        assert_eq!(c("../../"), "../..");
        assert_eq!(c("./../"), "..");
        assert_eq!(c("/.."), "/..");
        assert_eq!(c("/../"), "/..");
        assert_eq!(c("/../.."), "/../..");
        assert_eq!(c("/../../"), "/../..");
        assert_eq!(c("/"), "/");
        assert_eq!(c("/foo/.."), "/");
        assert_eq!(c("."), ".");
        assert_eq!(c("./."), ".");
        assert_eq!(c("foo/.."), ".");
        assert_eq!(c("foo/.._bar"), "foo/.._bar");
        // Backslashes are ordinary characters on POSIX.
        assert_eq!(c("foo\\bar"), "foo\\bar");
    }

    #[test]
    fn path_samples_windows() {
        assert_eq!(w(".\\foo.h").0, "foo.h");
        assert_eq!(w(".\\foo\\.\\bar.h").0, "foo/bar.h");
        assert_eq!(w(".\\x\\foo\\..\\bar.h").0, "x/bar.h");
        assert_eq!(w(".\\x\\foo\\..\\..\\bar.h").0, "bar.h");
        assert_eq!(w("foo\\\\bar").0, "foo/bar");
        assert_eq!(w("foo\\\\.\\\\..\\\\\\bar").0, "bar");
        assert_eq!(w(".\\x\\..\\foo\\..\\..\\bar.h").0, "../bar.h");
        assert_eq!(w("foo\\.\\.").0, "foo");
        assert_eq!(w("foo\\bar\\..").0, "foo");
        assert_eq!(w("foo\\.hidden_bar").0, "foo/.hidden_bar");
        assert_eq!(w("\\foo").0, "/foo");
        assert_eq!(w("\\\\foo").0, "\\\\foo");
        assert_eq!(w("\\").0, "/");
        assert_eq!(w("C:\\foo\\..\\bar").0, "C:/bar");
        assert_eq!(w("C:/x/../../y").0, "y");
    }

    #[test]
    fn slash_tracking() {
        assert_eq!(w("foo.h"), ("foo.h".into(), 0));
        assert_eq!(w("a\\foo.h"), ("a/foo.h".into(), 1));
        assert_eq!(w("a/bcd/efh\\foo.h"), ("a/bcd/efh/foo.h".into(), 4));
        assert_eq!(w("a\\bcd/efh\\foo.h"), ("a/bcd/efh/foo.h".into(), 5));
        assert_eq!(w("a\\bcd\\efh\\foo.h"), ("a/bcd/efh/foo.h".into(), 7));
        assert_eq!(w("a/bcd/efh/foo.h"), ("a/bcd/efh/foo.h".into(), 0));
        assert_eq!(w("a\\./efh\\foo.h"), ("a/efh/foo.h".into(), 3));
        assert_eq!(w("a\\../efh\\foo.h"), ("efh/foo.h".into(), 1));
        assert_eq!(w("a\\b\\c\\d\\e\\f\\g\\foo.h"), ("a/b/c/d/e/f/g/foo.h".into(), 127));
        assert_eq!(w("a\\b\\c\\..\\..\\..\\g\\foo.h"), ("g/foo.h".into(), 1));
        assert_eq!(w("a\\b/c\\../../..\\g\\foo.h"), ("g/foo.h".into(), 1));
        assert_eq!(w("a\\b/c\\./../..\\g\\foo.h"), ("a/g/foo.h".into(), 3));
        assert_eq!(w("a\\b/c\\./../..\\g/foo.h"), ("a/g/foo.h".into(), 1));
        assert_eq!(w("a\\\\\\foo.h"), ("a/foo.h".into(), 1));
        assert_eq!(w("a/\\\\foo.h"), ("a/foo.h".into(), 0));
        assert_eq!(w("a\\//foo.h"), ("a/foo.h".into(), 1));
    }

    #[test]
    fn decanonicalize_roundtrip() {
        let (p, bits) = w("a\\bcd/efh\\foo.h");
        assert_eq!(decanonicalize_with(&p, bits, true), "a\\bcd/efh\\foo.h");
        assert_eq!(decanonicalize_with(&p, bits, false), "a/bcd/efh/foo.h");
    }
}
