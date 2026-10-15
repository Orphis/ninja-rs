//! C/C++ include scanner (like reclient's and Siso's): finds the headers a
//! compile reads by following `#include` directives, without running the
//! preprocessor.
//!
//! It is conservative: preprocessor conditionals are ignored, every
//! directive is followed, and every directory of the search path that
//! contains a candidate contributes it (which also covers `#include_next`).
//! Strings in `#define` lines that look like header names are also treated
//! as candidates (they are often used as computed includes). A computed
//! include (`#include MACRO`) is covered when the scanned files or the
//! command line define the macro as a header name (directly or through
//! other macros); otherwise the files under the directory containing the
//! file, and under those its path names, are sent with what they include,
//! with a warning (see `Scanner::computed_files`). Files are lexed as the
//! preprocessor reads them (`cpp_deps::lex`): a `#include` in a comment or
//! a string is not a directive. With MSVC's lookup (cl, clang-cl, Clang for
//! the MSVC ABI), `#include "..."` also searches the directory of every
//! file reached. Directory listings, and what's found from them, are kept
//! for the directories' stamps (see `ScanCache`).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use ninja_os::FileStamp;

use cpp_deps::lex::{DirKind, Kind, P, Token};
/// Lexical normalization (removes `.` and `..` components).
pub use cpp_deps::scan::normalize;

use crate::cmdline::split_rsp;
use crate::store::Walk;

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
    /// The macros the command line defines or undefines (`-D`, `-U`).
    pub defines: Vec<String>,
    /// The command line's definitions (`-D`), as directives: a header name
    /// one is defined as is a candidate (mbedTLS's
    /// `-DMBEDTLS_CONFIG_FILE="my_config.h"`, see `Scanner::scan_compile`).
    pub macros: Directives,
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
    /// compiler's predefined macros may add it (see `Scanner::msvc_lookup`).
    pub msvc_lookup: bool,
    /// The forced includes as given, where `forced_includes` are placed
    /// from (see `place_forced_includes`).
    forced: Vec<String>,
    cwd: PathBuf,
    /// The compiler is cl.exe (not clang-cl, whose `/FI` is `-include`).
    cl_exe: bool,
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
        let u: Vec<u16> = content[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
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
    // The definitions, as `#define` lines.
    let mut definitions = String::new();
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
        if let Some(v) = opt(&["-D", "-U"], &mut i).or_else(|| if msvc { opt(&["/D", "/U"], &mut i) } else { None }) {
            if let Some(v) = &v
                && a[1..].starts_with('D')
            {
                // `NAME=VALUE` (cl's `NAME#VALUE`), `NAME` (1).
                let (name, value) = v.split_once('=').or_else(|| if msvc { v.split_once('#') } else { None }).unwrap_or((v, "1"));
                if !value.contains(['\n', '\r']) {
                    definitions.push_str(&format!("#define {name} {value}\n"));
                }
            }
            info.defines.extend(v.map(|v| v.split(['=', '(', '#']).next().unwrap_or_default().to_string()));
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
    (info.forced, info.cwd, info.cl_exe) = (forced, cwd.to_path_buf(), cl_exe);
    info.macros = parse_directives(definitions.as_bytes());
    info.place_forced_includes(info.msvc_lookup);
    let (files, dirs) = cpp_deps::driver::read_paths(argv, cwd);
    info.extra_files.extend(files);
    info.extra_dirs.extend(dirs);
    info
}

impl CompileInfo {
    /// Finds the forced includes, with MSVC's lookup or not (the flags'
    /// when parsed; the compiler may say otherwise: see
    /// `Scanner::msvc_lookup`). One is found in the working directory (but
    /// by cl.exe, which searches it as an `#include "..."` in the source),
    /// else as an `#include "..."` in the source (MSVC's lookup next to it
    /// first): every place it may be.
    pub fn place_forced_includes(&mut self, msvc_lookup: bool) {
        self.forced_includes.clear();
        for name in &self.forced {
            let p = abs(name, &self.cwd);
            if Path::new(name).is_absolute() || (!self.cl_exe && p.is_file()) {
                self.forced_includes.push(p);
                continue;
            }
            let source_dirs = self.sources.iter().filter_map(|s| s.parent()).filter(|_| msvc_lookup);
            // cl.exe's built-in directories are INCLUDE's.
            let include = if self.cl_exe { std::env::var_os("INCLUDE").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default() } else { Vec::new() };
            let dirs: Vec<PathBuf> = source_dirs
                .map(Path::to_path_buf)
                .chain(self.quote_dirs.iter().chain(&self.user_dirs).chain(&self.system_dirs).chain(&self.after_dirs).cloned())
                .chain(include)
                .collect();
            let found: Vec<PathBuf> = dirs.iter().map(|d| normalize(&d.join(name))).filter(|p| p.is_file()).collect();
            // None: where the build may make it.
            self.forced_includes.extend(if found.is_empty() && !self.cl_exe { vec![p] } else { found });
        }
    }
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
    /// The directories each computed include (as `computed`) spells out
    /// (see `path_prefixes`).
    pub computed_paths: Vec<Option<Vec<String>>>,
    /// The object-like macros defined (see `Define`).
    pub defines: Vec<Define>,
    /// Where each is in `defines`, by name.
    pub by_name: HashMap<String, Vec<usize>>,
}

/// An object-like macro's definition, as computed includes see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Define {
    pub name: String,
    /// Defined as names joined by slashes: another macro's name, a path
    /// (`#define BOOST_FT_al_path boost/function_types/detail/components_impl`).
    pub path: Option<String>,
    /// The directories its body spells out (see `path_prefixes`).
    pub paths: Option<Vec<String>>,
}

/// Tokens that are names joined by slashes, without spaces.
fn plain_path(toks: &[Token]) -> Option<String> {
    let mut path = String::new();
    for (i, t) in toks.iter().enumerate() {
        let name = i % 2 == 0;
        if (i > 0 && t.space) || (name && t.kind != Kind::Ident) || (!name && !t.is(P::Slash)) {
            return None;
        }
        path.push_str(t.sym.as_str());
    }
    (toks.len() % 2 == 1).then_some(path)
}

/// The directories tokens spell out, which a header name made of them is
/// under: names joined by slashes, without spaces, up to the last slash
/// (`BOOST_PP_STRINGIZE(boost/mpl/list/AUX778076_LIST_HEADER)` names a
/// header under `boost/mpl/list/`; macros among the names are the
/// scanner's, see `Macros::substituted`). None when a slash is elsewhere
/// (`../lib/x.h`, not after a name): a path that can't be read.
fn path_prefixes(toks: &[Token]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut i = 0;
    // Where the last path read ends: a slash before is in it, under its
    // directory (`lib/sub-dir/x.h`).
    let mut path_end = 0;
    while i < toks.len() {
        // Not a part of a longer path (after `/`, `.`, `..`).
        let starts = toks[i].kind == Kind::Ident && (i == 0 || toks[i - 1].is(P::LParen) || toks[i - 1].is(P::Comma) || toks[i - 1].is(P::Lt));
        if !starts {
            if toks[i].is(P::Slash) && i >= path_end {
                return None;
            }
            i += 1;
            continue;
        }
        let (mut j, mut path, mut prefix) = (i, String::new(), None);
        while toks.get(j).is_some_and(|t| t.kind == Kind::Ident && (j == i || !t.space)) {
            path.push_str(toks[j].sym.as_str());
            j += 1;
            if !toks.get(j).is_some_and(|t| t.is(P::Slash) && !t.space) {
                break;
            }
            path.push('/');
            j += 1;
            prefix = Some(path.clone());
        }
        if prefix.is_some() {
            // The rest of the path, to a space or a delimiter.
            let mut end = j;
            while toks.get(end).is_some_and(|t| !t.space && ![P::LParen, P::RParen, P::Comma, P::Gt].iter().any(|&p| t.is(p))) {
                end += 1;
            }
            path_end = end;
        }
        out.extend(prefix);
        i = j.max(i + 1);
    }
    Some(out)
}

pub(crate) fn header_like(name: &str) -> bool {
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

/// The file `#pragma GCC dependency "file"` (or `<file>`) names, from the
/// pragma's tokens: GCC and Clang look it up as an `#include`'s (an error
/// when it's missing).
fn dependency(toks: &[Token]) -> Option<Include> {
    let [g, d, rest @ ..] = toks else { return None };
    if g.sym.as_str() != "GCC" || d.sym.as_str() != "dependency" {
        return None;
    }
    let name = if rest.first().is_some_and(|t| t.kind == Kind::Str) { &rest[..1] } else { rest };
    header_names(name).into_iter().next().map(|(inc, _)| inc)
}

/// The `#pragma GCC dependency`s of the `_Pragma("...")` operators in
/// tokens (a macro's body).
fn pragma_dependencies(toks: &[Token]) -> Vec<Include> {
    let operand = |w: &[Token]| {
        let lit = w[2].sym.as_str();
        let inner = lit.strip_prefix('"')?.strip_suffix('"')?;
        // Destringized: `\"` and `\\` are `"` and `\`.
        let (mut text, mut chars) = (String::new(), inner.chars());
        while let Some(c) = chars.next() {
            text.push(if c == '\\' { chars.next().unwrap_or(c) } else { c });
        }
        dependency(&cpp_deps::lex::tokens(text.as_bytes()))
    };
    toks.windows(4)
        .filter(|w| w[0].sym.as_str() == "_Pragma" && w[1].is(P::LParen) && w[2].kind == Kind::Str && w[3].is(P::RParen))
        .filter_map(operand)
        .collect()
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
                [t] if t.kind == Kind::Ident => {
                    d.computed.push(t.sym.as_str().to_string());
                    d.computed_paths.push(Some(Vec::new()));
                }
                [] => {}
                toks => {
                    d.computed.push(String::new());
                    d.computed_paths.push(path_prefixes(toks));
                }
            },
            DirKind::If | DirKind::Elif | DirKind::Ifdef | DirKind::Ifndef | DirKind::Elifdef | DirKind::Elifndef => {
                d.includes.extend(has_include_operands(&dir.toks));
            }
            // (Also `_Pragma("...")` in text, which the lexer makes one.)
            DirKind::Pragma => d.includes.extend(dependency(&dir.toks)),
            DirKind::Define => {
                let Some(m) = &dir.mac else { continue };
                let name = m.name.as_str();
                // (`#define HAS_X __has_include(<x>)`.)
                d.includes.extend(has_include_operands(&m.body));
                d.includes.extend(pragma_dependencies(&m.body));
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
                // (A function-like macro's name alone isn't expanded.)
                if m.params.is_none() {
                    d.defines.push(Define { name: name.to_string(), path: plain_path(&m.body), paths: path_prefixes(&m.body) });
                }
            }
            _ => {}
        }
    }
    for (i, def) in d.defines.iter().enumerate() {
        d.by_name.entry(def.name.clone()).or_default().push(i);
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

/// A file's version (see `FileStamp`).
type FileKey = (PathBuf, FileStamp);

/// A file's directives, for its version.
type Parsed = (FileStamp, Arc<Directives>);

/// The directories something was found in, with their stamps then (None:
/// not a directory): it holds while theirs are the same.
type Deps = Vec<(PathBuf, Option<FileStamp>)>;

#[derive(Clone, Debug, Default)]
pub struct Resolved {
    pub files: Vec<PathBuf>,
    /// The file's directives about macros (see `Directives`).
    pub directives: Arc<Directives>,
    /// A candidate's `..` after a symbolic link (see `candidates`).
    pub linked: Option<PathBuf>,
    /// The directories its includes were looked for in.
    deps: Deps,
}

/// A directory's entries, by name (as `Scanner::name_key`): whether each
/// is a directory, else a file (links followed, other kinds left out).
type Entries = HashMap<String, bool>;

/// A directory as listed: its stamp, taken before, and whether the listing
/// holds for later scans: once its stamp's time is a clock tick older than
/// when the stamp was taken (see `FileStamp::is_racy`), any change since
/// shows in its stamp; one changed within the tick is listed again.
#[derive(Clone)]
struct Listed {
    stamp: FileStamp,
    trusted: bool,
    entries: Arc<Entries>,
}

/// What a compile's scan saw of directories: each one's stamp, taken once
/// (with when), and its listing (see `Scanner::listing`).
#[derive(Default)]
struct Seen {
    stamps: RefCell<HashMap<PathBuf, Option<(FileStamp, SystemTime)>>>,
    listed: RefCell<HashMap<PathBuf, Option<Listed>>>,
}

/// The directories a resolution looked in (see `Resolved::deps`), and
/// whether their listings hold for later scans.
struct Track {
    deps: Deps,
    trusted: bool,
}

impl Default for Track {
    fn default() -> Self {
        Track { deps: Vec::new(), trusted: true }
    }
}

/// A compiler's built-in directories, whether it searches includers'
/// directories (when it said: see `Scanner::msvc_lookup`), and the macros
/// it predefines (when it said).
type Builtin = (Arc<Vec<PathBuf>>, Option<bool>, Arc<HashSet<String>>);

/// Scanner caches, shared across actions (and builds, in a daemon). What
/// depends on directories' contents is kept for their stamps (see
/// `Listed`): a header that appears (a step that doesn't declare it
/// installing it, a generated one) is found by the next scan.
#[derive(Default)]
pub struct ScanCache {
    directives: Mutex<HashMap<PathBuf, Parsed>>,
    /// By file version and search path, while the directories it looked in
    /// don't change (see `Resolved::deps`).
    resolutions: Mutex<HashMap<(FileKey, u64), Arc<Resolved>>>,
    /// By directory, for its stamp.
    listings: RwLock<HashMap<PathBuf, Listed>>,
    /// The walks of computed includes' directories (see `Scanner::walk`),
    /// by directory and depth, while the directories walked don't change.
    walks: Mutex<HashMap<(PathBuf, usize), Arc<Walk>>>,
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
    /// Names looked for in directories (see `Lookup`).
    pub lookups: usize,
    /// Directories whose stamps were taken, and those listed.
    pub dirs_checked: usize,
    pub dirs_listed: usize,
    /// Directories walked for computed includes (not those whose walk was
    /// kept).
    pub walks: usize,
}

/// How many reasons the summary of compiles left to the conservative
/// scanner names (see `ScanCache::fallback_summary`).
const SUMMARY_REASONS: usize = 4;

impl ScanCache {
    /// Counts a compile the chosen scanner left to this one, and why;
    /// true the first time for such a reason (one without what is the
    /// compile's: where the scan stopped being exact, which header wasn't
    /// found, the operand of an operator it doesn't know), which is then
    /// worth telling.
    pub fn fell_back(&self, why: &str) -> bool {
        // What it is, without the details after it ("what: details").
        let what = |w: &str| w.split(": ").next().unwrap_or_default().to_string();
        let reason = match why.strip_prefix("not exact (").and_then(|w| w.strip_suffix(')')) {
            Some(w) if w.ends_with(" not found") => "not exact (an #include not found)".to_string(),
            // ("unknown __has_builtin(__builtin_x)".)
            Some(w) if w.starts_with("unknown ") && w.contains('(') => format!("not exact ({})", w.split('(').next().unwrap_or_default()),
            // ("file:line: what", or "file: what"; a drive's colon has no
            // space after it.)
            Some(w) => format!("not exact ({})", what(w.split_once(": ").map_or(w, |(_, w)| w))),
            None if why.starts_with("more than one input") => "more than one input".to_string(),
            None => what(why),
        };
        let mut fallbacks = self.fallbacks.lock().unwrap();
        let n = fallbacks.entry(reason).or_default();
        *n += 1;
        *n == 1
    }

    /// How many compiles another scanner left to this one, and why (the
    /// most frequent reasons): a line for the end of the build.
    pub fn fallback_summary(&self) -> Option<String> {
        let fallbacks = self.fallbacks.lock().unwrap();
        let total: usize = fallbacks.values().sum();
        let mut reasons: Vec<(&String, &usize)> = fallbacks.iter().collect();
        reasons.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        let mut shown: Vec<String> = reasons.iter().take(SUMMARY_REASONS).map(|(why, n)| format!("{n}: {why}")).collect();
        match reasons.len().saturating_sub(SUMMARY_REASONS) {
            0 => {}
            1 => shown.push("1 more reason".to_string()),
            more => shown.push(format!("{more} more reasons")),
        }
        (total > 0).then(|| format!("{total} compiles were scanned conservatively ({})", shown.join("; ")))
    }
}

pub struct ScanResult {
    /// Every file the compile may read (sources, headers, forced includes).
    pub files: BTreeSet<PathBuf>,
    /// The directories of the computed includes the scan can't name.
    pub dirs: BTreeSet<PathBuf>,
    pub warnings: Vec<String>,
    /// Why the compile can't run remotely, if so: a computed include in a
    /// directory the build writes to (objects, logs, headers not generated
    /// yet), or of a macro the command line defines (but as a header
    /// name), or a `..` after a symbolic link (which the compiler resolves
    /// elsewhere than a remote worker).
    pub local: Option<String>,
    /// The computed includes the scan can't name (see
    /// `Scanner::computed_files`).
    pub computed: Vec<Computed>,
    /// What the files reached and the command line define.
    macros: Macros,
    /// The names looked for in directories, and those directories.
    lookup: Lookup,
    /// The directories the scan saw, for `computed_files`'s.
    seen: Seen,
}

/// A computed include the scan can't name.
#[derive(Clone, Debug)]
pub struct Computed {
    pub file: PathBuf,
    /// Its macro (`#include MACRO`), or empty.
    pub name: String,
    /// The directories its tokens spell out (see `path_prefixes`).
    pub paths: Option<Vec<String>>,
}

/// What a compile's files and command line define, as computed includes
/// see it.
#[derive(Clone, Default)]
struct Macros {
    /// The directives of the files reached, and the command line's.
    sources: Vec<Arc<Directives>>,
    /// Defined as (or containing) a header name, or as such a macro's name.
    header: HashSet<String>,
    /// `#define A B`: by name, the macros it's defined as.
    aliases: HashMap<String, BTreeSet<String>>,
    /// Defined on the command line, but as a header name or a macro's
    /// name: unknown to the scan.
    command_line: HashSet<String>,
}

/// The definitions of macros in the files reached, by name, each name
/// looked up once (see `Directives::by_name`).
struct Bodies<'a> {
    sources: &'a [Arc<Directives>],
    found: RefCell<HashMap<String, Rc<Vec<&'a Define>>>>,
}

impl<'a> Bodies<'a> {
    fn get(&self, name: &str) -> Rc<Vec<&'a Define>> {
        if let Some(defs) = self.found.borrow().get(name) {
            return defs.clone();
        }
        let defs: Vec<&'a Define> = self.sources.iter().flat_map(|d| d.by_name.get(name).into_iter().flatten().map(|&i| &d.defines[i])).collect();
        let defs = Rc::new(defs);
        self.found.borrow_mut().insert(name.to_string(), defs.clone());
        defs
    }

    fn contains(&self, name: &str) -> bool {
        !self.get(name).is_empty()
    }
}

/// The most paths a computed include's macros' definitions make of one,
/// and how deep in definitions they go (see `Macros::expansions`).
const SUBSTITUTED_MAX: usize = 64;
const SUBSTITUTED_DEPTH: usize = 8;

impl Macros {
    fn add(&mut self, d: &Arc<Directives>) {
        self.header.extend(d.header_macros.iter().cloned());
        for (a, b) in &d.macro_aliases {
            self.aliases.entry(a.clone()).or_default().insert(b.clone());
        }
        self.sources.push(d.clone());
    }

    fn add_command_line(&mut self, d: &Directives) {
        let header: HashSet<&String> = d.header_macros.iter().collect();
        // (A macro's name is followed; `-DM=a/b` is no header name.)
        let unknown = |def: &&Define| !header.contains(&def.name) && def.path.as_ref().is_none_or(|p| p.contains('/'));
        self.command_line.extend(d.defines.iter().filter(unknown).map(|def| def.name.clone()));
        self.add(&Arc::new(d.clone()));
    }

    /// `#define A B` with B a header macro makes A one.
    fn close(&mut self) {
        let mut defined_as: HashMap<&str, Vec<&String>> = HashMap::new();
        for (a, bs) in &self.aliases {
            for b in bs {
                defined_as.entry(b).or_default().push(a);
            }
        }
        let (mut more, mut todo): (HashSet<&String>, Vec<&str>) = (HashSet::new(), self.header.iter().map(String::as_str).collect());
        while let Some(h) = todo.pop() {
            for &a in defined_as.get(h).into_iter().flatten() {
                if !self.header.contains(a) && more.insert(a) {
                    todo.push(a);
                }
            }
        }
        let more: Vec<String> = more.into_iter().cloned().collect();
        self.header.extend(more);
    }

    /// `name`, the macros it's defined as, theirs, and so on.
    fn closure(&self, name: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::from([name.to_string()]);
        let mut todo = vec![name.to_string()];
        while let Some(n) = todo.pop() {
            for a in self.aliases.get(&n).into_iter().flatten() {
                if out.insert(a.clone()) {
                    todo.push(a.clone());
                }
            }
        }
        out
    }

    fn bodies(&self) -> Bodies<'_> {
        Bodies { sources: &self.sources, found: RefCell::default() }
    }

    /// Where `c`'s header may be: under the directories its paths name
    /// (its tokens', or its macro's definitions', with macros' plain
    /// definitions put in: see `expansions`), the names in those, and
    /// whether it may be a plain name (a definition spelling no directory).
    /// Err when that can't be told: a path that can't be read, a macro in
    /// it defined otherwise than as names.
    fn spec(&self, c: &Computed, bodies: &Bodies) -> Result<(Vec<String>, BTreeSet<String>, bool), String> {
        let (mut paths, mut plain) = (Vec::new(), false);
        if c.name.is_empty() {
            match &c.paths {
                Some(p) => {
                    paths.clone_from(p);
                    plain = p.is_empty();
                }
                None => return Err(format!("computed #include whose path can't be read: {}", c.file.display())),
            }
        } else {
            let mut defined = false;
            for n in self.closure(&c.name) {
                // (Another macro's name: in the closure.)
                for def in bodies.get(&n).iter().filter(|def| def.path.as_ref().is_none_or(|p| p.contains('/'))) {
                    defined = true;
                    match &def.paths {
                        Some(p) if p.is_empty() => plain = true,
                        Some(p) => paths.extend(p.iter().cloned()),
                        None => return Err(format!("computed #include of a macro whose path can't be read: {n} in {}", c.file.display())),
                    }
                }
            }
            plain |= !defined;
        }
        let paths = self.substituted(&paths, bodies).map_err(|n| format!("computed #include with a macro in its path: {n} in {}", c.file.display()))?;
        let names = paths.iter().flat_map(|p| p.split('/')).filter(|n| !n.is_empty()).map(str::to_string).collect();
        Ok((paths, names, plain))
    }

    /// `paths` with their macros' definitions put in (see `expansions`).
    /// Err with a macro defined otherwise than as names, or making too
    /// many.
    fn substituted(&self, paths: &[String], bodies: &Bodies) -> Result<Vec<String>, String> {
        let mut out = BTreeSet::new();
        for p in paths {
            out.extend(Self::expansions(p, bodies, SUBSTITUTED_DEPTH)?.into_iter().map(|e| format!("{e}/")));
        }
        Ok(out.into_iter().collect())
    }

    /// What `path` (names joined by slashes) may stand for: each name a
    /// macro is, with the names its definitions are (theirs in turn), and
    /// itself, which `#x` keeps: `lib/PLATFORM` is also `lib/posix` after
    /// `#define PLATFORM posix`.
    fn expansions(path: &str, bodies: &Bodies, depth: usize) -> Result<Vec<String>, String> {
        let mut alternatives = vec![String::new()];
        for name in path.split('/').filter(|n| !n.is_empty()) {
            let mut names = vec![name.to_string()];
            for def in bodies.get(name).iter() {
                match &def.path {
                    Some(p) if depth > 0 => names.extend(Self::expansions(p, bodies, depth - 1)?),
                    _ => return Err(name.to_string()),
                }
            }
            alternatives = alternatives.iter().flat_map(|a| names.iter().map(move |n| if a.is_empty() { n.clone() } else { format!("{a}/{n}") })).collect();
            alternatives.sort();
            alternatives.dedup();
            if alternatives.len() > SUBSTITUTED_MAX {
                return Err(name.to_string());
            }
        }
        Ok(alternatives)
    }
}

/// Why computed includes' files can't be sent.
enum Unsent {
    /// Too many (see `store::Walk::over`).
    Over(String),
    /// The compile runs locally.
    Local(String),
}

impl Unsent {
    fn reason(self) -> String {
        match self {
            Unsent::Over(why) | Unsent::Local(why) => why,
        }
    }
}

/// Where computed includes' paths are looked for, besides their files'
/// directories: MSVC's lookup's directories (those of the files reached)
/// and the search path's; each path in each once.
#[derive(Default)]
struct Bases {
    msvc: bool,
    dirs: Vec<PathBuf>,
    known: HashSet<PathBuf>,
    /// By path, the directories it names under them.
    found: HashMap<String, Vec<PathBuf>>,
}

impl Bases {
    fn new(sp: &SearchPath, files: &BTreeSet<PathBuf>) -> Self {
        let mut b = Bases { msvc: sp.msvc, ..Default::default() };
        let reached = files.iter().filter_map(|f| f.parent()).filter(|_| sp.msvc);
        for d in reached.chain(sp.quote.iter().chain(&sp.angled).map(PathBuf::as_path)) {
            if b.known.insert(d.to_path_buf()) {
                b.dirs.push(d.to_path_buf());
            }
        }
        b
    }

    /// The directories `path` names under them.
    fn under(&mut self, scanner: &Scanner, seen: &Seen, path: &str) -> &[PathBuf] {
        let dirs = &self.dirs;
        self.found.entry(path.to_string()).or_insert_with(|| dirs.iter().filter_map(|b| scanner.dir_in(seen, b, path)).collect())
    }

    /// With MSVC's lookup, the directories of more files reached: the
    /// directories the paths looked for name under them.
    fn reached(&mut self, scanner: &Scanner, seen: &Seen, files: &BTreeSet<PathBuf>) -> Vec<(PathBuf, usize)> {
        let mut out = Vec::new();
        for d in files.iter().filter_map(|f| f.parent()).filter(|_| self.msvc) {
            if !self.known.insert(d.to_path_buf()) {
                continue;
            }
            for (p, found) in self.found.iter_mut() {
                if let Some(under) = scanner.dir_in(seen, d, p) {
                    out.push((under.clone(), crate::store::DIR_DEPTH_MAX));
                    found.push(under);
                }
            }
            self.dirs.push(d.to_path_buf());
        }
        out
    }
}

/// The directories build edges write to (their outputs', the build
/// directory's own), where files may appear or change during the build: a
/// computed include in one runs locally, and the files a computed
/// include's walk sends from them are header-like ones (see
/// `store::walk`). Not the rest of the build directory: sources fetched at
/// configure time (CMake's FetchContent and CPM put them in `_deps`) are
/// sources.
#[derive(Debug, Default)]
pub struct WrittenDirs {
    /// As keys (see `key`).
    dirs: HashSet<PathBuf>,
    case_insensitive: bool,
}

impl WrittenDirs {
    pub fn new(dirs: impl IntoIterator<Item = PathBuf>, case_insensitive: bool) -> Self {
        let mut w = WrittenDirs { case_insensitive, ..Default::default() };
        for d in dirs {
            let key = w.key(&d);
            w.dirs.insert(key);
        }
        w
    }

    fn key(&self, p: &Path) -> PathBuf {
        if self.case_insensitive { PathBuf::from(p.to_string_lossy().to_lowercase()) } else { p.to_path_buf() }
    }

    /// Whether the build writes to `dir`.
    pub fn contains(&self, dir: &Path) -> bool {
        !self.dirs.is_empty() && self.dirs.contains(&self.key(dir))
    }
}

pub struct Scanner<'a> {
    pub cache: &'a ScanCache,
    pub windows: bool,
    /// Where the build writes (see `WrittenDirs`).
    pub written: &'a WrittenDirs,
}

/// Names looked for in the directories of the files reached, each pair
/// once: with MSVC's lookup, the quoted names of the files reached, in the
/// directory of each (a superset of each's includers); and the quoted
/// header names macros are defined as (the command line's too), in the
/// directory of each file with a computed include, where `#include MACRO`
/// looks first.
#[derive(Clone, Default)]
struct Lookup {
    /// Tried with each other.
    dirs: Vec<PathBuf>,
    names: Vec<Name>,
    /// Not tried yet.
    new_dirs: Vec<PathBuf>,
    new_names: Vec<Name>,
    seen_dirs: HashSet<PathBuf>,
    seen_names: HashSet<String>,
    /// Names are compared as Windows compares them (see
    /// `Scanner::name_key`).
    windows: bool,
}

/// A name looked for, with its key in listings when it's a plain name.
type Name = (String, Option<String>);

impl Lookup {
    fn dir(&mut self, dir: &Path) {
        if self.seen_dirs.insert(dir.to_path_buf()) {
            self.new_dirs.push(dir.to_path_buf());
        }
    }

    fn names<'a>(&mut self, incs: impl IntoIterator<Item = &'a Include>) {
        for inc in incs.into_iter().filter(|i| !i.angled && Path::new(&i.name).is_relative()) {
            if self.seen_names.insert(inc.name.clone()) {
                let n = &inc.name;
                let plain = !n.is_empty() && !n.contains(['/', '\\']) && n != "." && n != "..";
                let key = plain.then(|| if self.windows { n.to_ascii_lowercase() } else { n.clone() });
                self.new_names.push((n.clone(), key));
            }
        }
    }
}

fn file_key(p: &Path) -> Option<FileKey> {
    let (m, stamp) = ninja_os::stat(p).ok()?;
    m.is_file().then(|| (p.to_path_buf(), stamp))
}

impl Scanner<'_> {
    fn name_key(&self, s: &str) -> String {
        if self.windows { s.to_ascii_lowercase() } else { s.to_string() }
    }

    /// A directory's stamp, taken once per compile (see `Seen`), and when.
    fn stamp(&self, seen: &Seen, dir: &Path) -> Option<(FileStamp, SystemTime)> {
        if let Some(s) = seen.stamps.borrow().get(dir) {
            return *s;
        }
        let at = SystemTime::now();
        let s = ninja_os::stat(dir).ok().filter(|(m, _)| m.is_dir()).map(|(_, s)| (s, at));
        self.cache.stats.lock().unwrap().dirs_checked += 1;
        seen.stamps.borrow_mut().insert(dir.to_path_buf(), s);
        s
    }

    /// Whether the directories `deps` names have the same stamps.
    fn unchanged(&self, seen: &Seen, deps: impl IntoIterator<Item = (impl AsRef<Path>, Option<FileStamp>)>) -> bool {
        deps.into_iter().all(|(d, s)| self.stamp(seen, d.as_ref()).map(|s| s.0) == s)
    }

    /// A directory's listing: the one kept for its stamp when it holds
    /// (see `Listed`), else read (once per compile).
    fn listing(&self, seen: &Seen, dir: &Path) -> Option<Listed> {
        if let Some(l) = seen.listed.borrow().get(dir) {
            return l.clone();
        }
        let l = self.stamp(seen, dir).and_then(|(stamp, at)| {
            if let Some(l) = self.cache.listings.read().unwrap().get(dir).filter(|l| l.trusted && l.stamp == stamp) {
                return Some(l.clone());
            }
            let entries = self.entries(dir)?;
            self.cache.stats.lock().unwrap().dirs_listed += 1;
            let l = Listed { stamp, trusted: !stamp.is_racy(at), entries: Arc::new(entries) };
            self.cache.listings.write().unwrap().insert(dir.to_path_buf(), l.clone());
            Some(l)
        });
        seen.listed.borrow_mut().insert(dir.to_path_buf(), l.clone());
        l
    }

    fn entries(&self, dir: &Path) -> Option<Entries> {
        let rd = std::fs::read_dir(dir).ok()?;
        let kind = |e: &std::fs::DirEntry| match e.file_type() {
            Ok(t) if t.is_symlink() => std::fs::metadata(e.path()).ok().filter(|m| m.is_dir() || m.is_file()).map(|m| m.is_dir()),
            Ok(t) if t.is_dir() || t.is_file() => Some(t.is_dir()),
            _ => None,
        };
        Some(rd.flatten().filter_map(|e| Some((self.name_key(&e.file_name().to_string_lossy()), kind(&e)?))).collect())
    }

    /// What `rel` (plain names) is under `base`, from listings: Some(true)
    /// a directory, Some(false) a file. The directories looked in are
    /// noted in `track`.
    fn kind_in(&self, seen: &Seen, base: &Path, rel: &Path, mut track: Option<&mut Track>) -> Option<bool> {
        let mut dir = base.to_path_buf();
        let mut names = rel.components().peekable();
        loop {
            let Some(Component::Normal(name)) = names.next() else { return None };
            let l = self.listing(seen, &dir);
            if let Some(t) = track.as_deref_mut() {
                t.deps.push((dir.clone(), self.stamp(seen, &dir).map(|s| s.0)));
                t.trusted &= l.as_ref().is_none_or(|l| l.trusted);
            }
            let kind = *l?.entries.get(&self.name_key(&name.to_string_lossy()))?;
            if names.peek().is_none() {
                return Some(kind);
            }
            if !kind {
                return None;
            }
            dir.push(name);
        }
    }

    /// `name` in `dir`, if it's a file there (see `kind_in`).
    fn file_in(&self, seen: &Seen, dir: &Path, name: &Path, track: Option<&mut Track>) -> Option<PathBuf> {
        let p = normalize(&dir.join(name));
        let plain = !name.as_os_str().is_empty() && name.components().all(|c| matches!(c, Component::Normal(_)));
        let kind = if plain { self.kind_in(seen, dir, name, track) } else { self.kind_in(seen, p.parent()?, Path::new(p.file_name()?), track) };
        (kind == Some(false)).then_some(p)
    }

    fn directives(&self, p: &Path, key: &FileKey) -> Arc<Directives> {
        if let Some((stamp, d)) = self.cache.directives.lock().unwrap().get(p)
            && *stamp == key.1
        {
            return d.clone();
        }
        let d = Arc::new(std::fs::read(p).map(|c| parse_directives(&c)).unwrap_or_default());
        self.cache.stats.lock().unwrap().files_parsed += 1;
        self.cache.directives.lock().unwrap().insert(p.to_path_buf(), (key.1, d.clone()));
        d
    }

    /// The files `inc` may name, from a file in `from_dir` (None: the
    /// command line's), noting the directories looked in. A `..` after a
    /// symbolic link, which the compiler resolves elsewhere than a remote
    /// worker (whose inputs are plain directories), is noted in `linked`
    /// when a file is there either way.
    #[allow(clippy::too_many_arguments)]
    fn candidates(
        &self,
        seen: &Seen,
        inc: &Include,
        from_dir: Option<&Path>,
        sp: &SearchPath,
        out: &mut Vec<PathBuf>,
        linked: &mut Option<PathBuf>,
        track: &mut Track,
    ) {
        let name = &inc.name;
        let np = Path::new(name);
        let mut try_in = |dir: &Path| {
            let joined = dir.join(np);
            if linked.is_none()
                && let Some(link) = cpp_deps::scan::dotdot_after_link(&joined)
                && (joined.is_file() || normalize(&joined).is_file())
            {
                *linked = Some(link);
            }
            out.extend(self.file_in(seen, dir, np, Some(track)));
        };
        if np.is_absolute() || (self.windows && name.len() > 2 && name.as_bytes()[1] == b':') {
            try_in(Path::new(""));
            return;
        }
        let quote = if inc.angled { None } else { Some(from_dir.into_iter().chain(sp.quote.iter().map(PathBuf::as_path))) };
        for d in quote.into_iter().flatten().chain(sp.angled.iter().map(PathBuf::as_path)) {
            try_in(d);
        }
    }

    /// The files `m`'s names are in its directories that weren't looked for
    /// yet: the new names in every directory, the others in the new
    /// directories.
    fn lookups(&self, seen: &Seen, m: &mut Lookup) -> Vec<PathBuf> {
        let (new_dirs, new_names) = (std::mem::take(&mut m.new_dirs), std::mem::take(&mut m.new_names));
        let mut found = Vec::new();
        let mut look = |dir: &Path, names: &[Name]| {
            self.cache.stats.lock().unwrap().lookups += names.len();
            let listed = self.listing(seen, dir);
            for (n, key) in names {
                match key {
                    Some(k) => found.extend(listed.as_ref().filter(|l| l.entries.get(k) == Some(&false)).map(|_| normalize(&dir.join(n)))),
                    None => found.extend(self.file_in(seen, dir, Path::new(n), None)),
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

    /// Resolves the includes of one file (cached per file version and
    /// search path, while the directories it looked in don't change).
    fn resolve_file(&self, seen: &Seen, p: &Path, sp: &SearchPath, sp_id: u64) -> Arc<Resolved> {
        let Some(key) = file_key(p) else { return Arc::new(Resolved::default()) };
        let ck = (key.clone(), sp_id);
        let kept = self.cache.resolutions.lock().unwrap().get(&ck).cloned();
        if let Some(r) = kept
            && self.unchanged(seen, r.deps.iter().map(|(d, s)| (d, *s)))
        {
            self.cache.stats.lock().unwrap().resolution_hits += 1;
            return r;
        }
        self.cache.stats.lock().unwrap().resolution_misses += 1;
        let d = self.directives(p, &key);
        let from_dir = p.parent().unwrap_or(Path::new(""));
        let (mut files, mut linked, mut track) = (Vec::new(), None, Track::default());
        for inc in d.includes.iter().chain(&d.define_candidates) {
            self.candidates(seen, inc, Some(from_dir), sp, &mut files, &mut linked, &mut track);
        }
        files.sort();
        files.dedup();
        track.deps.sort_by(|a, b| a.0.cmp(&b.0));
        track.deps.dedup_by(|a, b| a.0 == b.0);
        let r = Arc::new(Resolved { files, directives: d, linked, deps: track.deps });
        let mut kept = self.cache.resolutions.lock().unwrap();
        if track.trusted {
            kept.insert(ck, r.clone());
        } else {
            kept.remove(&ck);
        }
        r
    }

    /// Finds every file reachable from the sources and forced includes.
    pub fn scan(&self, roots: &[PathBuf], sp: &SearchPath) -> ScanResult {
        self.scan_compile(roots, sp, &Directives::default())
    }

    /// `scan`, with the command line's definitions (`CompileInfo::macros`):
    /// a header name one is defined as is a candidate, and a computed
    /// include of one defined otherwise (but as a macro's name) is unknown:
    /// the compile runs locally.
    pub fn scan_compile(&self, roots: &[PathBuf], sp: &SearchPath, command_line: &Directives) -> ScanResult {
        let mut macros = Macros::default();
        macros.add_command_line(command_line);
        let seen = Seen::default();
        let lookup = Lookup { windows: self.windows, ..Default::default() };
        let mut r = self.scan_in(roots, sp, (macros, lookup), &BTreeSet::new(), Some(command_line), &seen);
        r.seen = seen;
        r
    }

    /// `scan`, going on from what a scan of the files `scanned` knew
    /// (`known`), which it doesn't go through again.
    fn scan_in(
        &self,
        roots: &[PathBuf],
        sp: &SearchPath,
        known: (Macros, Lookup),
        scanned: &BTreeSet<PathBuf>,
        command_line: Option<&Directives>,
        seen: &Seen,
    ) -> ScanResult {
        let sp_id = sp.id();
        let mut files = BTreeSet::new();
        let mut linked = None;
        let mut queue: VecDeque<PathBuf> = roots.iter().cloned().collect();
        // The files with computed includes.
        let mut computed: Vec<(PathBuf, Arc<Directives>)> = Vec::new();
        let (mut macros, mut lookup) = known;
        // The header names of the command line's definitions, as `#include`s
        // from no file.
        if let Some(cl) = command_line {
            let mut found = Vec::new();
            for inc in &cl.define_candidates {
                self.candidates(seen, inc, None, sp, &mut found, &mut linked, &mut Track::default());
            }
            queue.extend(found);
            lookup.names(&cl.define_candidates);
        }
        loop {
            while let Some(f) = queue.pop_front() {
                if scanned.contains(&f) || !files.insert(f.clone()) {
                    continue;
                }
                let r = self.resolve_file(seen, &f, sp, sp_id);
                if linked.is_none() {
                    linked.clone_from(&r.linked);
                }
                let (d, from_dir) = (&r.directives, f.parent().unwrap_or(Path::new("")));
                if !d.computed.is_empty() {
                    computed.push((f.clone(), d.clone()));
                    lookup.dir(from_dir);
                }
                if sp.msvc {
                    lookup.dir(from_dir);
                    lookup.names(&d.includes);
                }
                lookup.names(&d.define_candidates);
                macros.add(d);
                queue.extend(r.files.iter().filter(|inc| !files.contains(*inc)).cloned());
            }
            queue.extend(self.lookups(seen, &mut lookup).into_iter().filter(|f| !files.contains(f)));
            if queue.is_empty() {
                break;
            }
        }
        macros.close();
        // A computed include whose macro names a header is covered: the
        // header was followed as a candidate. Otherwise its directory's
        // files go, and those the directories its path names (see
        // `computed_files`); in a directory the build writes to (objects,
        // logs, headers not generated yet), the compile runs locally.
        let mut local = linked.map(|l| format!("`..` after a symbolic link, which the compiler resolves elsewhere than a remote worker: {}", l.display()));
        let (mut dirs, mut warnings, mut unnamed) = (BTreeSet::new(), Vec::new(), Vec::new());
        for (f, d) in &computed {
            let Some(parent) = f.parent() else { continue };
            for (m, paths) in d.computed.iter().zip(&d.computed_paths) {
                if !m.is_empty() {
                    if let Some(c) = macros.closure(m).into_iter().find(|n| macros.command_line.contains(n)) {
                        local.get_or_insert_with(|| format!("computed #include of a macro the command line defines: {c}"));
                        continue;
                    }
                    if macros.header.contains(m) {
                        continue;
                    }
                }
                if self.written.contains(parent) {
                    local.get_or_insert_with(|| format!("computed #include in a directory the build writes to: {}", f.display()));
                    continue;
                }
                if dirs.insert(parent.to_path_buf()) {
                    warnings.push(format!("computed #include in {}: sending the files under {}", f.display(), parent.display()));
                }
                unnamed.push(Computed { file: f.clone(), name: m.clone(), paths: paths.clone() });
            }
        }
        ScanResult { files, dirs, warnings, local, computed: unnamed, macros, lookup, seen: Seen::default() }
    }

    /// The files to send for the computed includes the scan can't name
    /// (`r.computed`): those under their files' directories and under the
    /// directories their paths name (see `places`), and what those include
    /// (scanned, with their own computed includes, to a fixpoint). Too
    /// many (see `store::DIR_FILES_MAX`): narrowed to the directories their
    /// paths name (and their files', for those that may be plain names).
    /// Err (the compile runs locally) with the reason: where
    /// narrowing would look can't be told (a macro in a path: `is_macro`
    /// says the command line's and the compiler's), a computed include in a
    /// directory the build writes to (`written`), still too many.
    pub fn computed_files(
        &self,
        r: &ScanResult,
        sp: &SearchPath,
        written: &dyn Fn(&Path) -> bool,
        is_macro: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<PathBuf>, String> {
        if r.computed.is_empty() {
            return Ok(Vec::new());
        }
        match self.send(r, sp, written, is_macro, false) {
            Err(Unsent::Over(_)) => self.send(r, sp, written, is_macro, true).map_err(Unsent::reason),
            sent => sent.map_err(Unsent::reason),
        }
    }

    fn send(
        &self,
        r: &ScanResult,
        sp: &SearchPath,
        written: &dyn Fn(&Path) -> bool,
        is_macro: &dyn Fn(&str) -> bool,
        narrowed: bool,
    ) -> Result<Vec<PathBuf>, Unsent> {
        let seen = &r.seen;
        let (mut known, mut computed) = ((r.macros.clone(), r.lookup.clone()), r.computed.clone());
        let mut bases = Bases::new(sp, &r.files);
        let (mut scanned, mut reached) = (r.files.clone(), BTreeSet::new());
        // The files walked, with their sizes, and how deep under each
        // directory the walks went.
        let (mut sent, mut walked): (BTreeMap<PathBuf, u64>, HashMap<PathBuf, usize>) = Default::default();
        let (mut placed, mut places) = (0, Vec::new());
        loop {
            {
                let bodies = known.0.bodies();
                for c in &computed[placed..] {
                    places.extend(self.places(seen, c, &known.0, &bodies, &mut bases, is_macro, narrowed)?);
                }
            }
            placed = computed.len();
            for (d, depth) in std::mem::take(&mut places) {
                if walked.get(&d).is_some_and(|&k| k >= depth) {
                    continue;
                }
                let w = self.walk(seen, &d, depth, written);
                for (dir, k, _) in &w.dirs {
                    let e = walked.entry(dir.clone()).or_default();
                    *e = (*e).max(*k);
                }
                if let Some(why) = &w.over {
                    return Err(Unsent::Over(format!("{why}{}", if narrowed { " (narrowed)" } else { "" })));
                }
                sent.extend(w.files.iter().cloned());
                let bytes: u64 = sent.values().sum();
                if sent.len() > crate::store::DIR_FILES_MAX || bytes > crate::store::DIR_BYTES_MAX {
                    let mb = crate::store::DIR_BYTES_MAX >> 20;
                    let narrowed = if narrowed { " (narrowed)" } else { "" };
                    return Err(Unsent::Over(format!(
                        "computed #include with over {} files or {mb} MB to send{narrowed}: {}...",
                        crate::store::DIR_FILES_MAX,
                        d.display()
                    )));
                }
            }
            // What they include.
            let new: Vec<PathBuf> = sent.keys().filter(|f| !scanned.contains(*f)).cloned().collect();
            if new.is_empty() {
                break;
            }
            let more = self.scan_in(&new, sp, known, &scanned, None, seen);
            if let Some(why) = more.local {
                return Err(Unsent::Local(why));
            }
            scanned.extend(more.files.iter().cloned());
            // (MSVC's lookup's new directories, where paths may be too.)
            places.extend(bases.reached(self, seen, &more.files));
            reached.extend(more.files);
            computed.extend(more.computed);
            known = (more.macros, more.lookup);
        }
        let mut files: BTreeSet<PathBuf> = sent.into_keys().collect();
        files.extend(reached);
        Ok(files.into_iter().collect())
    }

    /// Where a computed include's header may be, and how deep under each
    /// directory to go: under the directories its paths name (see
    /// `Macros::spec`), wherever an `#include "..."` from its file may find
    /// them (its directory, `bases`), all under them; and its file's
    /// directory, all under it: `narrowed`, only when it may be a plain
    /// name (`BOOST_PP_ITERATE()`'s, whose headers are named elsewhere), as
    /// the macros it calls may still make a path there (Boost.FunctionTypes'
    /// `<base_path/arity ## max_arity ## _ ## mfp.hpp>`). A path found
    /// nowhere names no file a compiler finds either. Narrowed, Err when
    /// where it is can't be told (see `Macros::spec`), or a name in its
    /// paths is a macro of the command line's or the compiler's
    /// (`is_macro`).
    #[allow(clippy::too_many_arguments)]
    fn places(
        &self,
        seen: &Seen,
        c: &Computed,
        macros: &Macros,
        bodies: &Bodies,
        bases: &mut Bases,
        is_macro: &dyn Fn(&str) -> bool,
        narrowed: bool,
    ) -> Result<Vec<(PathBuf, usize)>, Unsent> {
        let dir = c.file.parent().unwrap_or(Path::new(""));
        let mut out = vec![(dir.to_path_buf(), crate::store::DIR_DEPTH_MAX)];
        let too_many = |why: String| Unsent::Local(format!("{why} (too many files to send under {})", dir.display()));
        let (paths, names, plain) = match macros.spec(c, bodies) {
            Ok(spec) => spec,
            Err(why) if narrowed => return Err(too_many(why)),
            Err(_) => return Ok(out),
        };
        if narrowed {
            // (Those the scanned files define are put in.)
            if let Some(n) = names.iter().find(|n| !bodies.contains(n) && is_macro(n)) {
                return Err(too_many(format!("computed #include with a predefined or command line macro in its path: {n} in {}", c.file.display())));
            }
            if !plain {
                out.clear();
            }
        }
        for p in &paths {
            out.extend(self.dir_in(seen, dir, p).map(|d| (d, crate::store::DIR_DEPTH_MAX)));
            out.extend(bases.under(self, seen, p).iter().map(|d| (d.clone(), crate::store::DIR_DEPTH_MAX)));
        }
        Ok(out)
    }

    /// `base`/`path`, if it's a directory (see `kind_in`).
    fn dir_in(&self, seen: &Seen, base: &Path, path: &str) -> Option<PathBuf> {
        (self.kind_in(seen, base, Path::new(path), None) == Some(true)).then(|| normalize(&base.join(path)))
    }

    /// The directories narrowing sends for `r`'s computed includes, and
    /// how deep under each (see `places`).
    pub fn narrowed_dirs(&self, r: &ScanResult, sp: &SearchPath, is_macro: &dyn Fn(&str) -> bool) -> Result<BTreeMap<PathBuf, usize>, String> {
        let (bodies, mut bases) = (r.macros.bodies(), Bases::new(sp, &r.files));
        let mut out = BTreeMap::new();
        for c in &r.computed {
            for (d, depth) in self.places(&r.seen, c, &r.macros, &bodies, &mut bases, is_macro, true).map_err(Unsent::reason)? {
                let e = out.entry(d).or_default();
                *e = depth.max(*e);
            }
        }
        Ok(out)
    }

    /// The files under `dir` for computed includes (see `store::walk`),
    /// kept while the directories walked don't change.
    fn walk(&self, seen: &Seen, dir: &Path, depth: usize, written: &dyn Fn(&Path) -> bool) -> Arc<Walk> {
        let key = (dir.to_path_buf(), depth);
        let kept = self.cache.walks.lock().unwrap().get(&key).cloned();
        if let Some(w) = kept
            && self.unchanged(seen, w.dirs.iter().map(|(d, _, s)| (d, *s)).chain(w.builds.iter().map(|(d, s)| (d, *s))))
        {
            return w;
        }
        self.cache.stats.lock().unwrap().walks += 1;
        let w = Arc::new(crate::store::walk(dir, depth, written));
        let mut kept = self.cache.walks.lock().unwrap();
        if w.trusted {
            kept.insert(key, w.clone());
        } else {
            kept.remove(&key);
        }
        w
    }

    /// Whether the compiler searches includers' directories (MSVC's lookup):
    /// when the flags say so (cl's, clang-cl's; Clang's MSVC compatibility,
    /// which its search follows whatever the target), or its predefined
    /// macros (`_MSC_VER` and `__clang__`: Clang for the MSVC ABI, which its
    /// driver's name or configuration files may choose) when `builtin_dirs`
    /// could ask it. Either way: searching more directories only sends more
    /// files.
    pub fn msvc_lookup(&self, compiler: &Path, info: &CompileInfo) -> bool {
        let answer = self.cache.builtin.lock().unwrap().get(&builtin_key(compiler, info)).and_then(|(_, msvc, _)| *msvc);
        info.style == Some(Style::Msvc) || info.msvc_lookup || answer == Some(true)
    }

    /// The compiler's built-in include directories, from `-v` output
    /// (cached per compiler, relevant flags and language).
    pub fn builtin_dirs(&self, compiler: &Path, info: &CompileInfo) -> Result<Arc<Vec<PathBuf>>, String> {
        let key = builtin_key(compiler, info);
        if let Some((d, _, _)) = self.cache.builtin.lock().unwrap().get(&key) {
            return Ok(d.clone());
        }
        // (Asked with `-nostdinc` too, for `msvc_lookup`.)
        let (dirs, msvc, predefined) = match discover_builtin_dirs(compiler, info, info.lang.unwrap_or(Lang::Cxx)) {
            Err(_) if info.no_builtin_includes => (Vec::new(), None, HashSet::new()),
            found => found?,
        };
        let dirs = Arc::new(if info.no_builtin_includes { Vec::new() } else { dirs });
        self.cache.builtin.lock().unwrap().insert(key, (dirs.clone(), msvc, Arc::new(predefined)));
        Ok(dirs)
    }

    /// Whether `name` is a macro the command line or the compiler (as
    /// `builtin_dirs` asked it) defines.
    pub fn predefined(&self, compiler: &Path, info: &CompileInfo, name: &str) -> bool {
        info.defines.iter().any(|d| d == name) || self.cache.builtin.lock().unwrap().get(&builtin_key(compiler, info)).is_some_and(|(_, _, p)| p.contains(name))
    }
}

/// The key of a compiler's built-in directories: it, the flags that change
/// them, and the language.
fn builtin_key(compiler: &Path, info: &CompileInfo) -> String {
    format!("{}|{:?}|{:?}|{}", compiler.display(), info.style, info.lang.unwrap_or(Lang::Cxx), info.builtin_flags.join("\u{1}"))
}

/// The include search list a compiler prints, whether it searches
/// includers' directories (MSVC's lookup), and the macros it predefines.
type Discovered = (Vec<PathBuf>, Option<bool>, HashSet<String>);

/// Runs `<compiler> -E -v` on an empty input and parses the include search
/// list it prints, with `-dM` (clang-cl's `/clang:-dM`): the macros it
/// predefines, and whether a GCC-style driver is Clang for the MSVC ABI
/// (`_MSC_VER` and `__clang__`), which searches includers' directories.
/// cl.exe can't say: `INCLUDE`'s, and the macros it may predefine (see
/// `CL_MACROS`). Killed when it takes too long.
pub fn discover_builtin_dirs(compiler: &Path, info: &CompileInfo, lang: Lang) -> Result<Discovered, String> {
    discover_builtin_dirs_for(compiler, info, lang, cpp_deps::probe::TIMEOUT)
}

fn discover_builtin_dirs_for(compiler: &Path, info: &CompileInfo, lang: Lang, timeout: std::time::Duration) -> Result<Discovered, String> {
    let base = compiler.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if base == "cl" {
        // MSVC's cl.exe has no -v; it uses the INCLUDE environment variable.
        let dirs = std::env::var_os("INCLUDE").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default();
        return Ok((dirs, None, CL_MACROS.iter().map(|m| m.to_string()).collect()));
    }
    let mut cmd = std::process::Command::new(compiler);
    cmd.args(&info.builtin_flags);
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let gcc = info.style != Some(Style::Msvc);
    if !gcc {
        cmd.arg("-v").arg("-E").arg("/clang:-dM").arg(if lang == Lang::C { "/TC" } else { "/TP" }).arg(null);
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
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let out = ninja_os::process::output(cmd, None, timeout).map_err(|e| format!("running {} -v: {e}", compiler.display()))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let dirs = parse_search_list(&text);
    if dirs.is_empty() && !info.no_builtin_includes {
        return Err(format!("could not find the include search list in `{} -v` output (exit {:?})", compiler.display(), out.status.code()));
    }
    let macros = String::from_utf8_lossy(&out.stdout);
    let predefined: HashSet<String> =
        macros.lines().filter_map(|l| l.strip_prefix("#define ")).filter_map(|d| d.split([' ', '(']).next()).map(str::to_string).collect();
    let msvc = gcc.then(|| predefined.contains("_MSC_VER") && predefined.contains("__clang__"));
    Ok((dirs, msvc, predefined))
}

/// The macros cl.exe may predefine (Microsoft's "Predefined macros"),
/// whatever its flags and target: names a computed include's path can't
/// be taken for (see `Scanner::computed_files`).
const CL_MACROS: &[&str] = &[
    "__ATOM__",
    "__AVX__",
    "__AVX2__",
    "__AVX512BW__",
    "__AVX512CD__",
    "__AVX512DQ__",
    "__AVX512F__",
    "__AVX512VL__",
    "__AVX10_VER__",
    "_CHAR_UNSIGNED",
    "__CLR_VER",
    "_CONTROL_FLOW_GUARD",
    "__COUNTER__",
    "__cplusplus",
    "__cplusplus_cli",
    "__cplusplus_winrt",
    "_CPPRTTI",
    "_CPPUNWIND",
    "__DATE__",
    "_DEBUG",
    "_DLL",
    "__FILE__",
    "__func__",
    "__FUNCDNAME__",
    "__FUNCSIG__",
    "__FUNCTION__",
    "_INTEGRAL_MAX_BITS",
    "_ISO_VOLATILE",
    "_KERNEL_MODE",
    "__LINE__",
    "_M_AMD64",
    "_M_ARM",
    "_M_ARM_ARMV7VE",
    "_M_ARM_FP",
    "_M_ARM64",
    "_M_ARM64EC",
    "_M_CEE",
    "_M_CEE_PURE",
    "_M_CEE_SAFE",
    "_M_FP_CONTRACT",
    "_M_FP_EXCEPT",
    "_M_FP_FAST",
    "_M_FP_PRECISE",
    "_M_FP_STRICT",
    "_M_HYBRID",
    "_M_HYBRID_X86_ARM64",
    "_M_IX86",
    "_M_IX86_FP",
    "_M_THUMB",
    "_M_X64",
    "_MANAGED",
    "_MSC_BUILD",
    "_MSC_EXTENSIONS",
    "_MSC_FULL_VER",
    "_MSC_VER",
    "_MSVC_LANG",
    "_MSVC_TRADITIONAL",
    "__MSVC_RUNTIME_CHECKS",
    "_MT",
    "_NATIVE_NULLPTR_SUPPORTED",
    "_NATIVE_WCHAR_T_DEFINED",
    "_OPENMP",
    "_PREFAST_",
    "__SANITIZE_ADDRESS__",
    "__STDC__",
    "__STDC_HOSTED__",
    "__STDC_NO_ATOMICS__",
    "__STDC_NO_COMPLEX__",
    "__STDC_NO_THREADS__",
    "__STDC_NO_VLA__",
    "__STDC_VERSION__",
    "__STDCPP_DEFAULT_NEW_ALIGNMENT__",
    "__STDCPP_THREADS__",
    "__TIME__",
    "__TIMESTAMP__",
    "_VC_NODEFAULTLIB",
    "_WCHAR_T_DEFINED",
    "_WIN32",
    "_WIN64",
    "_WINRT_DLL",
];

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
pub(crate) mod tests {
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
        let scanner = Scanner { cache: &cache, windows: false, written: &WrittenDirs::default() };
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
        assert_eq!(info.defines, ["X"]);
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
        age(&dir);
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: false, written: &WrittenDirs::default() };
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

    /// Sets a directory's modification time.
    fn set_dir_time(dir: &Path, t: SystemTime) {
        #[cfg(windows)]
        let f = {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_WRITE_ATTRIBUTES; FILE_FLAG_BACKUP_SEMANTICS (a directory).
            std::fs::OpenOptions::new().access_mode(0x100).custom_flags(0x0200_0000).open(dir).unwrap()
        };
        #[cfg(not(windows))]
        let f = std::fs::File::open(dir).unwrap();
        f.set_modified(t).unwrap();
    }

    /// Makes `dir` and the directories under it an hour old: their listings
    /// hold for later scans (see `Listed`).
    fn age(dir: &Path) {
        let hour_ago = SystemTime::now() - std::time::Duration::from_secs(3600);
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            todo.extend(std::fs::read_dir(&d).unwrap().flatten().filter(|e| e.file_type().unwrap().is_dir()).map(|e| e.path()));
            set_dir_time(&d, hour_ago);
        }
    }

    /// Listings and resolutions are kept for the stamps of the directories
    /// they're of, even where the build writes (one stat of each per scan):
    /// a scan again lists and resolves nothing; a directory changed is
    /// listed again, and what was looked for in it resolved again. One
    /// changed within the file system's clock tick (here, in the future) is
    /// listed again each time: a change within the tick could keep its
    /// stamp.
    #[test]
    fn listings_by_stamp() {
        let dir = tree("by_stamp", &[("src/main.c", "#include <a.h>\n#include <b.h>\n"), ("inc/a.h", "#include \"c.h\"\n"), ("gen/b.h", "")]);
        age(&dir);
        let cache = ScanCache::default();
        let written = WrittenDirs::new([dir.join("gen")], cfg!(windows));
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &written };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc"), dir.join("gen")], msvc: false };
        let stats = || cache.stats.lock().unwrap().clone();
        let scan = || rel(&dir, &scanner.scan(&[dir.join("src/main.c")], &sp).files);
        assert_eq!(scan(), ["gen/b.h", "inc/a.h", "src/main.c"]);
        let first = stats();
        // inc and gen: once each.
        assert_eq!((first.dirs_checked, first.dirs_listed, first.resolution_misses), (2, 2, 3));
        assert_eq!(scan(), ["gen/b.h", "inc/a.h", "src/main.c"]);
        let second = stats();
        assert_eq!((second.dirs_checked, second.dirs_listed, second.resolution_misses, second.resolution_hits), (4, 2, 3, 3));
        // c.h made in gen: gen listed again, what was looked for there
        // resolved again (main.c, a.h; c.h, new), not b.h.
        std::fs::write(dir.join("gen/c.h"), "").unwrap();
        assert_eq!(scan(), ["gen/b.h", "gen/c.h", "inc/a.h", "src/main.c"]);
        let third = stats();
        assert_eq!((third.dirs_listed - second.dirs_listed, third.resolution_misses - second.resolution_misses), (1, 3));
        // gen changed "within the tick": listed again by every scan.
        set_dir_time(&dir.join("gen"), SystemTime::now() + std::time::Duration::from_secs(3600));
        for _ in 0..2 {
            let before = stats();
            assert_eq!(scan(), ["gen/b.h", "gen/c.h", "inc/a.h", "src/main.c"]);
            assert_eq!(stats().dirs_listed - before.dirs_listed, 1);
        }
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
            let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
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
        // Directories are compared as Windows compares names.
        let written = WrittenDirs::new([dir.join(if cfg!(windows) { "BUILD" } else { "build" })], cfg!(windows));
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &written };
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
        // Without the files reasons name after them.
        let once =
            |a: &str, b: &str| format!("not exact ({a}: the same as a file with #pragma once (whether it's read again depends on the file system): {b})");
        assert!(cache.fell_back(&once("/s/a/x.h", "/s/b/x.h")));
        assert!(!cache.fell_back(&once("/s/c/y.h", "/s/d/y.h")));
        assert!(cache.fell_back("`..` after a symbolic link: /s/lnk"));
        assert!(!cache.fell_back("`..` after a symbolic link: /s/other"));
        assert_eq!(
            cache.fallback_summary().unwrap(),
            "11 compiles were scanned conservatively (2: `..` after a symbolic link; 2: not exact (__COUNTER__ in a condition or #include); \
             2: not exact (an #include not found); 2: not exact (the same as a file with #pragma once (whether it's read again depends on the \
             file system)); 2 more reasons)"
        );
    }

    /// A fake GCC-style compiler in `dir` named `name`, whose `-E -v -dM`
    /// says the search list `inc` and prints `macros`.
    pub(crate) fn fake_compiler(dir: &Path, name: &str, macros: &[&str]) -> PathBuf {
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
            test_tool(dir, name, "", &format!("printf '%s' '{list}' >&2\nprintf '%s' '{defines}'\n"))
        }
    }

    /// A test's fake tool in `dir`: `name.cmd` running `batch` on Windows, a
    /// script `name` running `sh` elsewhere. The script is run once before it's
    /// returned: another test's thread forking while it was open for writing
    /// leaves a child holding it so, and running it fails (ETXTBSY) until that
    /// child has started its own program. (As `cpp_deps`'s tests do.)
    pub(crate) fn test_tool(dir: &Path, name: &str, batch: &str, sh: &str) -> PathBuf {
        if cfg!(windows) {
            let tool = dir.join(format!("{name}.cmd"));
            std::fs::write(&tool, batch).unwrap();
            return tool;
        }
        let tool = dir.join(name);
        std::fs::write(&tool, format!("#!/bin/sh\n[ \"$1\" = --ninja-rs-ready ] && exit 0\n{sh}")).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        for _ in 0..1000 {
            match std::process::Command::new(&tool).arg("--ninja-rs-ready").status() {
                Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(std::time::Duration::from_millis(5)),
                r => {
                    assert!(r.unwrap().success());
                    return tool;
                }
            }
        }
        panic!("{} stays busy", tool.display());
    }

    /// Whether the compiler searches includers' directories: the compiler
    /// may say so where the flags don't (Clang for the MSVC ABI: a driver
    /// named after its target, a configuration file); the flags where it
    /// doesn't (Clang's MSVC compatibility, whatever the target, which
    /// `_MSC_VER` doesn't show). And forced includes are placed as it says.
    #[test]
    fn msvc_lookup_from_the_compiler() {
        let dir = test_dirs::TestDir::new("ninja_rs_scan_msvc_compiler");
        std::fs::write(dir.join("a.cc"), "").unwrap();
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let lookup = |compiler: &Path, flags: &[&str]| {
            let argv: Vec<String> = [compiler.to_string_lossy().into_owned()].into_iter().chain(s(flags)).chain(s(&["-c", "a.cc"])).collect();
            let info = parse_compile(&argv, &dir);
            assert_eq!(*scanner.builtin_dirs(compiler, &info).unwrap(), vec![dir.join("builtin")]);
            scanner.msvc_lookup(compiler, &info)
        };
        let msvc = fake_compiler(&dir, "x86_64-pc-windows-msvc-clang++", &["__clang__", "_MSC_VER"]);
        assert!(lookup(&msvc, &[]));
        let gnu = fake_compiler(&dir, "clang++", &["__clang__"]);
        assert!(!lookup(&gnu, &["--target=x86_64-linux-gnu"]));
        assert!(lookup(&gnu, &["--target=x86_64-linux-gnu", "-fms-compatibility"]));
        assert!(lookup(&gnu, &["--target=x86_64-linux-gnu", "-Xclang", "-fms-compatibility"]));
        let gcc = fake_compiler(&dir, "g++", &["__GNUC__"]);
        assert!(!lookup(&gcc, &[]));
        // pre.h next to the source, found there first (not in inc/).
        for f in ["src/a.cc", "src/pre.h", "inc/pre.h"] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "").unwrap();
        }
        let flags = ["--target=x86_64-linux-gnu", "-include", "pre.h", "-Iinc", "-c", "src/a.cc"];
        let argv: Vec<String> = [msvc.to_string_lossy().into_owned()].into_iter().chain(s(&flags)).collect();
        let mut info = parse_compile(&argv, &dir);
        scanner.builtin_dirs(&msvc, &info).unwrap();
        assert!(!info.msvc_lookup);
        info.place_forced_includes(scanner.msvc_lookup(&msvc, &info));
        assert_eq!(rel(&dir, &info.forced_includes.iter().cloned().collect()), ["inc/pre.h", "src/pre.h"]);
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

    /// A computed include the scan can't name sends the files under its
    /// directory (Boost.MPL's reach subdirectories), but another build
    /// directory's (the build directory in a source root), a repository's
    /// own, and of a directory the build writes to, its header-like files;
    /// in such a directory, the compile runs locally. Not the whole build
    /// directory: sources fetched there at configure time are sent.
    #[test]
    fn computed_include_directories() {
        let dir = tree(
            "computed_dirs",
            &[
                ("main.cpp", "#define STR(x) #x\n#define HDR(x) STR(sub/x.h)\n#include HDR(gen)\n"),
                ("sub/gen.h", ""),
                ("sub/deeper/more.h", ""),
                // Generated in the source tree.
                ("gen/out.h", ""),
                (".git/config", "GIT"),
                ("build/.ninja_log", "LOG"),
                ("build/app.exe", "EXE"),
                ("build/gen/main.cpp", "#include HDR(gen)\n"),
                ("build/_deps/lib-src/include/lib.h", "#include LIB_HEADER(x)\n"),
                ("build/_deps/lib-src/include/lib/x.h", ""),
                ("build/_deps/lib-build/lib.a", "A"),
                ("build-release/CMakeCache.txt", "CACHE"),
                ("build-release/obj.o", "O"),
            ],
        );
        let cache = ScanCache::default();
        let written_dirs = ["build", "build/gen", "build/_deps/lib-build", "gen"].map(|d| dir.join(d));
        let written = WrittenDirs::new(written_dirs, cfg!(windows));
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &written };
        let sent = |r: &ScanResult| rel(&dir, &crate::store::dir_files(&r.dirs, &|d| written.contains(d)).unwrap().into_iter().collect());
        let sp = SearchPath::default();
        let r = scanner.scan(&[dir.join("main.cpp")], &sp);
        assert!(r.local.is_none());
        assert_eq!(sent(&r), ["gen/out.h", "main.cpp", "sub/deeper/more.h", "sub/gen.h"]);
        let r = scanner.scan(&[dir.join("build/gen/main.cpp")], &sp);
        assert!(r.local.is_some_and(|why| why.contains("the build writes to")));
        // Fetched at configure time (CMake's FetchContent).
        let r = scanner.scan(&[dir.join("build/_deps/lib-src/include/lib.h")], &sp);
        assert!(r.local.is_none());
        assert_eq!(sent(&r), ["build/_deps/lib-src/include/lib/x.h", "build/_deps/lib-src/include/lib.h"]);
        // Too many to send: run locally.
        let big = dir.join("big");
        for i in 0..=crate::store::DIR_FILES_MAX {
            let sub = big.join(format!("{}", i % 3));
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("{i}.h")), "").unwrap();
        }
        assert!(crate::store::dir_files(&BTreeSet::from([big]), &|_| false).is_err());
    }

    /// Too many files under the directories of computed includes: those
    /// under the directories their paths name (Boost.MPL's
    /// `BOOST_PP_STRINGIZE(boost/mpl/list/AUX778076_LIST_HEADER)`), in the
    /// include or its macro's definition, with what they include; a macro
    /// the files define as a plain name, also as that name; unless a name
    /// of theirs is a macro of the command line's or the compiler's.
    #[test]
    fn computed_include_paths() {
        let dir = tree(
            "computed_paths",
            &[
                ("main.cpp", "#include <lib/big.h>\n#include <lib/aux_/inc.h>\n"),
                ("inc/lib/big.h", "#define STR(x) #x\n#define XSTR(x) STR(x)\n#include XSTR(lib/list/HEADER)\n"),
                ("inc/lib/list/list10.h", "#include <lib/config/typeof.h>\n"),
                ("inc/lib/config/typeof.h", "#include XSTR(lib/extra/HEADER)\n"),
                ("inc/lib/extra/x.h", ""),
                ("inc/lib/list/aux_/pre.h", ""),
                ("inc/lib/aux_/inc.h", "#define INC_STRING XSTR(lib/aux_/pre/HEADER)\n#include INC_STRING\n"),
                ("inc/lib/aux_/pre/plain/a.h", ""),
                ("other.cpp", "#define list vector\n#include <lib/big.h>\n"),
            ],
        );
        big_file(&dir.join("inc/lib/more/big.bin"));
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: false };
        let r = scanner.scan(&[dir.join("main.cpp")], &sp);
        assert!(crate::store::dir_files(&r.dirs, &|_| false).is_err());
        let narrowed = |r: &ScanResult, macros: &[&str]| {
            let dirs = scanner.narrowed_dirs(r, &sp, &|n| macros.contains(&n))?;
            Ok::<_, String>(dirs.into_iter().map(|(d, depth)| (d.strip_prefix(&*dir).unwrap().to_string_lossy().replace('\\', "/"), depth)).collect::<Vec<_>>())
        };
        let deep = crate::store::DIR_DEPTH_MAX;
        assert_eq!(narrowed(&r, &[]), Ok(vec![("inc/lib/aux_/pre".to_string(), deep), ("inc/lib/list".to_string(), deep)]));
        assert!(narrowed(&r, &["list"]).is_err());
        assert_eq!(
            rel(&dir, &crate::store::files_under(scanner.narrowed_dirs(&r, &sp, &|_| false).unwrap(), &|_| false).unwrap().into_iter().collect()),
            ["inc/lib/aux_/pre/plain/a.h", "inc/lib/list/aux_/pre.h", "inc/lib/list/list10.h"]
        );
        // And what those include, elsewhere: not with them.
        let computed = |r: &ScanResult, macros: &[&str]| scanner.computed_files(r, &sp, &|_| false, &|n| macros.contains(&n));
        let sent = rel(&dir, &computed(&r, &[]).unwrap().into_iter().collect());
        for f in ["inc/lib/config/typeof.h", "inc/lib/list/list10.h", "inc/lib/extra/x.h"] {
            assert!(sent.contains(&f.to_string()), "{f}: {sent:?}");
        }
        assert!(computed(&r, &["list"]).is_err());
        // `list` a macro the files define as `vector`: `lib/list/` (which
        // `#x` keeps), and `lib/vector/`, found nowhere.
        let r = scanner.scan(&[dir.join("other.cpp")], &sp);
        assert_eq!(narrowed(&r, &[]), Ok(vec![("inc/lib/list".to_string(), deep)]));
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
        age(&dir);
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: true };
        let files = rel(&dir, &scanner.scan(&[dir.join("src/main.cc")], &sp).files);
        assert_eq!(files, ["inc/l1/a.h", "inc/l1/y.h", "src/b.h", "src/deep/c.h", "src/deep/zz.h", "src/main.cc"]);
        // Each name in each directory once.
        let stats = || cache.stats.lock().unwrap().clone();
        let (dirs, names) = (3, 4);
        assert_eq!(stats().lookups, dirs * names);
        // Listed once: a scan again lists nothing.
        let listed = stats().dirs_listed;
        assert_eq!(rel(&dir, &scanner.scan(&[dir.join("src/main.cc")], &sp).files), files);
        assert_eq!(stats().dirs_listed, listed);
    }

    /// A `..` after a symbolic link: the compiler reads elsewhere than a
    /// remote worker would: local. Not on Windows, which applies `..`
    /// before following links (a junction here), as remote workers do.
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
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let r = scanner.scan(&[dir.join("main.c")], &SearchPath::default());
        if cfg!(windows) {
            assert!(r.local.is_none(), "{:?}", r.local);
            assert_eq!(rel(&dir, &r.files), ["b.h", "lnk/a.h", "main.c"]);
        } else {
            assert!(r.local.is_some_and(|why| why.contains("symbolic link")));
        }
    }

    /// A file rewritten with its size and modification time kept (an
    /// archive extracted over it) is read again: its stamp tells.
    #[test]
    fn rewritten_files() {
        let dir = tree("rewritten", &[("main.c", "#include \"a.h\"\n"), ("a.h", ""), ("b.h", "")]);
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let main = dir.join("main.c");
        let stamp = |p: &Path| ninja_os::stat(p).unwrap().1;
        let before = stamp(&main);
        assert_eq!(rel(&dir, &scanner.scan(std::slice::from_ref(&main), &SearchPath::default()).files), ["a.h", "main.c"]);
        // Once the file system's clock ticked: a new change time.
        loop {
            std::fs::write(dir.join("tick"), "").unwrap();
            if stamp(&dir.join("tick")).change > before.change {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::fs::write(&main, "#include \"b.h\"\n").unwrap();
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(before.mtime_ns);
        std::fs::File::options().write(true).open(&main).unwrap().set_modified(mtime).unwrap();
        assert_eq!((stamp(&main).len, stamp(&main).mtime_ns), (before.len, before.mtime_ns));
        assert_eq!(rel(&dir, &scanner.scan(std::slice::from_ref(&main), &SearchPath::default()).files), ["b.h", "main.c"]);
    }

    /// The compiler's run for its built-in directories is given up on when
    /// it hangs.
    #[test]
    fn builtin_dirs_time_out() {
        let dir = test_dirs::TestDir::new("ninja_rs_scan_hung");
        let compiler = test_tool(&dir, "hung", "@ping -n 30 127.0.0.1 >NUL\r\n", "sleep 30\n");
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

    /// A header a step that doesn't declare it installs in the build
    /// directory (ExternalProject's install tree), after another compile
    /// scanned the same search path: found then, ahead of the system's.
    /// And a name MSVC's lookup didn't find in a directory, found there
    /// once it's made.
    #[test]
    fn headers_made_after_a_scan() {
        let dir = tree(
            "made_later",
            &[("src/a.c", "#include <zlib.h>\n"), ("src/b.c", "#include <zlib.h>\n"), ("sys/zlib.h", "/*system*/\n"), ("build/obj/x.o", "")],
        );
        let build = dir.join("build");
        let cache = ScanCache::default();
        // The build directory and its edges' output directories.
        let written = WrittenDirs::new([build.clone(), build.join("obj"), build.join("ext-prefix/src/ext-stamp")], cfg!(windows));
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &written };
        let sp = SearchPath { quote: vec![], angled: vec![build.join("ext/include"), dir.join("sys")], msvc: false };
        let scan = |f: &str| rel(&dir, &scanner.scan(&[dir.join(f)], &sp).files);
        assert_eq!(scan("src/a.c"), ["src/a.c", "sys/zlib.h"]);
        std::fs::create_dir_all(build.join("ext/include")).unwrap();
        std::fs::write(build.join("ext/include/zlib.h"), "/*built*/\n").unwrap();
        assert_eq!(scan("src/b.c"), ["build/ext/include/zlib.h", "src/b.c", "sys/zlib.h"]);
        assert_eq!(scan("src/a.c"), ["build/ext/include/zlib.h", "src/a.c", "sys/zlib.h"]);
        // MSVC's lookup: "y.h" next to the source, made later.
        let dir = tree("made_later_msvc", &[("src/main.cc", "#include <x.h>\n"), ("inc/x.h", "#include \"y.h\"\n"), ("inc2/y.h", "")]);
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc"), dir.join("inc2")], msvc: true };
        let scan = || rel(&dir, &scanner.scan(&[dir.join("src/main.cc")], &sp).files);
        assert_eq!(scan(), ["inc/x.h", "inc2/y.h", "src/main.cc"]);
        std::fs::write(dir.join("src/y.h"), "").unwrap();
        assert_eq!(scan(), ["inc/x.h", "inc2/y.h", "src/main.cc", "src/y.h"]);
    }

    /// A file of `len` bytes (sparse where the file system can), which
    /// takes a walk of its directory over `store::DIR_BYTES_MAX`.
    fn big_file(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::File::create(p).unwrap().set_len(crate::store::DIR_BYTES_MAX + 1).unwrap();
    }

    /// Too many files to send for a computed include whose path a macro
    /// names: the macro's definitions, names (or paths), stand in for it
    /// (with its own name, which `#x` doesn't expand); otherwise the
    /// compile runs locally. Never the includer's directory alone, which lacks the
    /// header (here behind `__has_include`, which a remote compile would
    /// take the other branch of).
    #[test]
    fn computed_include_macro_paths() {
        let plat = "#include \"config.h\"\n#define STR(x) #x\n#define XSTR(x) STR(x)\n#if __has_include(XSTR(lib/PLATFORM/impl.h))\n\
                    #include XSTR(lib/PLATFORM/impl.h)\n#else\nint v = 2;\n#endif\n";
        let dir = tree(
            "macro_paths",
            &[
                ("main.cpp", "#include <lib/plat.h>\n"),
                ("inc/lib/plat.h", plat),
                ("inc/lib/config.h", "#define PLATFORM posix\n"),
                ("inc/lib/posix/impl.h", "int v = 1;\n"),
                ("other.cpp", "#define PLATFORM CAT(po, six)\n#include <lib/plat.h>\n"),
                ("ft.cpp", "#include <lib/ft.h>\n"),
                ("inc/lib/ft.h", "#define STR(x) #x\n#define XSTR(x) STR(x)\n#define AL_PATH lib/ft/impl\n#include XSTR(AL_PATH/master.h)\n"),
                ("inc/lib/ft/impl/master.h", "#undef AL_PATH\n#define AL_PATH lib/ft/other\n#include XSTR(AL_PATH/master.h)\n"),
                ("inc/lib/ft/other/master.h", ""),
            ],
        );
        big_file(&dir.join("inc/lib/filler/big.bin"));
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: false };
        let computed = |f: &str| {
            let r = scanner.scan(&[dir.join(f)], &sp);
            scanner.computed_files(&r, &sp, &|_| false, &|_| false).map(|files| rel(&dir, &files.into_iter().collect()))
        };
        let sent = computed("main.cpp").unwrap();
        assert!(sent.contains(&"inc/lib/posix/impl.h".to_string()), "{sent:?}");
        // Not names: local.
        let e = computed("other.cpp").unwrap_err();
        assert!(e.contains("PLATFORM"), "{e}");
        // A path (Boost.FunctionTypes'), defined again by a header it names.
        let sent = computed("ft.cpp").unwrap();
        for f in ["inc/lib/ft/impl/master.h", "inc/lib/ft/other/master.h"] {
            assert!(sent.contains(&f.to_string()), "{f}: {sent:?}");
        }
    }

    /// Narrowed, a computed include that may be a plain name keeps its
    /// directory's subdirectories: the macros it calls may make a path
    /// there (Boost.FunctionTypes' `<base_path/arity ## max_arity ## _ ##
    /// mfp.hpp>`).
    #[test]
    fn narrowing_keeps_plain_names_subdirectories() {
        let dir = tree(
            "narrow_plain",
            &[
                ("main.cpp", "#include <big/list.h>\n#include <ft/loop.h>\n"),
                ("inc/big/list.h", "#define STR(x) #x\n#include STR(big/pre/HEADER)\n"),
                ("inc/big/pre/list10.h", ""),
                (
                    "inc/ft/loop.h",
                    "#define AL_FILE(base, n) <base/arity ## n.h>\n#define AL_PATH ft/impl\n#define AL_PRE AL_FILE(AL_PATH, 10)\n#include AL_PRE\n",
                ),
                ("inc/ft/impl/arity10.h", ""),
            ],
        );
        big_file(&dir.join("inc/big/more/big.bin"));
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: false };
        let r = scanner.scan(&[dir.join("main.cpp")], &sp);
        let sent = rel(&dir, &scanner.computed_files(&r, &sp, &|_| false, &|_| false).unwrap().into_iter().collect());
        for f in ["inc/big/pre/list10.h", "inc/ft/impl/arity10.h"] {
            assert!(sent.contains(&f.to_string()), "{f}: {sent:?}");
        }
        assert!(!sent.contains(&"inc/big/more/big.bin".to_string()));
    }

    /// What is sent for a computed include is scanned: what it includes
    /// elsewhere goes too.
    #[test]
    fn sent_files_are_scanned() {
        let dir = tree(
            "sent_scanned",
            &[
                ("src/main.c", "#define STR(x) #x\n#define XSTR(x) STR(x)\n#include XSTR(sub/HEADER)\n"),
                ("src/sub/gen.h", "#include <dep.h>\n"),
                ("inc/dep.h", "#include \"more/dep2.h\"\n"),
                ("inc/more/dep2.h", ""),
            ],
        );
        age(&dir);
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: false };
        let sent = || {
            let r = scanner.scan(&[dir.join("src/main.c")], &sp);
            rel(&dir, &scanner.computed_files(&r, &sp, &|_| false, &|_| false).unwrap().into_iter().collect())
        };
        let first = sent();
        for f in ["inc/dep.h", "inc/more/dep2.h", "src/sub/gen.h"] {
            assert!(first.contains(&f.to_string()), "{f}: {first:?}");
        }
        // Walked once: another compile's walk is the one kept, while the
        // directories walked don't change.
        let walks = || cache.stats.lock().unwrap().walks;
        let walked = walks();
        assert_eq!(sent(), first);
        assert_eq!(walks(), walked);
        std::fs::write(dir.join("src/sub/new.h"), "").unwrap();
        assert!(sent().contains(&"src/sub/new.h".to_string()));
        assert!(walks() > walked);
    }

    /// A computed include over a directory the build writes headers to:
    /// those go (not its objects).
    #[test]
    fn computed_include_over_generated_headers() {
        let dir = tree(
            "gen_sub",
            &[("src/main.c", "#define STR(x) #x\n#define XSTR(x) STR(x)\n#include XSTR(gen/HEADER)\n"), ("src/gen/out.h", ""), ("src/gen/out.o", "O")],
        );
        let cache = ScanCache::default();
        let written = WrittenDirs::new([dir.join("src/gen"), dir.join("build")], cfg!(windows));
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &written };
        let r = scanner.scan(&[dir.join("src/main.c")], &SearchPath::default());
        assert!(r.local.is_none(), "{:?}", r.local);
        let sent = scanner.computed_files(&r, &SearchPath::default(), &|d| written.contains(d), &|_| false).map(|f| rel(&dir, &f.into_iter().collect()));
        assert_eq!(sent, Ok(vec!["src/gen/out.h".to_string(), "src/main.c".to_string()]));
    }

    /// Why narrowing fails, each time: a macro's name in a path, a computed
    /// include in a directory the build writes to among what narrowing
    /// finds, still too many files.
    #[test]
    fn narrowing_failures() {
        let dir = tree(
            "narrow_fail",
            &[
                ("a.c", "#include <lib/a.h>\n"),
                ("inc/lib/a.h", "#define STR(x) #x\n#include STR(lib/sub/NAME)\n"),
                ("inc/lib/sub/x.h", "#include <gen/g.h>\n"),
                ("inc/gen/g.h", "#include STR(gen/two/NAME)\n"),
                ("b.c", "#include <lib/b.h>\n"),
                ("inc/lib/b.h", "#define STR(x) #x\n#include STR(lib/big/NAME)\n"),
            ],
        );
        big_file(&dir.join("inc/lib/more/big.bin"));
        big_file(&dir.join("inc/lib/big/big.bin"));
        let cache = ScanCache::default();
        let written = WrittenDirs::new([dir.join("inc/gen")], cfg!(windows));
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &written };
        let sp = SearchPath { quote: vec![], angled: vec![dir.join("inc")], msvc: false };
        let failed = |f: &str, macros: &[&str]| {
            let r = scanner.scan(&[dir.join(f)], &sp);
            scanner.computed_files(&r, &sp, &|d| written.contains(d), &|n| macros.contains(&n)).unwrap_err()
        };
        let e = failed("a.c", &["sub"]);
        assert!(e.contains("sub") && e.contains("macro"), "{e}");
        let e = failed("a.c", &[]);
        assert!(e.contains("the build writes to"), "{e}");
        let e = failed("b.c", &[]);
        assert!(e.contains("MB"), "{e}");
    }

    /// `#pragma GCC dependency`, also as `_Pragma` in text or a macro: the
    /// file it names is looked up as an include's.
    #[test]
    fn pragma_dependency() {
        let d = parse_directives(
            b"#pragma GCC dependency \"dep.txt\" newer than this\n#pragma GCC dependency <dep2.txt>\n\
              _Pragma(\"GCC dependency <dep3.txt>\") int x;\n#define DEP _Pragma(\"GCC dependency \\\"dep4.txt\\\"\")\n#pragma once\n",
        );
        let names: Vec<(&str, bool)> = d.includes.iter().map(|i| (i.name.as_str(), i.angled)).collect();
        assert_eq!(names, [("dep.txt", false), ("dep2.txt", true), ("dep3.txt", true), ("dep4.txt", false)]);
    }

    /// The macros cl and clang-cl predefine: cl's from a list (it can't
    /// say), clang-cl's as it says.
    #[test]
    fn msvc_predefined_macros() {
        let dir = test_dirs::TestDir::new("ninja_rs_scan_msvc_macros");
        std::fs::write(dir.join("a.cc"), "").unwrap();
        let cache = ScanCache::default();
        let scanner = Scanner { cache: &cache, windows: cfg!(windows), written: &WrittenDirs::default() };
        let cl = PathBuf::from("cl.exe");
        let info = parse_compile(&s(&["cl.exe", "/c", "a.cc"]), &dir);
        scanner.builtin_dirs(&cl, &info).unwrap();
        for m in ["_WIN32", "_M_X64", "_MSC_VER", "_DEBUG"] {
            assert!(scanner.predefined(&cl, &info, m), "{m}");
        }
        assert!(!scanner.predefined(&cl, &info, "posix"));
        let clang_cl = fake_compiler(&dir, "clang-cl", &["_WIN32", "_M_ARM64"]);
        let argv: Vec<String> = [clang_cl.to_string_lossy().into_owned()].into_iter().chain(s(&["--driver-mode=cl", "/c", "a.cc"])).collect();
        let info = parse_compile(&argv, &dir);
        assert_eq!(info.style, Some(Style::Msvc));
        scanner.builtin_dirs(&clang_cl, &info).unwrap();
        assert!(scanner.predefined(&clang_cl, &info, "_M_ARM64"));
    }

    /// Compiles left to the conservative scanner: one reason for every
    /// operand of an operator it didn't know, and the summary names the
    /// most frequent four.
    #[test]
    fn fallback_reasons_capped() {
        let cache = ScanCache::default();
        assert!(cache.fell_back("not exact (unknown __has_builtin(__builtin_a))"));
        assert!(!cache.fell_back("not exact (unknown __has_builtin(__builtin_b))"));
        for (i, n) in [3, 1, 1, 1].iter().enumerate() {
            for _ in 0..*n {
                cache.fell_back(&format!("unsupported flag -f{i}"));
            }
        }
        let summary = cache.fallback_summary().unwrap();
        assert!(summary.starts_with("8 compiles were scanned conservatively (3: unsupported flag -f0; 2: not exact (unknown __has_builtin)"), "{summary}");
        assert!(summary.ends_with("; 1 more reason)"), "{summary}");
    }
}
