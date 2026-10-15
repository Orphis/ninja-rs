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
use std::sync::{Arc, Mutex, RwLock};
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
    /// "..."` searches every includer's directory. From the flags; the
    /// compiler's predefined macros decide (see `Scanner::msvc_lookup`).
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
/// The sysroot of headers: the last `-isysroot`, else the last
/// `--sysroot`.
fn header_sysroot(argv: &[String]) -> Option<String> {
    let (mut sysroot, mut isysroot) = (None, None);
    for (i, a) in argv.iter().enumerate() {
        match a.as_str() {
            "--sysroot" => sysroot = argv.get(i + 1).cloned(),
            "-isysroot" => isysroot = argv.get(i + 1).cloned(),
            _ => {
                if let Some(v) = a.strip_prefix("--sysroot=") {
                    sysroot = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("-isysroot").filter(|v| !v.is_empty()) {
                    isysroot = Some(v.to_string());
                }
            }
        }
    }
    isysroot.or(sysroot)
}

pub fn parse_compile(argv: &[String], cwd: &Path) -> CompileInfo {
    let mut info = CompileInfo { style: argv.first().and_then(|a| compiler_style(a, argv)), ..Default::default() };
    let msvc = info.style == Some(Style::Msvc);
    // cl.exe, not clang-cl (whose `/FI` is GCC's `-include`).
    let cl_exe = msvc && argv.first().is_some_and(|a| a.rsplit(['/', '\\']).next().unwrap_or(a).to_ascii_lowercase().trim_end_matches(".exe") == "cl");
    // A directory: `=` at its start stands for the sysroot, when given;
    // else GCC takes it as its configured one (none: `/inc`), Clang
    // literally (`cwd/=/inc`): both.
    let sysroot = header_sysroot(argv);
    let dirs = |v: &str| -> Vec<PathBuf> {
        match (v.strip_prefix('=').or_else(|| v.strip_prefix("$SYSROOT")), &sysroot) {
            (Some(rest), Some(root)) => vec![abs(&format!("{root}/{}", rest.trim_start_matches(['/', '\\'])), cwd)],
            (Some(rest), None) => vec![abs(rest, cwd), abs(v, cwd)],
            (None, _) => vec![abs(v, cwd)],
        }
    };
    // `-iprefix`, which `-iwithprefix` directories follow.
    let mut prefix = String::new();
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
        // A directory's flag, whose `=` stands for the sysroot.
        let dir_opt = |prefixes: &[&str], i: &mut usize| -> Option<Vec<PathBuf>> {
            for p in prefixes {
                if a == *p {
                    return Some(take(i).map(|v| dirs(&v)).unwrap_or_default());
                }
                if let Some(v) = a.strip_prefix(p)
                    && !v.is_empty()
                {
                    return Some(dirs(v));
                }
            }
            None
        };
        // Order matters: longer prefixes first.
        if let Some(v) = dir_opt(&["-isystem-after"], &mut i) {
            info.after_dirs.extend(v);
            continue;
        }
        if let Some(v) = dir_opt(&["-isystem", "-cxx-isystem", "-stdlib++-isystem", "-imsvc", "/imsvc", "/external:I", "-external:I"], &mut i) {
            info.system_dirs.extend(v);
            continue;
        }
        if let Some(v) = dir_opt(&["-iquote"], &mut i) {
            info.quote_dirs.extend(v);
            continue;
        }
        if let Some(v) = dir_opt(&["-idirafter"], &mut i) {
            info.after_dirs.extend(v);
            continue;
        }
        // The sysroot's, and the prefix's (Clang's and GCC's `-iprefix`).
        if let Some(v) = opt(&["-iwithsysroot"], &mut i) {
            if let Some(v) = v {
                info.system_dirs.extend(dirs(&format!("={v}")));
            }
            continue;
        }
        if let Some(v) = opt(&["-iprefix"], &mut i) {
            prefix = v.unwrap_or_default();
            continue;
        }
        if let Some(v) = opt(&["-iwithprefixbefore"], &mut i) {
            info.user_dirs.extend(v.map(|v| abs(&format!("{prefix}{v}"), cwd)));
            continue;
        }
        if let Some(v) = opt(&["-iwithprefix"], &mut i) {
            info.after_dirs.extend(v.map(|v| abs(&format!("{prefix}{v}"), cwd)));
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
        if let Some(v) = opt(&["--include-directory"], &mut i) {
            info.user_dirs.extend(v.map(|v| abs(&v, cwd)));
            continue;
        }
        if let Some(v) = dir_opt(&["-I", "/I"], &mut i) {
            // On POSIX "/I..." is a path, not a flag.
            if a.starts_with('/') && !msvc {
                info.sources.extend(source_if_any(&a, cwd));
                continue;
            }
            info.user_dirs.extend(v);
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
            || builtin("--config", &["--config"], "=", &mut i)
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
        if a == "-Xclang" {
            // cc1's MSVC compatibility, which the compiler's answer shows
            // (see `Scanner::msvc_lookup`).
            if let Some(v) = argv.get(i).filter(|v| matches!(v.as_str(), "-fms-compatibility" | "-fno-ms-compatibility")) {
                ms_compat = Some(v == "-fms-compatibility");
                info.builtin_flags.extend([a.clone(), v.clone()]);
            }
            i += 1;
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
    // Clang for the MSVC ABI: given as the target (the last), or the
    // host's (Windows hosts' Clang targets it), unless flags say otherwise.
    let target = info.builtin_flags.iter().rev().find_map(|f| f.strip_prefix("--target="));
    let clang = argv.first().is_some_and(|a| a.rsplit(['/', '\\']).next().unwrap_or(a).to_ascii_lowercase().contains("clang"));
    info.msvc_lookup = msvc
        || ms_compat.unwrap_or(match target {
            Some(t) => msvc_target(t),
            None => cfg!(windows) && clang,
        });
    // A forced include is found in the working directory (but by cl.exe,
    // which searches it as an `#include "..."` in the source), else as an
    // `#include "..."` in the source (MSVC's lookup next to it first): every
    // place it may be.
    for name in forced {
        let p = abs(&name, cwd);
        if Path::new(&name).is_absolute() || (!cl_exe && p.is_file()) {
            info.forced_includes.push(p);
            continue;
        }
        let source_dirs = info.sources.iter().filter_map(|s| s.parent()).filter(|_| info.msvc_lookup);
        // cl.exe's built-in directories are INCLUDE's.
        let include = if cl_exe { std::env::var_os("INCLUDE").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default() } else { Vec::new() };
        let dirs: Vec<PathBuf> = source_dirs
            .map(Path::to_path_buf)
            .chain(info.quote_dirs.iter().chain(&info.user_dirs).chain(&info.system_dirs).chain(&info.after_dirs).cloned())
            .chain(include)
            .collect();
        let found: Vec<PathBuf> = dirs.iter().map(|d| normalize(&d.join(&name))).filter(|p| p.is_file()).collect();
        // None: where the build may make it.
        info.forced_includes.extend(if found.is_empty() && !cl_exe { vec![p] } else { found });
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
    /// A candidate's `..` after a symbolic link (see `candidates`).
    pub linked: Option<PathBuf>,
}

/// A compiler's built-in directories, and whether it searches includers'
/// directories (when it said: see `Scanner::msvc_lookup`).
type Builtin = (Arc<Vec<PathBuf>>, Option<bool>);

/// Scanner caches, shared across actions (and builds, in a daemon).
#[derive(Default)]
pub struct ScanCache {
    directives: Mutex<HashMap<PathBuf, (u64, Option<SystemTime>, Arc<Directives>)>>,
    resolutions: Mutex<HashMap<(FileKey, u64), Arc<Resolved>>>,
    listings: RwLock<HashMap<PathBuf, Option<Arc<HashSet<String>>>>>,
    /// MSVC's lookups (see `Scanner::msvc_candidates`): by directory, the file
    /// each name is there (outside the build directory).
    msvc: RwLock<HashMap<PathBuf, HashMap<String, Option<PathBuf>>>>,
    /// The compilers' built-in directories, and whether they search
    /// includers' directories (see `Builtin`).
    builtin: Mutex<HashMap<String, Builtin>>,
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
    /// Directories whose files to send (computed includes the scan can't
    /// name: see `crate::store::dir_files`).
    pub dirs: BTreeSet<PathBuf>,
    pub warnings: Vec<String>,
    /// Why the compile can't run remotely, if so: a computed include in
    /// the build directory (whose files the build is writing), or a `..`
    /// after a symbolic link (which the compiler resolves elsewhere than a
    /// remote worker).
    pub local: Option<String>,
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
        if let Some(l) = self.cache.listings.read().unwrap().get(dir) {
            return l.clone();
        }
        let l = std::fs::read_dir(dir)
            .ok()
            .map(|rd| Arc::new(rd.filter_map(|e| e.ok()).map(|e| self.name_key(&e.file_name().to_string_lossy())).collect::<HashSet<_>>()));
        self.cache.listings.write().unwrap().insert(dir.to_path_buf(), l.clone());
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
    /// others. A `..` after a symbolic link, which the compiler resolves
    /// elsewhere than a remote worker (whose inputs are plain directories),
    /// is noted in `linked` when a file is there either way.
    fn candidates(&self, inc: &Include, from_dir: &Path, sp: &SearchPath, volatile: bool, out: &mut Vec<PathBuf>, linked: &mut Option<PathBuf>) {
        let name = &inc.name;
        let np = Path::new(name);
        let mut try_path = |joined: PathBuf| {
            let p = normalize(&joined);
            if linked.is_none()
                && let Some(link) = cpp_deps::scan::dotdot_after_link(&joined)
                && (joined.is_file() || p.is_file())
            {
                *linked = Some(link);
            }
            if self.is_volatile(&p) == volatile && self.is_file(&p) {
                out.push(p);
            }
        };
        if np.is_absolute() || (self.windows && name.len() > 2 && name.as_bytes()[1] == b':') {
            try_path(np.to_path_buf());
            return;
        }
        let quote = if inc.angled { None } else { Some(std::iter::once(from_dir).chain(sp.quote.iter().map(PathBuf::as_path))) };
        for d in quote.into_iter().flatten().chain(sp.angled.iter().map(PathBuf::as_path)) {
            try_path(d.join(np));
        }
    }

    /// The files MSVC's lookup may find that weren't looked for yet: the
    /// new names in every directory, the others in the new directories.
    fn msvc_candidates(&self, m: &mut MsvcLookup) -> Vec<PathBuf> {
        let (new_dirs, new_names) = (std::mem::take(&mut m.new_dirs), std::mem::take(&mut m.new_names));
        let mut found = Vec::new();
        let mut look = |dir: &Path, names: &[String]| {
            // The file a name is in a directory, looked up once (outside the
            // build directory): a plain name in the directory's listing,
            // others as paths.
            let find = |n: &String, listing: &Option<Arc<HashSet<String>>>| {
                let p = normalize(&dir.join(n));
                let exists = match listing {
                    Some(l) if !n.contains(['/', '\\']) => l.contains(&self.name_key(n)) && p.is_file(),
                    _ => self.is_file(&p),
                };
                exists.then_some(p)
            };
            if self.is_volatile(dir) {
                found.extend(names.iter().filter_map(|n| find(n, &None)));
                return;
            }
            let mut unknown = Vec::new();
            if let Some(known) = self.cache.msvc.read().unwrap().get(dir) {
                for n in names {
                    match known.get(n) {
                        Some(f) => found.extend(f.iter().cloned()),
                        None => unknown.push(n),
                    }
                }
            } else {
                unknown.extend(names);
            }
            if unknown.is_empty() {
                return;
            }
            let listing = self.listing(dir);
            let looked: Vec<(String, Option<PathBuf>)> = unknown.into_iter().map(|n| (n.clone(), find(n, &listing))).collect();
            found.extend(looked.iter().filter_map(|(_, f)| f.clone()));
            self.cache.msvc.write().unwrap().entry(dir.to_path_buf()).or_default().extend(looked);
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
        let (mut files, mut linked) = (Vec::new(), None);
        for inc in d.includes.iter().chain(&d.define_candidates) {
            self.candidates(inc, from_dir, sp, false, &mut files, &mut linked);
        }
        files.sort();
        files.dedup();
        let r = Arc::new(Resolved { files, directives: d.clone(), linked });
        self.cache.resolutions.lock().unwrap().insert(ck, r.clone());
        r
    }

    /// Finds every file reachable from the sources and forced includes.
    pub fn scan(&self, roots: &[PathBuf], sp: &SearchPath) -> ScanResult {
        let sp_id = sp.id();
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        let mut warnings = Vec::new();
        let mut linked = None;
        let mut queue: VecDeque<PathBuf> = roots.iter().cloned().collect();
        // Computed includes, and the macros the scanned files define.
        let mut computed: Vec<(PathBuf, Vec<String>)> = Vec::new();
        let mut header_macros: HashSet<String> = HashSet::new();
        let mut aliases: Vec<(String, String)> = Vec::new();
        let volatile_search = sp.quote.iter().chain(&sp.angled).any(|d| self.is_volatile(d));
        // MSVC's lookup: the quoted names, and the directories of the files
        // reached, each tried with every other (see `msvc_candidates`).
        let mut msvc = MsvcLookup::default();
        loop {
            while let Some(f) = queue.pop_front() {
                if !files.insert(f.clone()) {
                    continue;
                }
                let r = self.resolve_file(&f, sp, sp_id);
                if linked.is_none() {
                    linked.clone_from(&r.linked);
                }
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
                        self.candidates(inc, from_dir, sp, true, &mut found, &mut linked);
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
            queue.extend(self.msvc_candidates(&mut msvc).into_iter().filter(|f| !files.contains(f)));
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
        // header was followed as a candidate. Otherwise its directory's
        // files go (not its subdirectories'); not those of the build
        // directory, which the build is writing (objects, logs): the
        // compile runs locally then.
        let mut local = linked.map(|l| format!("`..` after the symbolic link {}, which the compiler resolves elsewhere than a remote worker", l.display()));
        for (f, macros) in computed {
            if macros.iter().all(|m| header_macros.contains(m)) {
                continue;
            }
            let Some(parent) = f.parent() else { continue };
            if self.is_volatile(parent) {
                local.get_or_insert_with(|| format!("computed #include in {}, in the build directory", f.display()));
            } else if dirs.insert(parent.to_path_buf()) {
                warnings.push(format!("computed #include in {}: sending the files of {}", f.display(), parent.display()));
            }
        }
        ScanResult { files, dirs, warnings, local }
    }

    /// Whether the compiler searches includers' directories (MSVC's lookup),
    /// as its predefined macros say (`_MSC_VER` and `__clang__`: Clang for
    /// the MSVC ABI, which its driver's name, configuration files or cc1's
    /// flags may choose) when `builtin_dirs` could ask it; else from the
    /// flags.
    pub fn msvc_lookup(&self, compiler: &Path, info: &CompileInfo) -> bool {
        let answer = self.cache.builtin.lock().unwrap().get(&builtin_key(compiler, info)).and_then(|(_, msvc)| *msvc);
        info.style == Some(Style::Msvc) || answer.unwrap_or(info.msvc_lookup)
    }

    /// The compiler's built-in include directories, from `-v` output
    /// (cached per compiler, relevant flags and language).
    pub fn builtin_dirs(&self, compiler: &Path, info: &CompileInfo) -> Result<Arc<Vec<PathBuf>>, String> {
        let key = builtin_key(compiler, info);
        if let Some((d, _)) = self.cache.builtin.lock().unwrap().get(&key) {
            return Ok(d.clone());
        }
        // (Asked with `-nostdinc` too, for `msvc_lookup`.)
        let (dirs, msvc) = match discover_builtin_dirs(compiler, info, info.lang.unwrap_or(Lang::Cxx)) {
            Err(_) if info.no_builtin_includes => (Vec::new(), None),
            found => found?,
        };
        let dirs = Arc::new(if info.no_builtin_includes { Vec::new() } else { dirs });
        self.cache.builtin.lock().unwrap().insert(key, (dirs.clone(), msvc));
        Ok(dirs)
    }
}

/// The key of a compiler's built-in directories: it, the flags that change
/// them, and the language.
fn builtin_key(compiler: &Path, info: &CompileInfo) -> String {
    format!("{}|{:?}|{:?}|{}", compiler.display(), info.style, info.lang.unwrap_or(Lang::Cxx), info.builtin_flags.join("\u{1}"))
}

/// Runs `<compiler> -E -v` on an empty input and parses the include search
/// list it prints; for GCC-style drivers, with `-dM`: whether the compiler
/// is Clang for the MSVC ABI (`_MSC_VER` and `__clang__`), which searches
/// includers' directories. Killed when it takes too long.
pub fn discover_builtin_dirs(compiler: &Path, info: &CompileInfo, lang: Lang) -> Result<(Vec<PathBuf>, Option<bool>), String> {
    discover_builtin_dirs_for(compiler, info, lang, cpp_deps::probe::TIMEOUT)
}

fn discover_builtin_dirs_for(compiler: &Path, info: &CompileInfo, lang: Lang, timeout: std::time::Duration) -> Result<(Vec<PathBuf>, Option<bool>), String> {
    let base = compiler.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if base == "cl" {
        // MSVC's cl.exe has no -v; it uses the INCLUDE environment variable.
        return Ok((std::env::var_os("INCLUDE").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default(), None));
    }
    let mut cmd = std::process::Command::new(compiler);
    cmd.args(&info.builtin_flags);
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let gcc = info.style != Some(Style::Msvc);
    if !gcc {
        cmd.arg("-v").arg("-E").arg(if lang == Lang::C { "/TC" } else { "/TP" }).arg(null);
    } else {
        let l = match lang {
            Lang::C => "c",
            Lang::Cxx => "c++",
            Lang::ObjC => "objective-c",
            Lang::ObjCxx => "objective-c++",
            Lang::Asm => "assembler-with-cpp",
        };
        cmd.arg("-E").arg("-x").arg(l).arg("-v").arg("-dM").arg(null);
    }
    let stdout = if gcc { std::process::Stdio::piped() } else { std::process::Stdio::null() };
    cmd.stdin(std::process::Stdio::null()).stdout(stdout).stderr(std::process::Stdio::piped());
    let out = ninja_os::process::output(cmd, None, timeout).map_err(|e| format!("running {} -v: {e}", compiler.display()))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let dirs = parse_search_list(&text);
    if dirs.is_empty() && !info.no_builtin_includes {
        return Err(format!("could not find the include search list in `{} -v` output (exit {:?})", compiler.display(), out.status.code()));
    }
    let macros = String::from_utf8_lossy(&out.stdout);
    let defined = |m: &str| {
        macros.lines().any(|l| l.strip_prefix("#define ").and_then(|d| d.strip_prefix(m)).is_some_and(|rest| rest.is_empty() || rest.starts_with(' ')))
    };
    Ok((dirs, gcc.then(|| defined("_MSC_VER") && defined("__clang__"))))
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
        let dir = test_dirs::TestDir::new("ninja_rs_scan_computed");
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
    }

    #[test]
    fn parse_gcc_command() {
        let cwd = test_dirs::TestDir::new("ninja_rs_scan_parse");
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
        let dir = test_dirs::TestDir::new("ninja_rs_scan");
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
        let sp = SearchPath { quote: vec![dir.to_path_buf()], angled: vec![dir.join("inc"), dir.join("sys")], msvc: false };
        let r = scanner.scan(&[dir.join("src/main.c")], &sp);
        let rel: Vec<String> = r.files.iter().map(|p| p.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(rel, vec!["comp/c.h", "inc/a.h", "inc/sub/b.h", "src/local.h", "src/main.c", "sys/a.h"]);
        assert_eq!(r.dirs.len(), 1);
        assert_eq!(r.warnings.len(), 1);
        // Second scan hits the cache.
        let before = cache.stats.lock().unwrap().resolution_hits;
        scanner.scan(&[dir.join("src/main.c")], &sp);
        assert!(cache.stats.lock().unwrap().resolution_hits > before);
    }

    /// A tree of files under a new test directory.
    fn tree(name: &str, files: &[(&str, &str)]) -> test_dirs::TestDir {
        let dir = test_dirs::TestDir::new(&format!("ninja_rs_scan_{name}"));
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

    /// A fake GCC-style compiler in `dir` named `name`, whose `-E -v -dM`
    /// says the search list `inc` and prints `macros`.
    fn fake_compiler(dir: &Path, name: &str, macros: &[&str]) -> PathBuf {
        let inc = dir.join("builtin");
        std::fs::create_dir_all(&inc).unwrap();
        let list = format!("#include <...> search starts here:\n {}\nEnd of search list.\n", inc.display());
        let defines: String = macros.iter().map(|m| format!("#define {m} 1\n")).collect();
        if cfg!(windows) {
            let echo = |text: &str, to: &str| text.lines().map(|l| format!("@echo {}{to}\r\n", if l.is_empty() { "." } else { l })).collect::<String>();
            let c = dir.join(format!("{name}.cmd"));
            std::fs::write(&c, format!("{}{}", echo(&list.replace('<', "^<").replace('>', "^>"), " 1>&2"), echo(&defines, ""))).unwrap();
            c
        } else {
            let c = dir.join(name);
            std::fs::write(&c, format!("#!/bin/sh\nprintf '%s' '{list}' >&2\nprintf '%s' '{defines}'\n")).unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&c, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            c
        }
    }

    /// Whether the compiler searches includers' directories is the
    /// compiler's to say (Clang for the MSVC ABI: a driver named after its
    /// target, a configuration file, cc1's flags), not the flags'.
    #[test]
    fn msvc_lookup_from_the_compiler() {
        let dir = test_dirs::TestDir::new("ninja_rs_scan_msvc_compiler");
        std::fs::write(dir.join("a.cc"), "").unwrap();
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), volatile_roots: vec![] };
        let lookup = |compiler: &Path, flags: &[&str]| {
            let argv: Vec<String> = [compiler.to_string_lossy().into_owned()].into_iter().chain(s(flags)).chain(s(&["-c", "a.cc"])).collect();
            let info = parse_compile(&argv, &dir);
            assert_eq!(*scanner.builtin_dirs(compiler, &info).unwrap(), vec![dir.join("builtin")]);
            scanner.msvc_lookup(compiler, &info)
        };
        let msvc = fake_compiler(&dir, "x86_64-pc-windows-msvc-clang++", &["__clang__", "_MSC_VER"]);
        assert!(lookup(&msvc, &[]));
        let gnu = fake_compiler(&dir, "clang++", &["__clang__"]);
        assert!(!lookup(&gnu, &["--target=x86_64-pc-windows-msvc"]));
        let gcc = fake_compiler(&dir, "g++", &["__GNUC__"]);
        assert!(!lookup(&gcc, &[]));
    }

    /// cl.exe searches a relative `/FI` as an `#include "..."` in the source,
    /// never the working directory (unless it's one of those); clang-cl, as
    /// `-include`, the working directory first.
    #[test]
    fn cl_forced_includes() {
        let dir = tree("cl_forced", &[("build/pre.h", "/*build*/\n"), ("inc/pre.h", "/*inc*/\n"), ("src/main.cc", ""), ("src/pre2.h", "")]);
        let forced = |compiler: &str, fi: &str| {
            let argv = s(&[compiler, "/nologo", &format!("/FI{fi}"), "/I../inc", "/c", "../src/main.cc"]);
            rel(&dir, &parse_compile(&argv, &dir.join("build")).forced_includes.into_iter().collect())
        };
        assert_eq!(forced("cl.exe", "pre.h"), ["inc/pre.h"]);
        assert_eq!(forced("cl.exe", "pre2.h"), ["src/pre2.h"]);
        assert_eq!(forced("clang-cl", "pre.h"), ["build/pre.h"]);
    }

    /// `=` at the start of a directory stands for the sysroot; without one,
    /// GCC's default and Clang's literal reading are both searched. And the
    /// directories other flags make.
    #[test]
    fn sysroot_and_prefix_dirs() {
        let cwd = test_dirs::TestDir::new("ninja_rs_scan_sysroot");
        let info = |flags: &[&str]| {
            let argv: Vec<String> = s(&["clang++"]).into_iter().chain(s(flags)).chain(s(&["-c", "a.cc"])).collect();
            parse_compile(&argv, &cwd)
        };
        let a = |p: &str| abs(p, &cwd);
        let i = info(&["--sysroot=sr", "-I=/inc", "-isystem=/sys", "-iquote", "=/q", "-idirafter=/after", "-iwithsysroot", "/ws"]);
        assert_eq!((i.user_dirs, i.quote_dirs, i.after_dirs), (vec![a("sr/inc")], vec![a("sr/q")], vec![a("sr/after")]));
        assert_eq!(i.system_dirs, vec![a("sr/sys"), a("sr/ws")]);
        assert_eq!(info(&["-I=/inc", "-isysroot", "isr", "--sysroot=sr"]).user_dirs, vec![a("isr/inc")]);
        assert_eq!(info(&["-I=/inc"]).user_dirs, vec![a("/inc"), a("=/inc")]);
        let i = info(&["-stdlib++-isystem", "cxx", "-iprefix", "pre/", "-iwithprefix", "after", "-iwithprefixbefore", "before"]);
        assert_eq!((i.system_dirs, i.after_dirs, i.user_dirs), (vec![a("cxx")], vec![a("pre/after")], vec![a("pre/before")]));
    }

    /// A computed include the scan can't name sends its directory's files,
    /// not its subdirectories' (a source root holding the build directory),
    /// and, in the build directory, makes the compile run locally.
    #[test]
    fn computed_include_directories() {
        let dir = tree(
            "computed_dirs",
            &[
                ("main.cpp", "#define STR(x) #x\n#define HDR(x) STR(x.h)\n#include HDR(gen)\n"),
                ("gen.h", ""),
                ("build/.ninja_log", "LOG"),
                ("build/app.exe", "EXE"),
                ("build/gen/main.cpp", "#include HDR(gen)\n"),
            ],
        );
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), volatile_roots: vec![dir.join("build")] };
        let sp = SearchPath::default();
        let r = scanner.scan(&[dir.join("main.cpp")], &sp);
        assert!(r.local.is_none());
        assert_eq!(rel(&dir, &crate::store::dir_files(&r.dirs).unwrap().into_iter().collect()), ["gen.h", "main.cpp"]);
        let r = scanner.scan(&[dir.join("build/gen/main.cpp")], &sp);
        assert!(r.local.is_some_and(|why| why.contains("build directory")));
        // Too many to send: run locally.
        let big = dir.join("big");
        std::fs::create_dir_all(&big).unwrap();
        for i in 0..=crate::store::DIR_FILES_MAX {
            std::fs::write(big.join(format!("{i}.h")), "").unwrap();
        }
        assert!(crate::store::dir_files(&BTreeSet::from([big])).is_err());
    }

    /// MSVC's lookup across rounds: a name seen early is found in a
    /// directory reached later, and a name seen later in a directory reached
    /// early.
    #[test]
    fn msvc_lookup_rounds() {
        let dir = tree(
            "msvc_rounds",
            &[
                // Round 1: main.cc reaches inc/l1/a.h; its "b.h" is found next
                // to main.cc (an includer's directory).
                ("src/main.cc", "#include <l1/a.h>\n#include \"zz.h\"\n"),
                ("inc/l1/a.h", "#include \"b.h\"\n"),
                ("src/b.h", "#include \"deep/c.h\"\n"),
                // Round 2: deep/c.h is reached; zz.h, named in round 1, is in
                // its directory, and y.h, named now, in inc/l1 (round 1's).
                ("src/deep/c.h", "#include \"y.h\"\n"),
                ("src/deep/zz.h", ""),
                ("inc/l1/y.h", ""),
            ],
        );
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), volatile_roots: vec![] };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: true };
        let files = rel(&dir, &scanner.scan(&[dir.join("src/main.cc")], &sp).files);
        assert_eq!(files, ["inc/l1/a.h", "inc/l1/y.h", "src/b.h", "src/deep/c.h", "src/deep/zz.h", "src/main.cc"]);
        // Looked up once: a scan again finds what was found.
        std::fs::remove_file(dir.join("inc/l1/y.h")).unwrap();
        let again = rel(&dir, &scanner.scan(&[dir.join("src/main.cc")], &sp).files);
        assert_eq!(again, files);
    }

    /// A `..` after a symbolic link: the compiler reads elsewhere than a
    /// remote worker would: local.
    #[test]
    fn dotdot_after_links() {
        let dir = tree("dotdot", &[("main.c", "#include \"lnk/a.h\"\n"), ("real/sub/a.h", "#include \"../b.h\"\n"), ("b.h", ""), ("real/b.h", "")]);
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("real/sub"), dir.join("lnk")).unwrap();
        #[cfg(windows)]
        assert!(
            std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(normalize(&dir.join("lnk")))
                .arg(normalize(&dir.join("real/sub")))
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), volatile_roots: vec![] };
        let r = scanner.scan(&[dir.join("main.c")], &SearchPath::default());
        assert!(r.local.is_some_and(|why| why.contains("symbolic link")));
    }

    /// The compiler's run for its built-in directories is given up on when
    /// it hangs.
    #[test]
    fn builtin_dirs_time_out() {
        let dir = test_dirs::TestDir::new("ninja_rs_scan_hung");
        let compiler = if cfg!(windows) {
            let c = dir.join("hung.cmd");
            std::fs::write(&c, "@ping -n 30 127.0.0.1 >NUL\r\n").unwrap();
            c
        } else {
            let c = dir.join("hung");
            std::fs::write(&c, "#!/bin/sh\nsleep 30\n").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&c, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            c
        };
        let info = CompileInfo { style: Some(Style::Gcc), ..Default::default() };
        let e = discover_builtin_dirs_for(&compiler, &info, Lang::C, std::time::Duration::from_millis(300)).unwrap_err();
        assert!(e.contains("timed out"), "{e}");
    }

    #[test]
    fn rsp_expansion() {
        let dir = test_dirs::TestDir::new("ninja_rs_rsp");
        std::fs::write(dir.join("a.rsp"), "-Ifoo \"-I bar\" @b.rsp").unwrap();
        std::fs::write(dir.join("b.rsp"), "-DX").unwrap();
        let (args, files) = expand_response_files(&s(&["cc", "@a.rsp", "x.c"]), &dir, false);
        assert_eq!(args, s(&["cc", "-Ifoo", "-I bar", "-DX", "x.c"]));
        assert_eq!(files.len(), 2);
    }
}
