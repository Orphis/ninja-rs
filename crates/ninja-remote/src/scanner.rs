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
//! directory containing the file is sent whole, with a warning. Files are
//! lexed as the preprocessor reads them (`cpp_deps::lex`): a `#include` in a
//! comment or a string is not a directive. With MSVC's lookup (cl,
//! clang-cl, Clang for the MSVC ABI), `#include "..."` also searches the
//! directory of every file reached.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use cpp_deps::lex::{DirKind, Kind, P, Token};
/// Lexical normalization (removes `.` and `..` components).
pub use cpp_deps::scan::normalize;

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
    /// Other files named in arguments (e.g. `-fprofile-use=`,
    /// `-fmodule-file=`, see `cpp_deps::driver::read_paths`), sent as
    /// inputs.
    pub extra_files: Vec<PathBuf>,
    /// Directories named in arguments that the compile reads (GCC's
    /// `-fprofile-use=<dir>`), sent whole.
    pub extra_dirs: Vec<PathBuf>,
    /// `--embed-dir`: searched by `#embed`.
    pub embed_dirs: Vec<PathBuf>,
    /// MSVC's lookup (cl, clang-cl, and Clang for the MSVC ABI): `#include
    /// "..."` searches every includer's directory.
    pub msvc_lookup: bool,
}

/// Whether a target triple is the MSVC ABI's (Clang's default environment
/// for Windows: `x86_64-pc-windows-msvc`, `x86_64-windows`), not MinGW's,
/// Cygwin's or Itanium's.
fn msvc_target(triple: &str) -> bool {
    let t = triple.to_ascii_lowercase();
    (t.contains("windows") || t.contains("win32")) && !["gnu", "cygnus", "cygwin", "mingw", "itanium"].iter().any(|e| t.contains(e))
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
    // Forced includes as given, and `-f[no-]ms-compatibility`'s last.
    let (mut forced, mut ms_compat): (Vec<String>, Option<bool>) = (Vec::new(), None);
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
        // (Files flags name are `read_paths`'.)
        if opt(&["-include-pch"], &mut i).is_some() {
            continue;
        }
        if let Some(v) = opt(&["--embed-dir"], &mut i) {
            if let Some(v) = v {
                info.embed_dirs.push(abs(&v, cwd));
            }
            continue;
        }
        if let Some(v) = opt(&["-include", "-imacros", "--include"], &mut i) {
            forced.extend(v);
            continue;
        }
        if msvc && let Some(v) = opt(&["/FI", "-FI"], &mut i) {
            forced.extend(v);
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
            "-nostdinc++" | "-m32" | "-m64" | "-mx32" | "-pthread" | "-fms-extensions" | "-fms-compatibility" | "-fno-ms-compatibility" => {
                match a.as_str() {
                    "-fms-compatibility" => ms_compat = Some(true),
                    "-fno-ms-compatibility" => ms_compat = Some(false),
                    _ => {}
                }
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
        // cc1's spellings through the driver: "-Xclang -include -Xclang F"
        // (CMake's precompiled headers with Clang), "-Xclang -include-pch",
        // "-Xclang -ivfsoverlay".
        if a == "-Xclang"
            && argv.get(i + 1).is_some_and(|n| n == "-Xclang")
            && let Some(flag) = argv.get(i).filter(|f| matches!(f.as_str(), "-include" | "-imacros" | "-include-pch" | "-ivfsoverlay"))
        {
            if matches!(flag.as_str(), "-include" | "-imacros") {
                forced.extend(argv.get(i + 2).cloned());
            }
            i += 3;
            continue;
        }
        if opt(&["-ivfsoverlay", "-vfsoverlay", "--vfsoverlay"], &mut i).is_some() {
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
    // Clang for the MSVC ABI: given as the target, or the host's (Windows
    // hosts' Clang targets it), unless flags say otherwise.
    let target = info.builtin_flags.iter().find_map(|f| f.strip_prefix("--target="));
    let clang = argv.first().is_some_and(|a| a.rsplit(['/', '\\']).next().unwrap_or(a).to_ascii_lowercase().contains("clang"));
    info.msvc_lookup = msvc
        || ms_compat.unwrap_or(match target {
            Some(t) => msvc_target(t),
            None => cfg!(windows) && clang,
        });
    // A forced include is found in the working directory, else as an
    // `#include "..."` in the source (cl's /FI first next to it): every
    // place it may be.
    for name in forced {
        let p = abs(&name, cwd);
        if p.is_file() || Path::new(&name).is_absolute() {
            info.forced_includes.push(p);
            continue;
        }
        let source_dirs = info.sources.iter().filter_map(|s| s.parent()).filter(|_| info.msvc_lookup);
        let dirs: Vec<PathBuf> = source_dirs
            .map(Path::to_path_buf)
            .chain(info.quote_dirs.iter().chain(&info.user_dirs).chain(&info.system_dirs).chain(&info.after_dirs).cloned())
            .collect();
        let found: Vec<PathBuf> = dirs.iter().map(|d| normalize(&d.join(&name))).filter(|p| p.is_file()).collect();
        // None: where the build may make it.
        info.forced_includes.extend(if found.is_empty() { vec![p] } else { found });
    }
    let (files, dirs) = cpp_deps::driver::read_paths(argv, cwd);
    info.extra_files.extend(files);
    info.extra_dirs.extend(dirs);
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

fn header_like(name: &str) -> bool {
    if name.is_empty() || name.len() > 256 || name.contains(['\n', '*', '?', '%']) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    [".h", ".hh", ".hpp", ".hxx", ".h++", ".inc", ".inl", ".ipp", ".def", ".tcc", ".ixx", ".tlh", ".tli"].iter().any(|e| lower.ends_with(e))
}

/// The header names in a directive's tokens, and where each ends: `"x"`
/// (a string literal), `<x>` (one token after `#include`, else spelled as
/// tokens).
fn header_names(toks: &[Token]) -> Vec<(Include, usize)> {
    let mut names = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        match t.kind {
            Kind::HeaderName => names.push((Include { name: t.sym.as_str().to_string(), angled: true }, i + 1)),
            Kind::Str => {
                // (Without an encoding prefix.)
                let s = t.sym.as_str();
                if let Some(name) = s.trim_start_matches(|c| c != '"').strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                    names.push((Include { name: name.to_string(), angled: false }, i + 1));
                }
            }
            Kind::Punct(P::Lt) => {
                if let Some(end) = toks[i + 1..].iter().position(|t| t.is(P::Gt)) {
                    let mut name = String::new();
                    for (n, t) in toks[i + 1..i + 1 + end].iter().enumerate() {
                        if n > 0 && t.space {
                            name.push(' ');
                        }
                        name.push_str(t.sym.as_str());
                    }
                    names.push((Include { name, angled: true }, i + end + 2));
                    i += end + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    names
}

/// The operands of `__has_include(...)`, `__has_include_next(...)` and
/// `__has_embed(...)` in tokens.
fn has_include_operands(toks: &[Token]) -> Vec<Include> {
    let mut found = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if matches!(t.sym.as_str(), "__has_include" | "__has_include_next" | "__has_embed")
            && toks.get(i + 1).is_some_and(|p| p.is(P::LParen))
            && toks.get(i + 2).is_some_and(|o| o.kind == Kind::Str || o.is(P::Lt))
            && let Some((inc, _)) = header_names(&toks[i + 2..]).into_iter().next()
        {
            found.push(inc);
        }
    }
    found
}

/// Extracts include directives from a source file, lexed as the
/// preprocessor reads it (comments, strings and raw strings hide a `#`,
/// lines end at a lone carriage return, splices join them).
pub fn parse_directives(content: &[u8]) -> Directives {
    let fd = cpp_deps::lex::lex(content);
    let mut d = Directives::default();
    for dir in &fd.dirs {
        match dir.kind {
            DirKind::Include | DirKind::IncludeNext | DirKind::Import | DirKind::Embed => match &dir.toks[..] {
                [t, ..] if matches!(t.kind, Kind::HeaderName | Kind::Str) => d.includes.extend(header_names(&dir.toks[..1]).into_iter().map(|(inc, _)| inc)),
                // `#include MACRO`: a computed include.
                [t] if t.kind == Kind::Ident => d.computed.push(t.sym.as_str().to_string()),
                [] => {}
                _ => d.computed.push(String::new()),
            },
            DirKind::If | DirKind::Elif | DirKind::Ifdef | DirKind::Ifndef | DirKind::Elifdef | DirKind::Elifndef => {
                d.includes.extend(has_include_operands(&dir.toks));
            }
            DirKind::Define => {
                let Some(m) = &dir.mac else { continue };
                let name = m.name.as_str();
                // (`#define HAS_X __has_include(<x>)`.)
                d.includes.extend(has_include_operands(&m.body));
                if let [t] = &m.body[..]
                    && t.kind == Kind::Ident
                {
                    d.macro_aliases.push((name.to_string(), t.sym.as_str().to_string()));
                }
                let candidates: Vec<Include> = header_names(&m.body).into_iter().map(|(inc, _)| inc).filter(|inc| header_like(&inc.name)).collect();
                if !candidates.is_empty() {
                    d.header_macros.push(name.to_string());
                }
                d.define_candidates.extend(candidates);
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
    /// MSVC's lookup: `#include "..."` also searches the directories of
    /// the including files, which is taken as every file reached (a
    /// superset of any file's includers).
    pub msvc: bool,
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
    /// Compiles another scanner left to this one, by reason (see
    /// `fell_back`).
    fallbacks: Mutex<BTreeMap<String, usize>>,
}

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub files_parsed: usize,
    pub resolution_hits: usize,
    pub resolution_misses: usize,
}

impl ScanCache {
    /// Counts a compile the chosen scanner left to this one, and why;
    /// true the first time for such a reason (one without what is the
    /// compile's: where the scan stopped being exact, which header wasn't
    /// found), which is then worth telling.
    pub fn fell_back(&self, why: &str) -> bool {
        let reason = match why.strip_prefix("not exact (").and_then(|w| w.strip_suffix(')')) {
            Some(w) if w.ends_with(" not found") => "not exact (an #include not found)".to_string(),
            // ("file:line: what", or "file: what"; a drive's colon has no
            // space after it.)
            Some(w) => format!("not exact ({})", w.split_once(": ").map_or(w, |(_, what)| what)),
            None if why.starts_with("more than one input") => "more than one input".to_string(),
            None => why.to_string(),
        };
        let mut fallbacks = self.fallbacks.lock().unwrap();
        let n = fallbacks.entry(reason).or_default();
        *n += 1;
        *n == 1
    }

    /// How many compiles another scanner left to this one, and why: a line
    /// for the end of the build.
    pub fn fallback_summary(&self) -> Option<String> {
        let fallbacks = self.fallbacks.lock().unwrap();
        let total: usize = fallbacks.values().sum();
        let reasons: Vec<String> = fallbacks.iter().map(|(why, n)| format!("{n}: {why}")).collect();
        (total > 0).then(|| format!("{total} compiles were scanned conservatively ({})", reasons.join("; ")))
    }
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

/// MSVC's lookup, in a scan: the quoted names of the files reached, each
/// tried in the directory of every file reached (a superset of its
/// includers'), each pair once.
#[derive(Default)]
struct MsvcLookup {
    /// Tried with each other.
    dirs: Vec<PathBuf>,
    names: Vec<String>,
    /// Not tried yet.
    new_dirs: Vec<PathBuf>,
    new_names: Vec<String>,
    seen_dirs: HashSet<PathBuf>,
    seen_names: HashSet<String>,
}

impl MsvcLookup {
    /// A file reached, in `dir`, with its directives.
    fn reached(&mut self, dir: &Path, d: &Directives) {
        if self.seen_dirs.insert(dir.to_path_buf()) {
            self.new_dirs.push(dir.to_path_buf());
        }
        for inc in d.includes.iter().chain(&d.define_candidates).filter(|i| !i.angled && Path::new(&i.name).is_relative()) {
            if self.seen_names.insert(inc.name.clone()) {
                self.new_names.push(inc.name.clone());
            }
        }
    }
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
        if self.windows {
            // Names are case-insensitive.
            let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
            let p = lower(p);
            self.volatile_roots.iter().any(|r| p.starts_with(lower(r)))
        } else {
            self.volatile_roots.iter().any(|r| p.starts_with(r))
        }
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

    /// The files `inc` may name, from a file in `from_dir`: in the
    /// directories that may change during the build (`volatile`), or in the
    /// others.
    fn candidates(&self, inc: &Include, from_dir: &Path, sp: &SearchPath, volatile: bool, out: &mut Vec<PathBuf>) {
        let name = &inc.name;
        let np = Path::new(name);
        let mut try_path = |p: PathBuf| {
            if self.is_volatile(&p) == volatile && self.is_file(&p) {
                out.push(p);
            }
        };
        if np.is_absolute() || (self.windows && name.len() > 2 && name.as_bytes()[1] == b':') {
            try_path(normalize(np));
            return;
        }
        let quote = if inc.angled { None } else { Some(std::iter::once(from_dir).chain(sp.quote.iter().map(PathBuf::as_path))) };
        for d in quote.into_iter().flatten().chain(sp.angled.iter().map(PathBuf::as_path)) {
            try_path(normalize(&d.join(np)));
        }
    }

    /// The files MSVC's lookup may find that weren't looked for yet: the
    /// new names in every directory, the others in the new directories.
    fn msvc_lookup(&self, m: &mut MsvcLookup) -> Vec<PathBuf> {
        let (new_dirs, new_names) = (std::mem::take(&mut m.new_dirs), std::mem::take(&mut m.new_names));
        let mut found = Vec::new();
        let mut look = |dir: &Path, names: &[String]| {
            // A plain name is looked up in the directory's listing (taken
            // once), others as paths.
            let listing = if self.is_volatile(dir) { None } else { self.listing(dir) };
            for n in names {
                let plain = !n.contains(['/', '\\']);
                let p = normalize(&dir.join(n));
                let exists = match &listing {
                    Some(l) if plain => l.contains(&self.name_key(n)) && p.is_file(),
                    _ => self.is_file(&p),
                };
                if exists {
                    found.push(p);
                }
            }
        };
        for d in &m.dirs {
            look(d, &new_names);
        }
        for d in &new_dirs {
            look(d, &new_names);
            look(d, &m.names);
        }
        m.dirs.extend(new_dirs);
        m.names.extend(new_names);
        found
    }

    /// Resolves the includes of one file in the directories that don't
    /// change during the build (cached per file version and search path);
    /// those that may are searched every time (`scan`).
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
        for inc in d.includes.iter().chain(&d.define_candidates) {
            self.candidates(inc, from_dir, sp, false, &mut files);
        }
        files.sort();
        files.dedup();
        let r = Arc::new(Resolved { files, directives: d.clone() });
        self.cache.resolutions.lock().unwrap().insert(ck, r.clone());
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
        let volatile_search = sp.quote.iter().chain(&sp.angled).any(|d| self.is_volatile(d));
        // MSVC's lookup: the quoted names, and the directories of the files
        // reached, each tried with every other (see `msvc_lookup`).
        let mut msvc = MsvcLookup::default();
        loop {
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
                let from_dir = f.parent().unwrap_or(Path::new(""));
                // What the build may have made since, or make later, there.
                let mut found = r.files.clone();
                if volatile_search || self.is_volatile(from_dir) {
                    for inc in r.directives.includes.iter().chain(&r.directives.define_candidates) {
                        self.candidates(inc, from_dir, sp, true, &mut found);
                    }
                }
                if sp.msvc {
                    msvc.reached(from_dir, &r.directives);
                }
                queue.extend(found.into_iter().filter(|inc| !files.contains(inc)));
            }
            if !sp.msvc {
                break;
            }
            queue.extend(self.msvc_lookup(&mut msvc).into_iter().filter(|f| !files.contains(f)));
            if queue.is_empty() {
                break;
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
    // (`#embed <...>`'s: a superset of where headers are.)
    angled.extend(info.embed_dirs.iter().cloned());
    let mut seen = HashSet::new();
    angled.retain(|d| seen.insert(d.clone()));
    SearchPath { quote: info.quote_dirs.clone(), angled, msvc: info.msvc_lookup }
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
        let d = parse_directives(b"char c = '\\''; /* x */ #include \"not.h\"\n#include \"yes.h\"\n");
        assert_eq!(d.includes, vec![Include { name: "yes.h".into(), angled: false }]);
    }

    #[test]
    fn directives_as_lexed() {
        // Lines ending at a lone carriage return; a raw string with "/* in
        // it; #embed; __has_include in a macro; a digraph.
        let d = parse_directives(
            b"#pragma once\r#include \"cr.h\"\r\nauto s = R\"x(a\"/*)x\";\n#include \"after_raw.h\"\n#embed \"data.bin\" limit(4)\n\
              #define HAS_OPTIONAL __has_include(<optional>)\n%:include <digraph.h>\n",
        );
        let names: Vec<&str> = d.includes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["cr.h", "after_raw.h", "data.bin", "optional", "digraph.h"]);
    }

    #[test]
    fn computed_includes_of_known_macros() {
        let dir = test_dirs::base().join(format!("ninja_rs_scan_computed_{}", std::process::id()));
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
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: false };
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
        let dir = test_dirs::base().join(format!("ninja_rs_scan_{}", std::process::id()));
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
        let sp = SearchPath { quote: vec![dir.clone()], angled: vec![dir.join("inc"), dir.join("sys")], msvc: false };
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

    /// A tree of files under a new temporary directory.
    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ninja_rs_scan_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (f, text) in files {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), text).unwrap();
        }
        dir
    }

    fn rel(dir: &Path, files: &BTreeSet<PathBuf>) -> Vec<String> {
        files.iter().map(|p| p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/")).collect()
    }

    #[test]
    fn msvc_lookup() {
        // cl finds x.h next to a.h, an includer of b.h (not its directory,
        // nor the source's).
        let dir = tree(
            "msvc",
            &[
                ("src/main.cc", "#include <l1/a.h>\n"),
                ("inc/l1/a.h", "#include \"l2/b.h\"\n"),
                ("inc/l1/l2/b.h", "#include \"x.h\"\n"),
                ("inc/l1/x.h", ""),
                ("other/x.h", ""),
                ("src/pre/pre.h", "#include \"p.h\"\n"),
                ("src/pre/p.h", ""),
            ],
        );
        let scanned = |argv: &[&str]| {
            let info = parse_compile(&s(argv), &dir);
            let cache = ScanCache::default();
            let scanner = Scanner { cache: &cache, windows: cfg!(windows), volatile_roots: vec![] };
            let mut roots = info.sources.clone();
            roots.extend(info.forced_includes.iter().cloned());
            (info.msvc_lookup, rel(&dir, &scanner.scan(&roots, &search_path(&info, &[])).files))
        };
        let all = s(&["inc/l1/a.h", "inc/l1/l2/b.h", "inc/l1/x.h", "other/x.h", "src/main.cc"]);
        for argv in [
            &["cl.exe", "/Iinc", "/Iother", "/c", "src/main.cc"][..],
            &["clang-cl", "/Iinc", "/Iother", "/c", "src/main.cc"],
            &["clang++", "--target=x86_64-pc-windows-msvc", "-Iinc", "-Iother", "-c", "src/main.cc"],
            &["clang++", "-target", "x86_64-windows", "-Iinc", "-Iother", "-c", "src/main.cc"],
            &["clang++", "--target=x86_64-linux-gnu", "-fms-compatibility", "-Iinc", "-Iother", "-c", "src/main.cc"],
        ] {
            assert_eq!(scanned(argv), (true, all.clone()), "{argv:?}");
        }
        let gnu = s(&["inc/l1/a.h", "inc/l1/l2/b.h", "other/x.h", "src/main.cc"]);
        for argv in [
            &["clang++", "--target=x86_64-linux-gnu", "-Iinc", "-Iother", "-c", "src/main.cc"][..],
            &["clang++", "--target=x86_64-w64-windows-gnu", "-Iinc", "-Iother", "-c", "src/main.cc"],
            &["clang++", "--target=x86_64-pc-windows-msvc", "-fno-ms-compatibility", "-Iinc", "-Iother", "-c", "src/main.cc"],
        ] {
            assert_eq!(scanned(argv), (false, gnu.clone()), "{argv:?}");
        }
        // The host's target, for Clang without one.
        assert_eq!(scanned(&["clang++", "-Iinc", "-Iother", "-c", "src/main.cc"]).0, cfg!(windows));
        assert!(!scanned(&["g++", "-Iinc", "-Iother", "-c", "src/main.cc"]).0);
        // cl's /FI is looked up as `#include "..."` in the source.
        let (_, files) = scanned(&["cl.exe", "/FIpre/pre.h", "/c", "src/main.cc"]);
        assert!(files.contains(&"src/pre/pre.h".to_string()) && files.contains(&"src/pre/p.h".to_string()), "{files:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn volatile_headers() {
        // build/config.h, generated during the build, shadows inc/config.h.
        let dir = tree("volatile", &[("src/main.c", "#include \"config.h\"\n"), ("inc/config.h", "")]);
        let cache = ScanCache::default();
        // Volatile roots are compared as Windows compares names.
        let root = dir.join(if cfg!(windows) { "BUILD" } else { "build" });
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), volatile_roots: vec![root] };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("build"), dir.join("inc")], msvc: false };
        assert_eq!(rel(&dir, &scanner.scan(&[dir.join("src/main.c")], &sp).files), vec!["inc/config.h", "src/main.c"]);
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/config.h"), "").unwrap();
        assert_eq!(rel(&dir, &scanner.scan(&[dir.join("src/main.c")], &sp).files), vec!["build/config.h", "inc/config.h", "src/main.c"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_flags_name() {
        let dir = tree("flags", &[("a.cc", ""), ("prof.afdo", ""), ("gcda/a.gcda", ""), ("data/x.bin", "")]);
        let info = parse_compile(
            &s(&[
                "g++",
                "-fprofile-sample-use=prof.afdo",
                "-fprofile-use=gcda",
                "--embed-dir=data",
                "-Xclang",
                "-include-pch",
                "-Xclang",
                "pch.pch",
                "-c",
                "a.cc",
            ]),
            &dir,
        );
        assert_eq!(info.extra_files, vec![dir.join("prof.afdo"), dir.join("pch.pch")]);
        assert_eq!(info.extra_dirs, vec![dir.join("gcda")]);
        assert_eq!(info.embed_dirs, vec![dir.join("data")]);
        assert_eq!(info.sources, vec![dir.join("a.cc")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallbacks_by_reason() {
        let cache = ScanCache::default();
        assert_eq!(cache.fallback_summary(), None);
        // Told once per reason, whatever the compile's details.
        assert!(cache.fell_back("not exact (C:/src/a.h:12: __COUNTER__ in a condition or #include)"));
        assert!(!cache.fell_back("not exact (/src/b.h:3: __COUNTER__ in a condition or #include)"));
        assert!(cache.fell_back("not exact (gen/config.h not found)"));
        assert!(!cache.fell_back("not exact (other.h not found)"));
        assert!(cache.fell_back("unsupported flag -fmodules"));
        assert!(cache.fell_back("unsupported flag -I-"));
        assert!(!cache.fell_back("unsupported flag -fmodules"));
        assert_eq!(
            cache.fallback_summary().unwrap(),
            "7 compiles were scanned conservatively (2: not exact (__COUNTER__ in a condition or #include); \
             2: not exact (an #include not found); 1: unsupported flag -I-; 2: unsupported flag -fmodules)"
        );
    }

    #[test]
    fn rsp_expansion() {
        let dir = test_dirs::base().join(format!("ninja_rs_rsp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rsp"), "-Ifoo \"-I bar\" @b.rsp").unwrap();
        std::fs::write(dir.join("b.rsp"), "-DX").unwrap();
        let (args, files) = expand_response_files(&s(&["cc", "@a.rsp", "x.c"]), &dir, false);
        assert_eq!(args, s(&["cc", "-Ifoo", "-I bar", "-DX", "x.c"]));
        assert_eq!(files.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
