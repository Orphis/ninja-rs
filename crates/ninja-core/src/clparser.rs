//! Parser for MSVC/clang-cl `/showIncludes` output (`deps = msvc`), ported
//! from ninja's `CLParser` and `IncludesNormalize`.

use std::collections::BTreeSet;

use crate::canon::canonicalize_path_with;

pub const DEFAULT_DEPS_PREFIX: &str = "Note: including file:";

/// Returns the included path if `line` is a /showIncludes line.
pub fn filter_show_includes<'a>(line: &'a str, deps_prefix: &str) -> Option<&'a str> {
    let prefix = if deps_prefix.is_empty() { DEFAULT_DEPS_PREFIX } else { deps_prefix };
    let rest = line.strip_prefix(prefix)?;
    Some(rest.trim_start_matches(' '))
}

/// Whether the path is a system include (filtered like ninja does).
pub fn is_system_include(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("program files") || lower.contains("microsoft visual studio")
}

/// cl.exe echoes the input file name first; drop it.
pub fn filter_input_filename(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [".c", ".cc", ".cxx", ".cpp", ".c++"].iter().any(|ext| lower.ends_with(ext))
}

fn split_components(p: &str) -> Vec<&str> {
    p.split('/').filter(|s| !s.is_empty()).collect()
}

fn drive_of(p: &str) -> Option<String> {
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return Some(p[..1].to_ascii_lowercase());
    }
    None
}

/// Normalizes an include path like ninja's `IncludesNormalize`: canonical,
/// and relative to `cwd` when on the same drive (case-insensitive).
pub fn normalize_include(input: &str, cwd: &str, windows: bool) -> String {
    let (canon, _) = canonicalize_path_with(input, windows);
    // On POSIX ninja only canonicalizes.
    let is_abs = windows && (drive_of(&canon).is_some() || canon.starts_with('/'));
    if !is_abs {
        return canon;
    }
    let (cwd_canon, _) = canonicalize_path_with(cwd, windows);
    if windows && drive_of(&canon) != drive_of(&cwd_canon) {
        return canon;
    }
    let a = split_components(&canon);
    let b = split_components(&cwd_canon);
    let eq = |x: &str, y: &str| if windows { x.eq_ignore_ascii_case(y) } else { x == y };
    let mut common = 0;
    while common < a.len() && common < b.len() && eq(a[common], b[common]) {
        common += 1;
    }
    let mut parts: Vec<&str> = Vec::new();
    for _ in common..b.len() {
        parts.push("..");
    }
    parts.extend_from_slice(&a[common..]);
    if parts.is_empty() {
        return ".".into();
    }
    parts.join("/")
}

#[derive(Debug, Default)]
pub struct ClParseResult {
    pub includes: BTreeSet<String>,
    pub filtered_output: String,
}

/// Parses compiler output: extracts includes and returns the output with
/// /showIncludes lines (and the echoed source file name) removed.
pub fn parse_cl_output(output: &str, deps_prefix: &str, cwd: &str, windows: bool) -> ClParseResult {
    let mut res = ClParseResult::default();
    let mut seen_show_includes = false;
    let bytes = output.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'\r' && bytes[end] != b'\n' {
            end += 1;
        }
        let line = &output[start..end];
        if let Some(include) = filter_show_includes(line, deps_prefix).filter(|s| !s.is_empty()) {
            seen_show_includes = true;
            let normalized = normalize_include(include, cwd, windows);
            if !is_system_include(&normalized) {
                res.includes.insert(normalized);
            }
        } else if !seen_show_includes && filter_input_filename(line) {
            // Drop it.
        } else {
            res.filtered_output.push_str(line);
            res.filtered_output.push('\n');
        }
        if end < bytes.len() && bytes[end] == b'\r' {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'\n' {
            end += 1;
        }
        start = end;
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_includes() {
        assert_eq!(filter_show_includes("", ""), None);
        assert_eq!(filter_show_includes("Sample compiler output", ""), None);
        assert_eq!(filter_show_includes("Note: including file: c:\\Some Files\\foobar.h", ""), Some("c:\\Some Files\\foobar.h"));
        assert_eq!(filter_show_includes("Note: including file:    c:\\initspaces.h", ""), Some("c:\\initspaces.h"));
        assert_eq!(filter_show_includes("Non-default prefix: inc file:    c:\\initspaces.h", "Non-default prefix: inc file:"), Some("c:\\initspaces.h"));
    }

    #[test]
    fn filter_input_filename_test() {
        assert!(filter_input_filename("foobar.cc"));
        assert!(filter_input_filename("foo bar.cc"));
        assert!(filter_input_filename("baz.c"));
        assert!(filter_input_filename("FOOBAR.CC"));
        assert!(!filter_input_filename("src\\cl_helper.cc(166) : fatal error C1075: end of file found ..."));
    }

    #[test]
    fn parse_simple() {
        let r = parse_cl_output("foo\r\nNote: inc file prefix:  foo.h\r\nbar\r\n", "Note: inc file prefix:", ".", false);
        assert_eq!(r.filtered_output, "foo\nbar\n");
        assert_eq!(r.includes.iter().cloned().collect::<Vec<_>>(), vec!["foo.h"]);
    }

    #[test]
    fn parse_filename_filter() {
        let r = parse_cl_output("foo.cc\r\ncl: warning\r\n", "", ".", false);
        assert_eq!(r.filtered_output, "cl: warning\n");
    }

    #[test]
    fn no_filename_filter_after_show_includes() {
        let r = parse_cl_output("foo.cc\r\nNote: including file: foo.h\r\nsomething something foo.cc\r\n", "", ".", false);
        assert_eq!(r.filtered_output, "something something foo.cc\n");
    }

    #[test]
    fn parse_system_include() {
        let r = parse_cl_output(
            "Note: including file: c:\\Program Files\\foo.h\r\nNote: including file: d:\\Microsoft Visual Studio\\bar.h\r\nNote: including file: path.h\r\n",
            "",
            "c:/build",
            true,
        );
        assert_eq!(r.filtered_output, "");
        assert_eq!(r.includes.iter().cloned().collect::<Vec<_>>(), vec!["path.h"]);
    }

    #[test]
    fn duplicated_header() {
        let r = parse_cl_output("Note: including file: foo.h\r\nNote: including file: bar.h\r\nNote: including file: foo.h\r\n", "", ".", false);
        assert_eq!(r.includes.len(), 2);
    }

    #[test]
    fn normalize_relative() {
        assert_eq!(normalize_include("C:\\src\\proj\\inc\\a.h", "c:/src/proj/build", true), "../inc/a.h");
        assert_eq!(normalize_include("D:\\other\\a.h", "c:/src/proj/build", true), "D:/other/a.h");
        assert_eq!(normalize_include("c:/SRC/proj/build/gen/x.h", "C:/src/proj/build", true), "gen/x.h");
        assert_eq!(normalize_include("/usr/include/./x.h", "/home/b", false), "/usr/include/x.h");
    }
}
