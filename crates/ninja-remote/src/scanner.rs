//! C/C++ include scanner (like reclient's and Siso's): finds the headers a
//! compile reads by following `#include` directives, without running the
//! preprocessor.
//!
//! It is conservative: preprocessor conditionals are ignored, every
//! directive is followed, and every directory of the search path that
//! contains a candidate contributes it (which also covers `#include_next`).
//! Strings in `#define` lines that look like header names are also treated
//! as candidates (they are often used as computed includes). A computed
//! include (`#include MACRO`) is covered when the scanned files define the
//! macro as a header name (directly or through other macros); otherwise the
//! directory containing the file is sent whole, with a warning. Comments are
//! skipped: a `#include` in a comment is not a directive.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::cmdline::split_rsp;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    /// gcc, clang and compatible drivers.
    Gcc,
    /// cl.exe and clang-cl.
    Msvc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    C,
    Cxx,
    ObjC,
    ObjCxx,
    Asm,
}

/// What the scanner needs to know about a compile command.
#[derive(Clone, Debug, Default)]
pub struct CompileInfo {
    pub style: Option<Style>,
    pub quote_dirs: Vec<PathBuf>,
    pub user_dirs: Vec<PathBuf>,
    pub system_dirs: Vec<PathBuf>,
    pub after_dirs: Vec<PathBuf>,
    pub forced_includes: Vec<PathBuf>,
    pub sources: Vec<PathBuf>,
    /// Flags that influence the compiler's built-in include directories.
    pub builtin_flags: Vec<String>,
    pub lang: Option<Lang>,
    /// `-nostdinc` / `/X`: no built-in include directories.
    pub no_builtin_includes: bool,
    pub sysroot: Option<PathBuf>,
    /// Response files referenced with `@file`.
    pub rsp_files: Vec<PathBuf>,
    /// Other existing files named in arguments (e.g. `-fprofile-use=`,
    /// `-fmodule-file=`), sent as inputs.
    pub extra_files: Vec<PathBuf>,
}

/// Recognizes the compiler driver from argv[0].
pub fn compiler_style(argv0: &str, args: &[String]) -> Option<Style> {
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0).to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base).to_string();
    if args.iter().any(|a| a == "--driver-mode=cl") {
        return Some(Style::Msvc);
    }
    if base == "cl" || base == "clang-cl" || base.ends_with("-clang-cl") {
        return Some(Style::Msvc);
    }
    if base.contains("clang") || base.contains("gcc") || base.contains("g++") || base == "cc" || base == "c++" {
        return Some(Style::Gcc);
    }
    None
}

fn lang_of_source(p: &str) -> Option<Lang> {
    let lower = p.to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    Some(match ext {
        "c" => Lang::C,
        "cc" | "cpp" | "cxx" | "c++" | "cp" | "ixx" | "cppm" | "ccm" | "cxxm" | "c++m" | "mpp" => Lang::Cxx,
        "m" => Lang::ObjC,
        "mm" => Lang::ObjCxx,
        "s" | "asm" => Lang::Asm,
        "cu" | "hip" => Lang::Cxx,
        _ => return None,
    })
}

fn abs(p: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(p);
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
    normalize(&joined)
}

/// Lexical normalization (removes `.` and `..` components).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Expands `@file` arguments (recursively). Returns the expanded argv and
/// the response files read.
pub fn expand_response_files(argv: &[String], cwd: &Path, windows: bool) -> (Vec<String>, Vec<PathBuf>) {
    let mut out = Vec::new();
    let mut files = Vec::new();
    fn go(args: &[String], cwd: &Path, windows: bool, depth: usize, out: &mut Vec<String>, files: &mut Vec<PathBuf>) {
        for a in args {
            if let Some(f) = a.strip_prefix('@')
                && depth < 8
            {
                let p = abs(f, cwd);
                if let Ok(content) = std::fs::read(&p) {
                    let text = decode_rsp(&content);
                    files.push(p);
                    let inner = split_rsp(&text, windows);
                    go(&inner, cwd, windows, depth + 1, out, files);
                    continue;
                }
            }
            out.push(a.clone());
        }
    }
    go(argv, cwd, windows, 0, &mut out, &mut files);
    (out, files)
}

/// Response files may be UTF-16 (MSVC) or UTF-8 (with or without BOM).
pub fn decode_rsp(content: &[u8]) -> String {
    if content.len() >= 2 && content[0] == 0xff && content[1] == 0xfe {
        let u: Vec<u16> = content[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    let c = content.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(content);
    String::from_utf8_lossy(c).into_owned()
}

/// Parses a compile command (after response file expansion).
pub fn parse_compile(argv: &[String], cwd: &Path) -> CompileInfo {
    let mut info = CompileInfo { style: argv.first().and_then(|a| compiler_style(a, argv)), ..Default::default() };
    let msvc = info.style == Some(Style::Msvc);
    let mut i = 1;
    let take = |i: &mut usize| -> Option<String> {
        *i += 1;
        argv.get(*i - 1).cloned()
    };
    // Flags taking a separate argument that we don't care about.
    const SKIP_NEXT: &[&str] = &[
        "-o",
        "-MF",
        "-MT",
        "-MQ",
        "-MJ",
        "-Xclang",
        "-Xlinker",
        "-Xpreprocessor",
        "-Xassembler",
        "-mllvm",
        "-arch",
        "-D",
        "-U",
        "-main-file-name",
        "-L",
        "-l",
        "-F",
        "-iframework",
        "-iprefix",
        "-iwithprefix",
        "-iwithprefixbefore",
        "-dependency-file",
        "-Xarch_host",
        "-Xarch_device",
        "-Xcuda-ptxas",
        "-z",
        "-u",
        "-e",
        "-T",
    ];
    while i < argv.len() {
        let a = argv[i].clone();
        i += 1;
        let opt = |prefixes: &[&str], i: &mut usize| -> Option<Option<String>> {
            for p in prefixes {
                if a == *p {
                    return Some(take(i));
                }
                if let Some(v) = a.strip_prefix(p)
                    && !v.is_empty()
                {
                    return Some(Some(v.trim_start_matches('=').to_string()));
                }
            }
            None
        };
        // Order matters: longer prefixes first.
        if let Some(v) = opt(&["-isystem-after"], &mut i) {
            if let Some(v) = v {
                info.after_dirs.push(abs(&v, cwd));
            }
            continue;
        }
        if let Some(v) = opt(&["-isystem", "-cxx-isystem", "-imsvc", "/imsvc", "/external:I", "-external:I"], &mut i) {
            if let Some(v) = v {
                info.system_dirs.push(abs(&v, cwd));
            }
            continue;
        }
        if let Some(v) = opt(&["-iquote"], &mut i) {
            if let Some(v) = v {
                info.quote_dirs.push(abs(&v, cwd));
            }
            continue;
        }
        if let Some(v) = opt(&["-idirafter"], &mut i) {
            if let Some(v) = v {
                info.after_dirs.push(abs(&v, cwd));
            }
            continue;
        }
        if let Some(v) = opt(&["-include-pch"], &mut i) {
            if let Some(v) = v {
                info.extra_files.push(abs(&v, cwd));
            }
            continue;
        }
        if let Some(v) = opt(&["-include", "-imacros", "--include"], &mut i) {
            if let Some(v) = v {
                info.forced_includes.push(abs(&v, cwd));
            }
            continue;
        }
        if msvc && let Some(v) = opt(&["/FI", "-FI"], &mut i) {
            if let Some(v) = v {
                info.forced_includes.push(abs(&v, cwd));
            }
            continue;
        }
        if let Some(v) = opt(&["--sysroot", "-isysroot", "/winsysroot", "-winsysroot"], &mut i) {
            if let Some(v) = v {
                let p = abs(&v, cwd);
                info.sysroot = Some(p.clone());
                let flag = a.split(['=', ':']).next().unwrap_or(&a).to_string();
                let flag = if flag.starts_with("--sysroot") { "--sysroot".to_string() } else { flag };
                info.builtin_flags.push(flag);
                info.builtin_flags.push(p.to_string_lossy().into_owned());
            }
            continue;
        }
        if let Some(v) = opt(&["-I", "/I", "--include-directory"], &mut i) {
            // On POSIX "/I..." is a path, not a flag.
            if a.starts_with('/') && !msvc {
                info.sources.extend(source_if_any(&a, cwd));
                continue;
            }
            if let Some(v) = v {
                info.user_dirs.push(abs(&v, cwd));
            }
            continue;
        }
        // Flags that change the built-in include directories, normalized to
        // their joined form.
        let mut builtin = |canonical: &str, prefixes: &[&str], joined_with: &str, i: &mut usize| -> bool {
            if let Some(v) = opt(prefixes, i) {
                if let Some(v) = v {
                    if joined_with.is_empty() {
                        info.builtin_flags.push(canonical.to_string());
                        info.builtin_flags.push(v);
                    } else {
                        info.builtin_flags.push(format!("{canonical}{joined_with}{v}"));
                    }
                }
                return true;
            }
            false
        };
        if builtin("--target", &["--target", "-target"], "=", &mut i)
            || builtin("--gcc-toolchain", &["--gcc-toolchain", "-gcc-toolchain"], "=", &mut i)
            || builtin("-resource-dir", &["-resource-dir"], "=", &mut i)
            || builtin("--driver-mode", &["--driver-mode"], "=", &mut i)
            || builtin("-stdlib", &["-stdlib="], "=", &mut i)
            || builtin("/vctoolsdir", &["/vctoolsdir", "-vctoolsdir"], "", &mut i)
            || builtin("/winsdkdir", &["/winsdkdir", "-winsdkdir"], "", &mut i)
            || builtin("/winsdkversion", &["/winsdkversion", "-winsdkversion"], "", &mut i)
        {
            continue;
        }
        match a.as_str() {
            "-nostdinc" | "-nostdlibinc" | "-nobuiltininc" | "/X" | "-X" => {
                if a == "-nostdinc" || a == "/X" || a == "-X" {
                    info.no_builtin_includes = true;
                }
                info.builtin_flags.push(a.clone());
                continue;
            }
            "-nostdinc++" | "-m32" | "-m64" | "-mx32" | "-pthread" | "-fms-extensions" | "-fms-compatibility" => {
                info.builtin_flags.push(a.clone());
                continue;
            }
            "/TP" | "-TP" => {
                info.lang = Some(Lang::Cxx);
                continue;
            }
            "/TC" | "-TC" => {
                info.lang = Some(Lang::C);
                continue;
            }
            _ => {}
        }
        if a == "-x" {
            if let Some(l) = argv.get(i) {
                info.lang = match l.as_str() {
                    "c" | "c-header" => Some(Lang::C),
                    "c++" | "c++-header" | "c++-module" | "cuda" | "hip" => Some(Lang::Cxx),
                    "objective-c" => Some(Lang::ObjC),
                    "objective-c++" => Some(Lang::ObjCxx),
                    "assembler-with-cpp" | "assembler" => Some(Lang::Asm),
                    _ => info.lang,
                };
            }
            i += 1;
            continue;
        }
        if a.starts_with("-fprofile-use=")
            || a.starts_with("-fprofile-instr-use=")
            || a.starts_with("-fsanitize-ignorelist=")
            || a.starts_with("-fsanitize-blacklist=")
        {
            if let Some((_, v)) = a.split_once('=') {
                let p = abs(v, cwd);
                if p.is_file() {
                    info.extra_files.push(p);
                }
            }
            continue;
        }
        if let Some(v) = a.strip_prefix("-fmodule-file=") {
            // "-fmodule-file=name=path" or "-fmodule-file=path".
            let path = v.rsplit_once('=').map(|(_, p)| p).unwrap_or(v);
            let p = abs(path, cwd);
            if p.is_file() {
                info.extra_files.push(p);
            }
            continue;
        }
        // cc1's spellings through the driver: "-Xclang -include -Xclang F"
        // (CMake's precompiled headers with Clang), "-Xclang -include-pch".
        if a == "-Xclang"
            && argv.get(i + 1).is_some_and(|n| n == "-Xclang")
            && let Some(flag) = argv.get(i).filter(|f| matches!(f.as_str(), "-include" | "-imacros" | "-include-pch"))
        {
            if let Some(v) = argv.get(i + 2) {
                if flag == "-include-pch" {
                    info.extra_files.push(abs(v, cwd));
                } else {
                    info.forced_includes.push(abs(v, cwd));
                }
            }
            i += 3;
            continue;
        }
        // Virtual file system overlays: the overlay file is an input (its
        // entries name files the include scan finds where they really are).
        // "-ivfsoverlay F", "-vfsoverlay F", or cc1's through "-Xclang".
        if a == "-Xclang" && argv.get(i).is_some_and(|n| n == "-ivfsoverlay") && argv.get(i + 1).is_some_and(|n| n == "-Xclang") {
            if let Some(v) = argv.get(i + 2) {
                info.extra_files.push(abs(v, cwd));
            }
            i += 3;
            continue;
        }
        if let Some(v) = opt(&["-ivfsoverlay", "-vfsoverlay", "--vfsoverlay"], &mut i) {
            if let Some(v) = v {
                info.extra_files.push(abs(&v, cwd));
            }
            continue;
        }
        if SKIP_NEXT.contains(&a.as_str()) {
            i += 1;
            continue;
        }
        if a.starts_with('-') || (msvc && a.starts_with('/') && !Path::new(&a).is_file()) {
            continue;
        }
        info.sources.extend(source_if_any(&a, cwd));
    }
    if info.lang.is_none() {
        info.lang = info.sources.iter().find_map(|s| lang_of_source(&s.to_string_lossy()));
    }
    info
}

fn source_if_any(a: &str, cwd: &Path) -> Option<PathBuf> {
    lang_of_source(a)?;
    let p = abs(a, cwd);
    p.is_file().then_some(p)
}

// ---------------------------------------------------------------------------
// Directive parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Include {
    pub name: String,
    pub angled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directives {
    pub includes: Vec<Include>,
    /// Header-looking strings from `#define` lines (possible computed includes).
    pub define_candidates: Vec<Include>,
    /// The macros of computed includes (`#include MACRO`); empty for one
    /// whose macro can't be named (`#include MACRO(x)`, other tokens).
    pub computed: Vec<String>,
    /// Macros defined as (or containing) a header name.
    pub header_macros: Vec<String>,
    /// `#define A B`: macros defined as another macro.
    pub macro_aliases: Vec<(String, String)>,
}

fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `line` without its comments (each replaced by a space). `in_comment`
/// carries a block comment from one line to the next. String and character
/// literals are kept whole, so that `"/*"` doesn't start a comment; an
/// apostrophe inside a number (`1'000`) is a digit separator.
fn strip_comments(line: &str, in_comment: &mut bool) -> String {
    let b = line.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if *in_comment {
            if c == b'*' && next == Some(b'/') {
                *in_comment = false;
                out.push(b' ');
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = quote {
            out.push(c);
            if c == b'\\' {
                if let Some(n) = next {
                    out.push(n);
                }
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match (c, next) {
            (b'/', Some(b'*')) => {
                *in_comment = true;
                i += 2;
            }
            (b'/', Some(b'/')) => break,
            (b'"', _) => {
                quote = Some(c);
                out.push(c);
                i += 1;
            }
            (b'\'', _) => {
                // A digit separator when the token before is a number.
                let start = out.iter().rposition(|&x| !(x.is_ascii_alphanumeric() || x == b'_' || x == b'\'')).map_or(0, |p| p + 1);
                if !out.get(start).is_some_and(u8::is_ascii_digit) {
                    quote = Some(c);
                }
                out.push(c);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn header_like(name: &str) -> bool {
    if name.is_empty() || name.len() > 256 || name.contains(['\n', '*', '?', '%']) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    [".h", ".hh", ".hpp", ".hxx", ".h++", ".inc", ".inl", ".ipp", ".def", ".tcc", ".ixx", ".tlh", ".tli"].iter().any(|e| lower.ends_with(e))
}

fn parse_quoted(s: &str) -> Option<(Include, &str)> {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('"') {
        let end = r.find('"')?;
        return Some((Include { name: r[..end].to_string(), angled: false }, &r[end + 1..]));
    }
    if let Some(r) = s.strip_prefix('<') {
        let end = r.find('>')?;
        return Some((Include { name: r[..end].to_string(), angled: true }, &r[end + 1..]));
    }
    None
}

/// Extracts include directives from a source file.
pub fn parse_directives(content: &[u8]) -> Directives {
    let text = String::from_utf8_lossy(content);
    let mut d = Directives::default();
    let mut lines = text.split('\n').peekable();
    let mut in_comment = false;
    while let Some(raw) = lines.next() {
        let mut line = raw.trim_end_matches('\r').to_string();
        // Line continuations.
        while line.ends_with('\\') {
            line.pop();
            match lines.next() {
                Some(next) => line.push_str(next.trim_end_matches('\r')),
                None => break,
            }
        }
        let line = strip_comments(&line, &mut in_comment);
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix('#') else { continue };
        let rest = rest.trim_start();
        let (kw, args) = match rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        match kw {
            "include" | "include_next" | "import" => match parse_quoted(args) {
                Some((inc, _)) => d.includes.push(inc),
                None => {
                    let a = args.trim();
                    // `#include MACRO`: a computed include.
                    if !a.is_empty() {
                        d.computed.push(if is_identifier(a) { a.to_string() } else { String::new() });
                    }
                }
            },
            "if" | "elif" | "ifdef" | "ifndef" | "elifdef" | "elifndef" => {
                // __has_include("x") / __has_include(<x>)
                let mut s = args;
                while let Some(pos) = s.find("__has_include") {
                    let after = &s[pos + "__has_include".len()..];
                    let after = after.strip_prefix("_next").unwrap_or(after);
                    let after = after.trim_start();
                    if let Some(a) = after.strip_prefix('(')
                        && let Some((inc, _)) = parse_quoted(a)
                    {
                        d.includes.push(inc);
                    }
                    s = after;
                }
            }
            "define" => {
                let a = args.trim_start();
                let name_end = a.find(|c: char| !c.is_ascii_alphanumeric() && c != '_').unwrap_or(a.len());
                let (name, value) = (&a[..name_end], a[name_end..].trim());
                if is_identifier(value) && !name.is_empty() {
                    d.macro_aliases.push((name.to_string(), value.to_string()));
                }
                let mut s = args;
                let mut defines_header = false;
                while let Some(p) = s.find(['"', '<']) {
                    match parse_quoted(&s[p..]) {
                        Some((inc, r)) => {
                            if header_like(&inc.name) {
                                d.define_candidates.push(inc);
                                defines_header = true;
                            }
                            s = r;
                        }
                        None => break,
                    }
                }
                if defines_header && !name.is_empty() {
                    d.header_macros.push(name.to_string());
                }
            }
            _ => {}
        }
    }
    d
}

// ---------------------------------------------------------------------------
// Search paths and caches
// ---------------------------------------------------------------------------

/// The include search path of a compile.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct SearchPath {
    pub quote: Vec<PathBuf>,
    pub angled: Vec<PathBuf>,
    /// MSVC also searches the directories of the including files; we
    /// approximate with the source files' directories.
    pub msvc_source_dirs: Vec<PathBuf>,
}

impl SearchPath {
    pub fn id(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

type FileKey = (PathBuf, u64, Option<SystemTime>);

#[derive(Clone, Debug, Default)]
pub struct Resolved {
    pub files: Vec<PathBuf>,
    /// The file's directives about macros (see `Directives`).
    pub directives: Arc<Directives>,
}

/// Scanner caches, shared across actions (and builds, in a daemon).
#[derive(Default)]
pub struct ScanCache {
    directives: Mutex<HashMap<PathBuf, (u64, Option<SystemTime>, Arc<Directives>)>>,
    resolutions: Mutex<HashMap<(FileKey, u64), Arc<Resolved>>>,
    listings: Mutex<HashMap<PathBuf, Option<Arc<HashSet<String>>>>>,
    builtin: Mutex<HashMap<String, Arc<Vec<PathBuf>>>>,
    pub stats: Mutex<ScanStats>,
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub files_parsed: usize,
    pub resolution_hits: usize,
    pub resolution_misses: usize,
}

pub struct ScanResult {
    /// Every file the compile may read (sources, headers, forced includes).
    pub files: BTreeSet<PathBuf>,
    /// Directories to send whole (computed includes).
    pub dirs: BTreeSet<PathBuf>,
    pub warnings: Vec<String>,
}

pub struct Scanner<'a> {
    pub cache: &'a ScanCache,
    pub windows: bool,
    /// Directories whose contents may change during the build (the build
    /// directory): never cached.
    pub volatile_roots: Vec<PathBuf>,
}

fn file_key(p: &Path) -> Option<FileKey> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() {
        return None;
    }
    Some((p.to_path_buf(), m.len(), m.modified().ok()))
}

impl Scanner<'_> {
    fn is_volatile(&self, p: &Path) -> bool {
        self.volatile_roots.iter().any(|r| p.starts_with(r))
    }

    fn name_key(&self, s: &str) -> String {
        if self.windows { s.to_ascii_lowercase() } else { s.to_string() }
    }

    fn listing(&self, dir: &Path) -> Option<Arc<HashSet<String>>> {
        if let Some(l) = self.cache.listings.lock().unwrap().get(dir) {
            return l.clone();
        }
        let l = std::fs::read_dir(dir)
            .ok()
            .map(|rd| Arc::new(rd.filter_map(|e| e.ok()).map(|e| self.name_key(&e.file_name().to_string_lossy())).collect::<HashSet<_>>()));
        self.cache.listings.lock().unwrap().insert(dir.to_path_buf(), l.clone());
        l
    }

    /// Whether `p` is an existing file, using cached directory listings
    /// outside the build directory.
    pub fn is_file(&self, p: &Path) -> bool {
        if self.is_volatile(p) {
            return p.is_file();
        }
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { return p.is_file() };
        match self.listing(parent) {
            Some(l) => l.contains(&self.name_key(&name.to_string_lossy())) && p.is_file(),
            None => false,
        }
    }

    fn directives(&self, p: &Path, key: &FileKey) -> Arc<Directives> {
        if let Some((len, t, d)) = self.cache.directives.lock().unwrap().get(p)
            && *len == key.1
            && *t == key.2
        {
            return d.clone();
        }
        let d = Arc::new(std::fs::read(p).map(|c| parse_directives(&c)).unwrap_or_default());
        self.cache.stats.lock().unwrap().files_parsed += 1;
        self.cache.directives.lock().unwrap().insert(p.to_path_buf(), (key.1, key.2, d.clone()));
        d
    }

    fn candidates(&self, inc: &Include, from_dir: &Path, sp: &SearchPath, out: &mut Vec<PathBuf>) -> bool {
        let name = &inc.name;
        let np = Path::new(name);
        if np.is_absolute() || (self.windows && name.len() > 2 && name.as_bytes()[1] == b':') {
            let p = normalize(np);
            if self.is_file(&p) {
                out.push(p);
                return true;
            }
            return false;
        }
        let mut found = false;
        let mut try_dir = |d: &Path, out: &mut Vec<PathBuf>| {
            let p = normalize(&d.join(np));
            if self.is_file(&p) {
                out.push(p);
                found = true;
            }
        };
        if !inc.angled {
            try_dir(from_dir, out);
            for d in &sp.quote {
                try_dir(d, out);
            }
            for d in &sp.msvc_source_dirs {
                try_dir(d, out);
            }
        }
        for d in &sp.angled {
            try_dir(d, out);
        }
        found
    }

    /// Resolves the includes of one file (cached per file version and
    /// search path).
    fn resolve_file(&self, p: &Path, sp: &SearchPath, sp_id: u64) -> Arc<Resolved> {
        let Some(key) = file_key(p) else { return Arc::new(Resolved::default()) };
        let ck = (key.clone(), sp_id);
        if let Some(r) = self.cache.resolutions.lock().unwrap().get(&ck) {
            self.cache.stats.lock().unwrap().resolution_hits += 1;
            return r.clone();
        }
        self.cache.stats.lock().unwrap().resolution_misses += 1;
        let d = self.directives(p, &key);
        let from_dir = p.parent().unwrap_or(Path::new(""));
        let mut files = Vec::new();
        let mut unresolved = false;
        for inc in &d.includes {
            if !self.candidates(inc, from_dir, sp, &mut files) {
                unresolved = true;
            }
        }
        for inc in &d.define_candidates {
            self.candidates(inc, from_dir, sp, &mut files);
        }
        files.sort();
        files.dedup();
        let r = Arc::new(Resolved { files, directives: d.clone() });
        // An include that is not found may be generated later in the build
        // directory: don't cache that.
        let volatile_search = sp.quote.iter().chain(&sp.angled).any(|d| self.is_volatile(d)) || self.is_volatile(from_dir);
        if !(unresolved && volatile_search) {
            self.cache.resolutions.lock().unwrap().insert(ck, r.clone());
        }
        r
    }

    /// Finds every file reachable from the sources and forced includes.
    pub fn scan(&self, roots: &[PathBuf], sp: &SearchPath) -> ScanResult {
        let sp_id = sp.id();
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        let mut warnings = Vec::new();
        let mut queue: VecDeque<PathBuf> = roots.iter().cloned().collect();
        // Computed includes, and the macros the scanned files define.
        let mut computed: Vec<(PathBuf, Vec<String>)> = Vec::new();
        let mut header_macros: HashSet<String> = HashSet::new();
        let mut aliases: Vec<(String, String)> = Vec::new();
        while let Some(f) = queue.pop_front() {
            if !files.insert(f.clone()) {
                continue;
            }
            let r = self.resolve_file(&f, sp, sp_id);
            if !r.directives.computed.is_empty() {
                computed.push((f.clone(), r.directives.computed.clone()));
            }
            header_macros.extend(r.directives.header_macros.iter().cloned());
            aliases.extend(r.directives.macro_aliases.iter().cloned());
            for inc in r.files.iter() {
                if !files.contains(inc) {
                    queue.push_back(inc.clone());
                }
            }
        }
        // `#define A B` with B a header macro makes A one.
        loop {
            let before = header_macros.len();
            for (a, b) in &aliases {
                if header_macros.contains(b) {
                    header_macros.insert(a.clone());
                }
            }
            if header_macros.len() == before {
                break;
            }
        }
        // A computed include whose macro names a header is covered: the
        // header was followed as a candidate. Otherwise its directory goes
        // whole.
        for (f, macros) in computed {
            if macros.iter().all(|m| header_macros.contains(m)) {
                continue;
            }
            if let Some(parent) = f.parent()
                && dirs.insert(parent.to_path_buf())
            {
                warnings.push(format!("computed #include in {}: sending {} whole", f.display(), parent.display()));
            }
        }
        ScanResult { files, dirs, warnings }
    }

    /// The compiler's built-in include directories, from `-v` output
    /// (cached per compiler, relevant flags and language).
    pub fn builtin_dirs(&self, compiler: &Path, info: &CompileInfo) -> Result<Arc<Vec<PathBuf>>, String> {
        if info.no_builtin_includes {
            return Ok(Arc::new(Vec::new()));
        }
        let lang = info.lang.unwrap_or(Lang::Cxx);
        let key = format!("{}|{:?}|{:?}|{}", compiler.display(), info.style, lang, info.builtin_flags.join("\u{1}"));
        if let Some(d) = self.cache.builtin.lock().unwrap().get(&key) {
            return Ok(d.clone());
        }
        let dirs = Arc::new(discover_builtin_dirs(compiler, info, lang)?);
        self.cache.builtin.lock().unwrap().insert(key, dirs.clone());
        Ok(dirs)
    }
}

/// Runs `<compiler> -E -v` on an empty input and parses the include search
/// list it prints.
pub fn discover_builtin_dirs(compiler: &Path, info: &CompileInfo, lang: Lang) -> Result<Vec<PathBuf>, String> {
    let base = compiler.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if base == "cl" {
        // MSVC's cl.exe has no -v; it uses the INCLUDE environment variable.
        return Ok(std::env::var_os("INCLUDE").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default());
    }
    let mut cmd = std::process::Command::new(compiler);
    cmd.args(&info.builtin_flags);
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    if info.style == Some(Style::Msvc) {
        cmd.arg("-v").arg("-E").arg(if lang == Lang::C { "/TC" } else { "/TP" }).arg(null);
    } else {
        let l = match lang {
            Lang::C => "c",
            Lang::Cxx => "c++",
            Lang::ObjC => "objective-c",
            Lang::ObjCxx => "objective-c++",
            Lang::Asm => "assembler-with-cpp",
        };
        cmd.arg("-E").arg("-x").arg(l).arg("-v").arg(null);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    let out = cmd.output().map_err(|e| format!("running {} -v: {e}", compiler.display()))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let dirs = parse_search_list(&text);
    if dirs.is_empty() {
        return Err(format!("could not find the include search list in `{} -v` output (exit {:?})", compiler.display(), out.status.code()));
    }
    Ok(dirs)
}

pub fn parse_search_list(text: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        let l = line.trim_end();
        if l.starts_with("#include \"...\" search starts here:") || l.starts_with("#include <...> search starts here:") {
            in_list = true;
            continue;
        }
        if l.starts_with("End of search list.") {
            in_list = false;
            continue;
        }
        if in_list {
            let d = l.trim();
            if d.ends_with("(framework directory)") || d.is_empty() {
                continue;
            }
            dirs.push(normalize(Path::new(d)));
        }
    }
    dirs
}

/// Builds the search path of a compile from its arguments and the
/// compiler's built-in directories.
pub fn search_path(info: &CompileInfo, builtin: &[PathBuf]) -> SearchPath {
    let mut angled = info.user_dirs.clone();
    angled.extend(info.system_dirs.iter().cloned());
    angled.extend(builtin.iter().cloned());
    angled.extend(info.after_dirs.iter().cloned());
    let mut seen = HashSet::new();
    angled.retain(|d| seen.insert(d.clone()));
    let msvc_source_dirs =
        if info.style == Some(Style::Msvc) { info.sources.iter().filter_map(|s| s.parent().map(Path::to_path_buf)).collect() } else { Vec::new() };
    SearchPath { quote: info.quote_dirs.clone(), angled, msvc_source_dirs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn styles() {
        assert_eq!(compiler_style("/usr/bin/clang++", &[]), Some(Style::Gcc));
        assert_eq!(compiler_style("C:\\llvm\\bin\\clang-cl.exe", &[]), Some(Style::Msvc));
        assert_eq!(compiler_style("cl.exe", &[]), Some(Style::Msvc));
        assert_eq!(compiler_style("x86_64-linux-gnu-g++", &[]), Some(Style::Gcc));
        assert_eq!(compiler_style("clang", &s(&["--driver-mode=cl"])), Some(Style::Msvc));
        assert_eq!(compiler_style("/usr/bin/ar", &[]), None);
    }

    #[test]
    fn directives() {
        let d = parse_directives(
            b"#include <stdio.h>\n  #  include \"a/b.h\" // c\n#include_next <x.h>\n#import \"objc.h\"\n#include MACRO\n#if __has_include(<opt.h>) && __has_include_next(\"o2.h\")\n#endif\n#define HDR <freetype/freetype.h>\n#define NOT_A_HEADER \"hello\"\n/* #include \"commented.h\" */\n#include \\\n  \"continued.h\"\n",
        );
        let names: Vec<(&str, bool)> = d.includes.iter().map(|i| (i.name.as_str(), i.angled)).collect();
        assert_eq!(
            names,
            vec![("stdio.h", true), ("a/b.h", false), ("x.h", true), ("objc.h", false), ("opt.h", true), ("o2.h", false), ("continued.h", false)]
        );
        assert_eq!(d.computed, vec!["MACRO".to_string()]);
        assert_eq!(d.define_candidates, vec![Include { name: "freetype/freetype.h".into(), angled: true }]);
        assert_eq!(d.header_macros, vec!["HDR".to_string()]);
    }

    #[test]
    fn comments_are_skipped() {
        let d = parse_directives(
            b"/* A comment\n   #include's protect themselves\n   #include \"in_comment.h\" */ #include \"after.h\"\n\
              #define GLOB \"*/*\"  /* \"/*\" in a string doesn't start a comment */\n\
              int x = 1'000; /* a digit separator\n#include <hidden.h>\n*/\n\
              #include <real.h> // #include <line_comment.h>\n\
              #define ALIAS HDR\n#define HDR <h.h>\n#include ALIAS\n#include IT(x)\n",
        );
        let names: Vec<&str> = d.includes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["after.h", "real.h"]);
        assert_eq!(d.computed, vec!["ALIAS".to_string(), String::new()]);
        assert_eq!(d.macro_aliases, vec![("ALIAS".to_string(), "HDR".to_string())]);
        assert_eq!(d.header_macros, vec!["HDR".to_string()]);
        assert_eq!(strip_comments("a /* b */ c // d", &mut false), "a   c ");
        assert_eq!(strip_comments("char c = '\\''; /* x", &mut false), "char c = '\\''; ");
    }

    #[test]
    fn computed_includes_of_known_macros() {
        let dir = std::env::temp_dir().join(format!("ninja_rs_scan_computed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("inc/ft")).unwrap();
        std::fs::write(dir.join("main.c"), "#include <config.h>\n#include FT_HEADER\n#include \"other.h\"\n").unwrap();
        std::fs::write(dir.join("inc/config.h"), "#define FT_IMPL <ft/impl.h>\n#define FT_HEADER FT_IMPL\n").unwrap();
        std::fs::write(dir.join("inc/ft/impl.h"), "").unwrap();
        std::fs::write(dir.join("inc/ft/unused.h"), "").unwrap();
        // A comment mentioning #include, as glibc's limits.h has.
        std::fs::write(dir.join("other.h"), "/*\n   #include's protect themselves\n*/\n").unwrap();
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: false, volatile_roots: vec![] };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc_source_dirs: vec![] };
        let r = scanner.scan(&[dir.join("main.c")], &sp);
        let rel: Vec<String> = r.files.iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(rel, vec!["inc/config.h", "inc/ft/impl.h", "main.c", "other.h"]);
        assert!(r.dirs.is_empty() && r.warnings.is_empty(), "{:?} {:?}", r.dirs, r.warnings);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_gcc_command() {
        let cwd = std::env::temp_dir();
        let src = cwd.join("ninja_rs_scan_test_main.cc");
        std::fs::write(&src, "").unwrap();
        let argv = s(&[
            "/usr/bin/clang++",
            "-I",
            "/inc1",
            "-I/inc2",
            "-isystem",
            "/sys",
            "-iquote/q",
            "-idirafter",
            "/after",
            "-include",
            "/forced.h",
            // CMake's precompiled headers with Clang.
            "-Xclang",
            "-include-pch",
            "-Xclang",
            "/pch.hxx.pch",
            "-Xclang",
            "-include",
            "-Xclang",
            "/pch.hxx",
            "--sysroot=/sr",
            "--target=x86_64-linux-gnu",
            "-DX=1",
            "-o",
            "out.o",
            "-c",
            src.to_str().unwrap(),
            "-MD",
            "-MF",
            "x.d",
        ]);
        let info = parse_compile(&argv, &cwd);
        let _ = std::fs::remove_file(&src);
        assert_eq!(info.style, Some(Style::Gcc));
        let a = |p: &str| abs(p, &cwd);
        assert_eq!(info.user_dirs, vec![a("/inc1"), a("/inc2")]);
        assert_eq!(info.system_dirs, vec![a("/sys")]);
        assert_eq!(info.quote_dirs, vec![a("/q")]);
        assert_eq!(info.after_dirs, vec![a("/after")]);
        assert_eq!(info.forced_includes, vec![a("/forced.h"), a("/pch.hxx")]);
        assert!(info.extra_files.contains(&a("/pch.hxx.pch")));
        assert_eq!(info.sysroot, Some(a("/sr")));
        assert_eq!(info.lang, Some(Lang::Cxx));
        assert_eq!(info.sources.len(), 1);
        assert!(info.builtin_flags.contains(&"--target=x86_64-linux-gnu".to_string()));
        assert!(info.builtin_flags.contains(&"--sysroot".to_string()));
    }

    #[test]
    fn search_list() {
        let text = "clang version 19\n#include \"...\" search starts here:\n#include <...> search starts here:\n /usr/lib/llvm/include/c++/v1\n /usr/lib/clang/19/include\n /usr/include\n /System/Library/Frameworks (framework directory)\nEnd of search list.\n";
        assert_eq!(
            parse_search_list(text),
            vec![PathBuf::from("/usr/lib/llvm/include/c++/v1"), PathBuf::from("/usr/lib/clang/19/include"), PathBuf::from("/usr/include")]
        );
    }

    #[test]
    fn scan_tree() {
        let dir = std::env::temp_dir().join(format!("ninja_rs_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("inc/sub")).unwrap();
        std::fs::create_dir_all(dir.join("sys")).unwrap();
        std::fs::create_dir_all(dir.join("comp")).unwrap();
        std::fs::write(dir.join("src/main.c"), "#include \"local.h\"\n#include <a.h>\n#include <missing.h>\n#ifdef X\n#include \"comp/c.h\"\n#endif\n")
            .unwrap();
        std::fs::write(dir.join("src/local.h"), "#include <sub/b.h>\n").unwrap();
        std::fs::write(dir.join("inc/a.h"), "#include_next <a.h>\n#pragma once\n").unwrap();
        std::fs::write(dir.join("sys/a.h"), "/* the real a.h */\n").unwrap();
        std::fs::write(dir.join("inc/sub/b.h"), "#include \"../a.h\"\n").unwrap();
        std::fs::write(dir.join("comp/c.h"), "#include COMPUTED\n").unwrap();
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: false, volatile_roots: vec![] };
        let sp = SearchPath { quote: vec![dir.clone()], angled: vec![dir.join("inc"), dir.join("sys")], msvc_source_dirs: vec![] };
        let r = scanner.scan(&[dir.join("src/main.c")], &sp);
        let rel: Vec<String> = r.files.iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(rel, vec!["comp/c.h", "inc/a.h", "inc/sub/b.h", "src/local.h", "src/main.c", "sys/a.h"]);
        assert_eq!(r.dirs.len(), 1);
        assert_eq!(r.warnings.len(), 1);
        // Second scan hits the cache.
        let before = cache.stats.lock().unwrap().resolution_hits;
        scanner.scan(&[dir.join("src/main.c")], &sp);
        assert!(cache.stats.lock().unwrap().resolution_hits > before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rsp_expansion() {
        let dir = std::env::temp_dir().join(format!("ninja_rs_rsp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rsp"), "-Ifoo \"-I bar\" @b.rsp").unwrap();
        std::fs::write(dir.join("b.rsp"), "-DX").unwrap();
        let (args, files) = expand_response_files(&s(&["cc", "@a.rsp", "x.c"]), &dir, false);
        assert_eq!(args, s(&["cc", "-Ifoo", "-I bar", "-DX", "x.c"]));
        assert_eq!(files.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
