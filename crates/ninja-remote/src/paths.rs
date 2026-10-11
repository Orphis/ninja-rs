//! Exec root selection and path mapping between the local machine and the
//! remote input root. See docs/paths.md.
//!
//! Local absolute paths are mapped to paths relative to an *exec root*
//! (by default the deepest common ancestor of the build directory and every
//! input of the action). The remote working directory is the build
//! directory's position in that tree, and absolute paths in command lines are
//! rewritten relative to it, so actions contain no machine-specific paths.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ninja_core::canon::canonicalize_path_with;

/// Normalizes an absolute local path: `/` separators, lexically canonical,
/// upper-case drive letter on Windows.
pub fn normalize_abs(p: &str, windows: bool) -> String {
    let s = if windows { p.replace('\\', "/") } else { p.to_string() };
    let (mut c, _) = canonicalize_path_with(&s, false);
    if windows && c.len() >= 2 && c.as_bytes()[1] == b':' {
        c = format!("{}{}", c[..1].to_ascii_uppercase(), &c[1..]);
        if c.len() == 2 {
            c.push('/');
        }
    }
    c
}

pub fn is_abs(p: &str, windows: bool) -> bool {
    if windows {
        let b = p.as_bytes();
        (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')) || p.starts_with("\\\\") || p.starts_with("//")
    } else {
        p.starts_with('/')
    }
}

fn components(p: &str) -> Vec<&str> {
    p.split('/').filter(|c| !c.is_empty()).collect()
}

fn eq_comp(a: &str, b: &str, windows: bool) -> bool {
    if windows { a.eq_ignore_ascii_case(b) } else { a == b }
}

/// The exec root: a local directory that maps to the root of the remote
/// input tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRoot {
    windows: bool,
    /// Normalized absolute local path. `None` on Windows when paths span
    /// several drives: the tree then has one top-level directory per drive.
    root: Option<String>,
}

impl ExecRoot {
    pub fn fixed(root: &str, windows: bool) -> Self {
        Self { windows, root: Some(normalize_abs(root, windows)) }
    }

    /// The deepest common ancestor of `paths` (normalized absolute paths).
    pub fn common(paths: &[String], windows: bool) -> Self {
        let mut it = paths.iter();
        let Some(first) = it.next() else { return Self { windows, root: Some("/".into()) } };
        let drive = |p: &str| if windows && p.len() >= 2 && p.as_bytes()[1] == b':' { p[..1].to_ascii_uppercase() } else { String::new() };
        let first_drive = drive(first);
        let mut common: Vec<String> = components(first).iter().map(|s| s.to_string()).collect();
        for p in it {
            if windows && drive(p) != first_drive {
                return Self { windows, root: None };
            }
            let comps = components(p);
            let mut n = 0;
            while n < common.len() && n < comps.len() && eq_comp(&common[n], comps[n], windows) {
                n += 1;
            }
            common.truncate(n);
        }
        let root = if windows {
            // common[0] is the drive ("C:").
            if common.len() <= 1 { format!("{first_drive}:/") } else { common.join("/") }
        } else {
            format!("/{}", common.join("/"))
        };
        Self { windows, root: Some(root) }
    }

    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    /// Whether it's a file system's root (`/`, `C:/`) or spans drives:
    /// paths under it may then name the host's own files (`/dev/null`).
    pub fn is_fs_root(&self) -> bool {
        self.root.as_deref().is_none_or(|r| components(r).len() <= usize::from(self.windows))
    }

    /// Tree path ('/'-separated, relative) of a normalized absolute local
    /// path, or `None` if it is outside the exec root.
    pub fn to_tree(&self, abs: &str) -> Option<String> {
        match &self.root {
            None => {
                // Multi-drive layout: "C:/x/y" -> "C/x/y".
                let b = abs.as_bytes();
                if b.len() >= 2 && b[1] == b':' {
                    let rest = abs[2..].trim_start_matches('/');
                    let d = abs[..1].to_ascii_uppercase();
                    return Some(if rest.is_empty() { d } else { format!("{d}/{rest}") });
                }
                None
            }
            Some(root) => {
                let rc = components(root);
                let pc = components(abs);
                if pc.len() < rc.len() {
                    return None;
                }
                for (a, b) in rc.iter().zip(pc.iter()) {
                    if !eq_comp(a, b, self.windows) {
                        return None;
                    }
                }
                Some(pc[rc.len()..].join("/"))
            }
        }
    }

    /// Local path of a tree path.
    pub fn to_local(&self, tree: &str) -> String {
        match &self.root {
            None => {
                let (d, rest) = tree.split_once('/').unwrap_or((tree, ""));
                format!("{}:/{rest}", d.to_ascii_uppercase())
            }
            Some(root) => {
                if tree.is_empty() {
                    root.clone()
                } else if root.ends_with('/') {
                    format!("{root}{tree}")
                } else {
                    format!("{root}/{tree}")
                }
            }
        }
    }
}

/// Relative path from tree directory `from` to tree path `to`.
pub fn relative(from: &str, to: &str, windows: bool) -> String {
    let f = components(from);
    let t = components(to);
    let mut n = 0;
    while n < f.len() && n < t.len() && eq_comp(f[n], t[n], windows) {
        n += 1;
    }
    let mut parts: Vec<&str> = Vec::new();
    for _ in n..f.len() {
        parts.push("..");
    }
    parts.extend_from_slice(&t[n..]);
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

/// Per-action mapping state.
#[derive(Clone, Debug)]
pub struct PathMapper {
    pub windows: bool,
    pub exec_root: ExecRoot,
    /// Tree path of the build directory (the remote working directory).
    pub work_dir: String,
    /// Normalized local build directory.
    pub build_dir: String,
    /// Canonical spelling of each tree path prefix (Windows: case folding).
    spelling: HashMap<String, String>,
    /// Top-level tree entries (case-folded on Windows): under a file
    /// system's root, absolute paths in arguments are only rewritten when
    /// they point into the tree.
    top_level: HashSet<String>,
}

impl PathMapper {
    pub fn new(exec_root: ExecRoot, build_dir: &str, windows: bool) -> Result<Self, String> {
        let build_dir = normalize_abs(build_dir, windows);
        let work_dir = exec_root.to_tree(&build_dir).ok_or_else(|| format!("build directory {build_dir} is outside the exec root {:?}", exec_root.root()))?;
        let mut m = Self { windows, exec_root, work_dir: String::new(), build_dir, spelling: HashMap::new(), top_level: HashSet::new() };
        m.work_dir = m.fold(&work_dir);
        m.note_top_level(&m.work_dir.clone());
        Ok(m)
    }

    fn key(&self, s: &str) -> String {
        if self.windows { s.to_ascii_lowercase() } else { s.to_string() }
    }

    /// Applies the canonical spelling to every prefix of `tree` (first
    /// spelling seen wins), so that one directory never appears twice with
    /// different case on Windows.
    fn fold(&mut self, tree: &str) -> String {
        if !self.windows || tree.is_empty() {
            return tree.to_string();
        }
        let mut out = String::new();
        let mut key = String::new();
        for c in tree.split('/') {
            if !out.is_empty() {
                out.push('/');
                key.push('/');
            }
            key.push_str(&c.to_ascii_lowercase());
            let spelled = self.spelling.entry(key.clone()).or_insert_with(|| format!("{out}{c}")).clone();
            out = spelled;
        }
        out
    }

    fn note_top_level(&mut self, tree: &str) {
        if let Some(first) = tree.split('/').next()
            && !first.is_empty()
        {
            let k = self.key(first);
            self.top_level.insert(k);
        }
    }

    /// Tree path for a local path (absolute, or relative to the build dir).
    pub fn tree_path(&mut self, local: &str) -> Option<String> {
        let abs = if is_abs(local, self.windows) {
            normalize_abs(local, self.windows)
        } else {
            normalize_abs(&format!("{}/{}", self.build_dir, local), self.windows)
        };
        let t = self.exec_root.to_tree(&abs)?;
        if t.split('/').any(|c| c == "..") {
            return None;
        }
        let t = self.fold(&t);
        self.note_top_level(&t);
        Some(t)
    }

    /// Local absolute path of a tree path.
    pub fn local_path(&self, tree: &str) -> PathBuf {
        PathBuf::from(self.exec_root.to_local(tree))
    }

    /// Path of tree path `tree` relative to the remote working directory.
    pub fn rel_from_work_dir(&self, tree: &str) -> String {
        relative(&self.work_dir, tree, self.windows)
    }

    /// `beside_tree`: also paths under the exec root that aren't in the
    /// tree (unless the root is a file system's).
    fn rewrite_one(&mut self, path: &str, beside_tree: bool) -> Option<String> {
        let abs = normalize_abs(path, self.windows);
        let tree = self.exec_root.to_tree(&abs)?;
        let first = tree.split('/').next().unwrap_or("");
        if !first.is_empty() && !self.top_level.contains(&self.key(first)) && (!beside_tree || self.exec_root.is_fs_root()) {
            return None;
        }
        let tree = self.fold(&tree);
        let mut rel = self.rel_from_work_dir(&tree);
        if (path.ends_with('/') || path.ends_with('\\')) && rel != "." {
            rel.push('/');
        }
        if self.windows && path.contains('\\') {
            rel = rel.replace('/', "\\");
        }
        Some(rel)
    }

    /// Rewrites absolute local paths inside one argument to paths relative
    /// to the working directory. Paths outside the exec root, or outside the
    /// input tree when the exec root is a file system's (`/dev/null`), are
    /// left alone.
    pub fn rewrite_arg(&mut self, arg: &str) -> String {
        let spans = find_abs_paths(arg, self.windows);
        if spans.is_empty() {
            return arg.to_string();
        }
        let mut out = String::new();
        let mut last = 0;
        for (s, e) in spans {
            out.push_str(&arg[last..s]);
            // A path under the exec root names the same place remotely,
            // in the tree or not (an include directory no header came
            // from): rewritten, the command doesn't depend on where the
            // checkout is.
            match self.rewrite_one(&arg[s..e], true) {
                Some(r) => out.push_str(&r),
                None => out.push_str(&arg[s..e]),
            }
            last = e;
        }
        out.push_str(&arg[last..]);
        out
    }

    /// Rewrites absolute local paths into the tree inside a command's
    /// output (diagnostics like `C:\src\a.c(3,13): warning` or
    /// `/src/a.c:3:13: error`) to paths relative to the working directory, as
    /// the command prints them when given relative paths, as remote execution
    /// does. A path ends at whitespace or a character diagnostics put after
    /// it; one with spaces keeps its tail, which stays correct relative to the
    /// rewritten head. Bytes that aren't UTF-8 pass through.
    pub fn rewrite_text(&mut self, text: &[u8]) -> Vec<u8> {
        let Some(root) = self.exec_root.root().map(str::to_string) else { return text.to_vec() };
        let root = root.trim_end_matches('/').to_string();
        let windows = self.windows;
        let spellings: Vec<Vec<u8>> = if windows { vec![root.clone().into_bytes(), root.replace('/', "\\").into_bytes()] } else { vec![root.into_bytes()] };
        let matches_at = |i: usize, s: &[u8]| {
            let Some(t) = text.get(i..i + s.len()) else { return false };
            if windows { t.eq_ignore_ascii_case(s) } else { t == s }
        };
        let mut out = Vec::with_capacity(text.len());
        let mut last = 0;
        let mut i = 0;
        while i < text.len() {
            let starts_word = i == 0 || !(text[i - 1].is_ascii_alphanumeric() || matches!(text[i - 1], b'_' | b'.' | b'/' | b'\\'));
            let hit = if starts_word { spellings.iter().find(|s| matches_at(i, s)).map(Vec::len) } else { None };
            let Some(len) = hit.filter(|&len| matches!(text.get(i + len), None | Some(b'/' | b'\\'))) else {
                i += 1;
                continue;
            };
            let mut e = i + len;
            while e < text.len()
                && !matches!(text[e], b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b':' | b'"' | b'\'' | b',' | b';' | b'<' | b'>' | b'|' | b'[' | b']')
            {
                e += 1;
            }
            if let Ok(path) = std::str::from_utf8(&text[i..e])
                && let Some(rel) = self.rewrite_one(path, false)
            {
                out.extend_from_slice(&text[last..i]);
                out.extend_from_slice(rel.as_bytes());
                last = e;
            }
            i = e;
        }
        out.extend_from_slice(&text[last..]);
        out
    }

    /// Maps a path reported by the remote action (in a depfile or
    /// /showIncludes output) back to a local path: relative paths are
    /// resolved against the working directory, absolute remote paths are
    /// matched against the input tree by suffix. Paths inside the build
    /// directory become relative to it, others absolute.
    pub fn map_back(&self, remote: &str, tree_files: &TreeIndex) -> String {
        let norm = if self.windows { remote.replace('\\', "/") } else { remote.to_string() };
        let tree = if is_abs(&norm, self.windows) || norm.starts_with('/') {
            match tree_files.match_suffix(&norm, self.windows) {
                Some(t) => t,
                None => return remote.to_string(),
            }
        } else {
            let joined = if self.work_dir.is_empty() { norm.clone() } else { format!("{}/{}", self.work_dir, norm) };
            let (c, _) = canonicalize_path_with(&joined, false);
            if c.starts_with("../") || c == ".." {
                return remote.to_string();
            }
            if c == "." { String::new() } else { c }
        };
        let local = self.exec_root.to_local(&tree);
        let local_norm = normalize_abs(&local, self.windows);
        if let Some(rel) = ExecRoot::fixed(&self.build_dir, self.windows).to_tree(&local_norm) {
            if rel.is_empty() {
                return ".".into();
            }
            return rel;
        }
        local_norm
    }
}

/// clang-cl options glued to a path, longest first (a prefix of another
/// comes after it).
const CL_JOINED_OPTIONS: &[&str] = &[
    "sourceDependencies",
    "vctoolsdir",
    "winsysroot",
    "external:I",
    "winsdkdir",
    "imsvc",
    "FI",
    "Fo",
    "Fe",
    "Fd",
    "Fp",
    "Fa",
    "Fi",
    "Fm",
    "Fr",
    "FR",
    "Yu",
    "Yc",
    "Tc",
    "Tp",
    "I",
];

/// Finds absolute paths embedded in a command line argument, e.g. in
/// `-I/x`, `/FoC:\x\y.obj`, `--sysroot=/x`, `-Wl,-rpath,/x`, and clang-cl's
/// `/vctoolsdir/x` on POSIX hosts.
pub fn find_abs_paths(arg: &str, windows: bool) -> Vec<(usize, usize)> {
    let b = arg.as_bytes();
    let mut spans = Vec::new();
    // clang-cl on a POSIX host: an option glued to its path ("/I/x",
    // "/vctoolsdir/x"), unless the whole argument is a file. The path
    // starts after the option.
    let cl_path = if !windows && arg.starts_with('/') && !Path::new(arg).exists() {
        CL_JOINED_OPTIONS.iter().find(|o| arg[1..].starts_with(*o) && arg[1 + o.len()..].starts_with('/')).map(|o| 1 + o.len())
    } else {
        None
    };
    let begin = cl_path.unwrap_or(0);
    let flag_prefix_ok = |i: usize| -> bool {
        if i == 0 {
            return true;
        }
        let prev = b[i - 1];
        if !(prev.is_ascii_alphanumeric()) {
            return true;
        }
        // A flag glued to its value: "-I/x", "/FoC:\x", "/external:IC:\x".
        (b[0] == b'-' || (windows && b[0] == b'/')) && b[1..i].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':'))
    };
    let mut i = begin;
    while i < b.len() {
        let start = if windows {
            i + 2 < b.len() && b[i].is_ascii_alphabetic() && b[i + 1] == b':' && (b[i + 2] == b'/' || b[i + 2] == b'\\')
        } else {
            b[i] == b'/'
                && (i == begin || matches!(b[i - 1], b'=' | b',' | b':' | b';' | b'@' | b'\'' | b'"') || {
                    // "-I/x", "-isystem/x": flag letters before the path.
                    b[0] == b'-' && b[1..i].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
                })
        };
        if start && (!windows || flag_prefix_ok(i)) {
            let mut e = i;
            // '=' ends a path too: prefix maps are OLD=NEW
            // (-ffile-prefix-map, -fdebug-prefix-map, /pathmap:).
            while e < b.len() && !matches!(b[e], b';' | b',' | b'"' | b'\'' | b'=') && !(!windows && b[e] == b':') {
                e += 1;
            }
            spans.push((i, e));
            i = e;
        } else {
            i += 1;
        }
    }
    spans
}

/// Index of the files in an input tree, for mapping remote absolute paths
/// back by suffix.
#[derive(Clone, Default, Debug)]
pub struct TreeIndex {
    files: HashSet<String>,
}

impl TreeIndex {
    pub fn new(paths: impl IntoIterator<Item = String>, windows: bool) -> Self {
        Self { files: paths.into_iter().map(|p| if windows { p.to_ascii_lowercase() } else { p }).collect() }
    }

    pub fn contains(&self, tree: &str, windows: bool) -> bool {
        let k = if windows { tree.to_ascii_lowercase() } else { tree.to_string() };
        self.files.contains(&k)
    }

    /// The longest suffix of `abs` (at a '/' boundary) that is a tree path.
    pub fn match_suffix(&self, abs: &str, windows: bool) -> Option<String> {
        let norm = if windows { abs.replace('\\', "/") } else { abs.to_string() };
        let (c, _) = canonicalize_path_with(&norm, false);
        let mut idx = 0;
        while let Some(p) = c[idx..].find('/') {
            let suffix = &c[idx + p + 1..];
            if self.contains(suffix, windows) {
                return Some(suffix.to_string());
            }
            idx += p + 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_root() {
        let r = ExecRoot::common(&["/home/u/proj/build".into(), "/home/u/proj/src/a.c".into(), "/home/u/.cache/llvm/bin/clang".into()], false);
        assert_eq!(r.root(), Some("/home/u"));
        assert_eq!(r.to_tree("/home/u/proj/src/a.c").as_deref(), Some("proj/src/a.c"));
        assert_eq!(r.to_tree("/home/v/x"), None);
        assert_eq!(r.to_local("proj/src/a.c"), "/home/u/proj/src/a.c");
        let w = ExecRoot::common(&["C:/Users/x/proj/build".into(), "c:/users/X/proj/src/a.c".into()], true);
        assert_eq!(w.root(), Some("C:/Users/x/proj"));
        assert_eq!(w.to_tree("C:/USERS/x/proj/src/a.c").as_deref(), Some("src/a.c"));
        let d = ExecRoot::common(&["C:/a/b".into(), "D:/c".into()], true);
        assert_eq!(d.root(), None);
        assert_eq!(d.to_tree("D:/c/x.h").as_deref(), Some("D/c/x.h"));
        assert_eq!(d.to_local("D/c/x.h"), "D:/c/x.h");
        let root = ExecRoot::common(&["C:/a".into(), "C:/b".into()], true);
        assert_eq!(root.root(), Some("C:/"));
        assert_eq!(root.to_tree("C:/a/x").as_deref(), Some("a/x"));
        assert_eq!(root.to_local("a/x"), "C:/a/x");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(relative("proj/build", "proj/src/a.c", false), "../src/a.c");
        assert_eq!(relative("proj/build", "proj/build/x.o", false), "x.o");
        assert_eq!(relative("", "a/b", false), "a/b");
        assert_eq!(relative("a/b", "", false), "../..");
        assert_eq!(relative("a", "a", false), ".");
    }

    #[test]
    fn find_paths() {
        assert_eq!(find_abs_paths("-I/usr/include", false), vec![(2, 14)]);
        assert_eq!(find_abs_paths("--sysroot=/sr", false), vec![(10, 13)]);
        assert_eq!(find_abs_paths("-Wl,-rpath,/a/b", false), vec![(11, 15)]);
        assert_eq!(find_abs_paths("foo/bar", false), vec![]);
        assert_eq!(find_abs_paths("/FoC:\\x\\y.obj", true), vec![(3, 13)]);
        assert_eq!(find_abs_paths("-IC:/x", true), vec![(2, 6)]);
        assert_eq!(find_abs_paths("/external:IC:\\x", true), vec![(11, 15)]);
        assert_eq!(find_abs_paths("-DX=C:/a;D:/b", true), vec![(4, 8), (9, 13)]);
        // clang-cl options glued to POSIX paths.
        assert_eq!(find_abs_paths("/vctoolsdir/tc/msvc", false), vec![(11, 19)]);
        assert_eq!(find_abs_paths("/imsvc/sdk/include", false), vec![(6, 18)]);
        assert_eq!(find_abs_paths("/I/src/include", false), vec![(2, 14)]);
        assert_eq!(find_abs_paths("/usr/include", false), vec![(0, 12)]);
        // Prefix maps: both sides, each up to the '='.
        assert_eq!(find_abs_paths("-ffile-prefix-map=/src/x=/x", false), vec![(18, 24), (25, 27)]);
        assert_eq!(find_abs_paths("/clang:-ffile-prefix-map=C:/co/.c=/c", true), vec![(25, 33)]);
        assert_eq!(find_abs_paths("abcC:/x", true), vec![]);
    }

    #[test]
    fn rewrite_args() {
        let root = ExecRoot::common(&["/home/u/proj/build".into(), "/home/u/proj/src/a.c".into(), "/home/u/tc/bin/clang".into()], false);
        let mut m = PathMapper::new(root, "/home/u/proj/build", false).unwrap();
        assert_eq!(m.work_dir, "proj/build");
        m.tree_path("/home/u/proj/src/a.c").unwrap();
        m.tree_path("/home/u/tc/bin/clang").unwrap();
        assert_eq!(m.rewrite_arg("/home/u/tc/bin/clang"), "../../tc/bin/clang");
        assert_eq!(m.rewrite_arg("-I/home/u/proj/src"), "-I../src");
        assert_eq!(m.rewrite_arg("-o/home/u/proj/build/a.o"), "-oa.o");
        assert_eq!(m.rewrite_arg("/dev/null"), "/dev/null");
        // Under the exec root but not in the tree (an include directory
        // no header came from): relative all the same.
        assert_eq!(m.rewrite_arg("/home/u/other/x"), "../../other/x");
        // Under a file system's root, only what's in the tree.
        let root = ExecRoot::common(&["/build".into(), "/src/a.c".into()], false);
        assert!(root.is_fs_root());
        let mut m = PathMapper::new(root, "/build", false).unwrap();
        m.tree_path("/src/a.c").unwrap();
        assert_eq!(m.rewrite_arg("-I/src"), "-I../src");
        assert_eq!(m.rewrite_arg("/dev/null"), "/dev/null");
        assert!(ExecRoot::common(&["C:/a".into(), "C:/b".into()], true).is_fs_root());
        assert!(!ExecRoot::common(&["C:/x/a".into(), "C:/x/b".into()], true).is_fs_root());
    }

    #[test]
    fn rewrite_windows_case_folding() {
        let root = ExecRoot::common(&["C:/Users/x/proj/build".into(), "C:/Users/x/proj/src/a.cpp".into()], true);
        let mut m = PathMapper::new(root, "C:\\Users\\x\\proj\\build", true).unwrap();
        // The first spelling of a directory wins.
        assert_eq!(m.tree_path("c:\\users\\x\\Proj\\SRC\\a.cpp").as_deref(), Some("SRC/a.cpp"));
        assert_eq!(m.tree_path("C:/Users/x/proj/src/B.h").as_deref(), Some("SRC/B.h"));
        assert_eq!(m.rewrite_arg("/FoC:\\Users\\x\\proj\\build\\obj\\a.obj"), "/Foobj\\a.obj");
        assert_eq!(m.rewrite_arg("-IC:/users/x/proj/src"), "-I../SRC");
    }

    #[test]
    fn rewrite_output_text() {
        let root = ExecRoot::common(&["/home/u/proj/build".into(), "/home/u/proj/src/a.c".into(), "/home/u/tc/bin/clang".into()], false);
        let mut m = PathMapper::new(root, "/home/u/proj/build", false).unwrap();
        m.tree_path("/home/u/proj/src/a.c").unwrap();
        m.tree_path("/home/u/tc/bin/clang").unwrap();
        let out =
            m.rewrite_text(b"/home/u/proj/src/a.c:3:13: warning: x [-Wy]\nIn file included from /home/u/proj/src/my dir/b.h:1:\n/usr/include/z.h:1: note\n");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "../src/a.c:3:13: warning: x [-Wy]\nIn file included from ../src/my dir/b.h:1:\n/usr/include/z.h:1: note\n"
        );
        // Only whole paths: the root inside another path or word stays.
        assert_eq!(m.rewrite_text(b"/x/home/u/proj/src/a.c"), b"/x/home/u/proj/src/a.c");
        assert_eq!(m.rewrite_text(b"/home/u/projects/a.c"), b"/home/u/projects/a.c");
        // Not UTF-8: unchanged around the paths.
        assert_eq!(m.rewrite_text(b"\xe9 /home/u/proj/src/a.c(1)"), b"\xe9 ../src/a.c(1)");

        let root = ExecRoot::common(&["C:/tmp/rc1/build".into(), "C:/tmp/rc1/gen.c".into()], true);
        let mut m = PathMapper::new(root, "C:\\tmp\\rc1\\build", true).unwrap();
        m.tree_path("C:/tmp/rc1/gen.c").unwrap();
        let out = m.rewrite_text(b"c:\\tmp\\RC1\\gen.c(3,13): warning: 'fopen' is deprecated\r\nC:/tmp/rc1/gen.c:3\r\n");
        assert_eq!(String::from_utf8(out).unwrap(), "..\\gen.c(3,13): warning: 'fopen' is deprecated\r\n../gen.c:3\r\n");
    }

    #[test]
    fn map_back_paths() {
        let root = ExecRoot::common(&["/home/u/proj/build".into(), "/home/u/proj/src/a.c".into(), "/home/u/tc/lib/clang/include/stddef.h".into()], false);
        let mut m = PathMapper::new(root, "/home/u/proj/build", false).unwrap();
        let files: Vec<String> =
            ["/home/u/proj/src/a.c", "/home/u/tc/lib/clang/include/stddef.h", "/home/u/proj/build/gen/g.h"].iter().map(|p| m.tree_path(p).unwrap()).collect();
        let idx = TreeIndex::new(files, false);
        assert_eq!(m.map_back("../src/a.c", &idx), "/home/u/proj/src/a.c");
        assert_eq!(m.map_back("gen/g.h", &idx), "gen/g.h");
        assert_eq!(m.map_back("/worker/exec/42/tc/lib/clang/include/stddef.h", &idx), "/home/u/tc/lib/clang/include/stddef.h");
        assert_eq!(m.map_back("/worker/exec/42/proj/build/gen/g.h", &idx), "gen/g.h");
        assert_eq!(m.map_back("/usr/include/stdio.h", &idx), "/usr/include/stdio.h");
    }
}
