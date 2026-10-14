//! Scanning a compile command: its flags turned into a scan configuration,
//! the compiler probed once per compiler and set of flags that matter
//! (predefined macros, operators, dialect, built-in include directories),
//! and queries answered by probing the compiler, then scanning again.
//!
//! GCC and Clang command lines, and `cl`-style ones (`cl.exe`, `clang-cl`).
//!
//! Profiles can be saved in a directory (`Environment::profile_dir`), so
//! that later processes don't probe the compiler again: a file per profile,
//! with its whole key (the compiler's path, size and modification time, the
//! flags and the variables that matter) checked when loading.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::external::is_cl_style;
use crate::hmap::HeaderMap;
use crate::intern;
use crate::probe::{self, Style};
use crate::scan::{self, Cache, CommandMacro, Config, Forced, MacroTable, Oracle, Output, SearchDir};
use crate::vfs::Overlay;

/// A directory of the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dir {
    /// A directory, or a header map file.
    Path(PathBuf),
    /// A framework directory (`-F`, `-iframework`).
    Framework(PathBuf),
}

/// A compile command, as the scanner needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub compiler: PathBuf,
    pub style: Style,
    pub source: PathBuf,
    /// `c` or `c++`.
    pub lang: String,
    pub quote_dirs: Vec<Dir>,
    /// `-I`/`-F`, `/I`: searched before the system ones.
    pub user_dirs: Vec<Dir>,
    /// `-isystem`, `-iframework`, `/external:I`, `-imsvc`.
    pub system_dirs: Vec<Dir>,
    pub after_dirs: Vec<Dir>,
    pub embed_dirs: Vec<PathBuf>,
    pub overlays: Vec<PathBuf>,
    pub commands: Vec<CommandMacro>,
    pub forced: Vec<Forced>,
    /// `cl.exe`: its built-in directories are `INCLUDE`'s, unless `/X`.
    pub no_include_env: bool,
    /// `cl.exe`: the conforming preprocessor (`/Zc:preprocessor`), whose
    /// `_MSVC_TRADITIONAL` is 0.
    pub conforming: bool,
    /// The flags that may change predefined macros and built-in include
    /// directories (the probe's flags).
    pub probe_args: Vec<String>,
}

fn lang_of(source: &Path) -> Option<&'static str> {
    let ext = source.extension()?.to_string_lossy();
    Some(match ext.to_ascii_lowercase().as_str() {
        _ if ext == "C" => "c++",
        // Assembly the compiler preprocesses (`.s` it doesn't).
        _ if ext == "S" || ext == "sx" => "assembler-with-cpp",
        "c" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "cp" | "ixx" | "cppm" => "c++",
        _ => return None,
    })
}

/// What scans run compilers with.
#[derive(Clone, Debug, Default)]
pub struct Environment {
    /// The compilers' environment (None: this process's).
    pub vars: Option<Arc<Vec<(String, String)>>>,
    /// Where profiles are saved and found.
    pub profile_dir: Option<PathBuf>,
}

impl Environment {
    pub fn var(&self, name: &str) -> Option<String> {
        match &self.vars {
            None => std::env::var(name).ok(),
            // Names are case-insensitive on Windows.
            Some(vars) => vars.iter().find(|(k, _)| if cfg!(windows) { k.eq_ignore_ascii_case(name) } else { k == name }).map(|(_, v)| v.clone()),
        }
    }

    fn compiler<'a>(&'a self, inv: &'a Invocation, cwd: &'a Path) -> probe::Compiler<'a> {
        probe::Compiler { path: &inv.compiler, args: &inv.probe_args, style: inv.style, lang: &inv.lang, cwd, env: self.vars.as_ref().map(|v| v.as_slice()) }
    }
}

/// Variables that change what compilers predefine or search.
const PROFILE_VARS: &[&str] = &[
    "INCLUDE",
    "EXTERNAL_INCLUDE",
    "CL",
    "_CL_",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "OBJC_INCLUDE_PATH",
    "OBJCPLUS_INCLUDE_PATH",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "GCC_EXEC_PREFIX",
    "COMPILER_PATH",
    "CCC_OVERRIDE_OPTIONS",
    // Which Xcode an xcrun shim runs.
    "DEVELOPER_DIR",
    // Where clang-cl finds the MSVC headers without INCLUDE.
    "VCToolsInstallDir",
    "VCINSTALLDIR",
    "WindowsSdkDir",
    "WindowsSDKVersion",
    "UniversalCRTSdkDir",
    "UCRTVersion",
];

/// Parses a compile command (response files expanded).
pub fn parse(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
    parse_in(argv, cwd, &Environment::default())
}

/// Parses a compile command, `/external:env:` reading `env`.
pub fn parse_in(argv: &[String], cwd: &Path, env: &Environment) -> Result<Invocation, String> {
    if is_cl_style(argv) { parse_cl(argv, cwd, env) } else { parse_gcc(argv, cwd) }
}

fn new_invocation(argv: &[String], style: Style) -> Result<Invocation, String> {
    Ok(Invocation {
        compiler: PathBuf::from(argv.first().ok_or("empty command")?),
        style,
        source: PathBuf::new(),
        lang: String::new(),
        quote_dirs: Vec::new(),
        user_dirs: Vec::new(),
        system_dirs: Vec::new(),
        after_dirs: Vec::new(),
        embed_dirs: Vec::new(),
        overlays: Vec::new(),
        commands: Vec::new(),
        forced: Vec::new(),
        no_include_env: false,
        conforming: false,
        probe_args: Vec::new(),
    })
}

/// Whether two search directories are the same: the same kind, and the
/// same directory once symlinks are resolved (GCC compares inodes).
fn same_dir(a: &Dir, b: &Dir) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match (a, b) {
        (Dir::Path(x), Dir::Path(y)) | (Dir::Framework(x), Dir::Framework(y)) => x == y || real(x) == real(y),
        _ => false,
    }
}

/// GCC's and Clang's `-iquote` and `-I` lists as they search them: a
/// directory given twice is searched at its first place only, one that is
/// also a system directory (`-isystem`, built in, `-idirafter`) only at its
/// place among those, and the last `-iquote` one not if it's the first
/// `-I` one (GCC's `merge_include_chains`).
fn gcc_search_lists(quote: &[Dir], user: &[Dir], system: [&[Dir]; 3]) -> (Vec<Dir>, Vec<Dir>) {
    let is_system = |d: &Dir| system.iter().any(|s| s.iter().any(|x| same_dir(x, d)));
    let dedup = |dirs: &[Dir]| {
        let mut out: Vec<Dir> = Vec::new();
        for d in dirs {
            if !is_system(d) && !out.iter().any(|o| same_dir(o, d)) {
                out.push(d.clone());
            }
        }
        out
    };
    let user = dedup(user);
    let mut quote = dedup(quote);
    if let (Some(last), Some(first)) = (quote.last(), user.first())
        && same_dir(last, first)
    {
        quote.pop();
    }
    (quote, user)
}

fn finish(mut inv: Invocation, lang: Option<String>) -> Result<Invocation, String> {
    if inv.source.as_os_str().is_empty() {
        return Err("no source file".into());
    }
    inv.lang = match lang.as_deref() {
        // A header (a precompiled one's): preprocessed as its language.
        Some("c-header") => "c".into(),
        Some("c++-header") => "c++".into(),
        Some(l) => l.to_string(),
        None => lang_of(&inv.source).ok_or_else(|| format!("unknown language of {}", inv.source.display()))?.to_string(),
    };
    if !matches!(inv.lang.as_str(), "c" | "c++" | "assembler-with-cpp") {
        return Err(format!("unsupported language {}", inv.lang));
    }
    Ok(inv)
}

/// GCC and Clang flags with a separate argument, and whether the probe
/// keeps them.
const WITH_ARG: &[(&str, bool)] = &[
    ("-o", false),
    ("-MF", false),
    ("-MT", false),
    ("-MQ", false),
    ("-I", false),
    ("-F", false),
    ("-iquote", false),
    ("-isystem", false),
    ("-iframework", false),
    ("-idirafter", false),
    ("-ivfsoverlay", false),
    ("--embed-dir", false),
    ("-D", false),
    ("-U", false),
    ("-include", false),
    ("-imacros", false),
    ("-x", false),
    ("-target", true),
    ("--target", true),
    ("-isysroot", true),
    ("--sysroot", true),
    ("-arch", true),
    ("-mllvm", true),
    ("-resource-dir", true),
    ("-stdlib", true),
];

fn parse_gcc(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
    let abs = |p: &str| -> PathBuf { scan::normalize(&cwd.join(p)) };
    let forced = |p: &str| if Path::new(p).is_absolute() { Forced::Path(abs(p)) } else { Forced::Gcc { cwd: cwd.to_path_buf(), name: p.to_string() } };
    let mut inv = new_invocation(argv, Style::Gcc)?;
    let mut lang: Option<String> = None;
    // A precompiled header stands for the header it was made of, which
    // must be forced too: its macros and files are unknown otherwise.
    let mut include_pch = false;
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "--" {
            // The end of the options: inputs follow.
            for input in &argv[i..] {
                if !inv.source.as_os_str().is_empty() {
                    return Err(format!("more than one input ({} and {input})", inv.source.display()));
                }
                inv.source = abs(input);
            }
            break;
        }
        // cc1's spellings through the driver: "-Xclang -include -Xclang F"
        // (CMake's precompiled headers with Clang). A precompiled header
        // stands for the header it was made of, which CMake includes too:
        // the scan reads that one. Options about precompiled headers alone
        // don't concern the probe.
        if a == "-Xclang" {
            let v = argv.get(i).ok_or("-Xclang needs an argument")?;
            let operand = (argv.get(i + 1).map(String::as_str) == Some("-Xclang")).then(|| argv.get(i + 2)).flatten();
            match (v.as_str(), operand) {
                ("-include" | "-imacros", Some(f)) => {
                    inv.forced.push(forced(f));
                    i += 3;
                }
                ("-include-pch", Some(_)) => {
                    include_pch = true;
                    i += 3;
                }
                ("-emit-pch" | "-fno-pch-timestamp" | "-fno-validate-pch", _) => i += 1,
                _ => {
                    inv.probe_args.push(a.clone());
                    inv.probe_args.push(v.clone());
                    i += 1;
                }
            }
            continue;
        }
        if a == "-include-pch" {
            include_pch = true;
            i += 1;
            continue;
        }
        // Unsupported: what changes file lookup or macros beyond flags.
        for bad in ["-iprefix", "-iwithprefix", "-iwithprefixbefore", "-I-", "--driver-mode=cl"] {
            if a == bad || (a.starts_with(bad) && bad.len() > 3) {
                return Err(format!("unsupported flag {a}"));
            }
        }
        if let Some(&(flag, keep)) = WITH_ARG.iter().find(|(f, _)| a == f) {
            let v = argv.get(i).ok_or_else(|| format!("{flag} needs an argument"))?.clone();
            i += 1;
            match flag {
                "-I" => inv.user_dirs.push(Dir::Path(abs(&v))),
                "-F" => inv.user_dirs.push(Dir::Framework(abs(&v))),
                "-iquote" => inv.quote_dirs.push(Dir::Path(abs(&v))),
                "-isystem" => inv.system_dirs.push(Dir::Path(abs(&v))),
                "-iframework" => inv.system_dirs.push(Dir::Framework(abs(&v))),
                "-idirafter" => inv.after_dirs.push(Dir::Path(abs(&v))),
                "-ivfsoverlay" => inv.overlays.push(abs(&v)),
                "--embed-dir" => inv.embed_dirs.push(abs(&v)),
                "-D" => inv.commands.push(CommandMacro::Define(v)),
                "-U" => inv.commands.push(CommandMacro::Undef(v)),
                "-include" | "-imacros" => inv.forced.push(forced(&v)),
                "-x" => lang = Some(v),
                _ if keep => {
                    inv.probe_args.push(a.clone());
                    inv.probe_args.push(v);
                }
                _ => {}
            }
            continue;
        }
        let joined = |p: &str| a.strip_prefix(p).filter(|v| !v.is_empty());
        if let Some(v) = joined("-iquote") {
            inv.quote_dirs.push(Dir::Path(abs(v)));
        } else if let Some(v) = joined("-isystem") {
            inv.system_dirs.push(Dir::Path(abs(v)));
        } else if let Some(v) = joined("-iframework") {
            inv.system_dirs.push(Dir::Framework(abs(v)));
        } else if let Some(v) = joined("-idirafter") {
            inv.after_dirs.push(Dir::Path(abs(v)));
        } else if let Some(v) = joined("-ivfsoverlay") {
            inv.overlays.push(abs(v.trim_start_matches('=')));
        } else if let Some(v) = joined("--embed-dir=") {
            inv.embed_dirs.push(abs(v));
        } else if let Some(v) = joined("-I") {
            inv.user_dirs.push(Dir::Path(abs(v)));
        } else if let Some(v) = joined("-F") {
            inv.user_dirs.push(Dir::Framework(abs(v)));
        } else if let Some(v) = joined("-D") {
            inv.commands.push(CommandMacro::Define(v.to_string()));
        } else if let Some(v) = joined("-U") {
            inv.commands.push(CommandMacro::Undef(v.to_string()));
        } else if let Some(v) = joined("-x") {
            lang = Some(v.to_string());
        } else if a == "-c" || a == "-E" || a == "-S" || a.starts_with("-M") || a.starts_with("-o") || a.starts_with("-W") && !a.starts_with("-Wp,") {
            // Outputs, dependency files, warnings: no effect on macros.
        } else if a.starts_with("-g")
            || a.starts_with("-ffile-prefix-map=")
            || a.starts_with("-fdebug-prefix-map=")
            || a.starts_with("-fmacro-prefix-map=")
            || a.starts_with("-ffile-compilation-dir=")
            || a.starts_with("-fdebug-compilation-dir=")
        {
            // Debug information and path maps: no effect on macros.
        } else if a.starts_with('-') {
            inv.probe_args.push(a.clone());
        } else {
            if !inv.source.as_os_str().is_empty() {
                return Err(format!("more than one input ({} and {a})", inv.source.display()));
            }
            inv.source = abs(a);
        }
    }
    if include_pch && inv.forced.is_empty() {
        return Err("a precompiled header without the header it was made from".into());
    }
    finish(inv, lang)
}

/// cl's flags, without their `/` or `-`, that name outputs or only change
/// diagnostics, debug information or code generation outside the
/// preprocessor (dropped from the probe).
fn cl_dropped(f: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "Fo",
        "Fd",
        "Fe",
        "Fa",
        "Fi",
        "Fm",
        "Fp",
        "FR",
        "Fr",
        "Yc",
        "Yu",
        "Z7",
        "Zi",
        "ZI",
        "FS",
        "MP",
        "errorReport",
        "diagnostics",
        "Bt",
        "d1",
        "pathmap",
        "Brepro",
        "experimental:deterministic",
        "external:W",
        "external:templates",
        "analyze:",
        "wd",
        "we",
        "wo",
    ];
    // Warnings: /W0 to /W4, /Wall, /WX, /Wv:, /w, /w1 to /w4, clang-cl's
    // -Wfoo and -Wno-foo (not /winsdkdir and the like).
    let warning = f == "w"
        || f.starts_with("WX")
        || f.starts_with("Wv")
        || f == "Wall"
        || (f.len() == 2 && (f.starts_with('W') || f.starts_with('w')) && f.as_bytes()[1].is_ascii_digit())
        || (f.starts_with('W') && f.as_bytes().get(1).is_some_and(u8::is_ascii_lowercase));
    warning || f == "c" || f == "showIncludes" || f == "nologo" || f == "Gy" || f == "Gw" || PREFIXES.iter().any(|p| f.starts_with(p))
}

fn parse_cl(argv: &[String], cwd: &Path, env: &Environment) -> Result<Invocation, String> {
    let abs = |p: &str| -> PathBuf { scan::normalize(&cwd.join(p)) };
    let mut inv = new_invocation(
        argv,
        if Path::new(&argv[0].replace('\\', "/")).file_stem().is_some_and(|s| s.eq_ignore_ascii_case("cl")) { Style::Msvc } else { Style::ClangCl },
    )?;
    let mut lang: Option<String> = None;
    let mut i = 1;
    // A flag's value, joined (`/Idir`) or separate (`/I dir`).
    let value = |f: &str, name: &str, i: &mut usize| -> Result<Option<String>, String> {
        match f.strip_prefix(name) {
            Some("") => {
                let v = argv.get(*i).ok_or_else(|| format!("/{name} needs an argument"))?.clone();
                *i += 1;
                Ok(Some(v))
            }
            Some(v) => Ok(Some(v.to_string())),
            None => Ok(None),
        }
    };
    while i < argv.len() {
        let a = argv[i].clone();
        i += 1;
        if a == "--" {
            // The end of the options (CMake writes it before the source).
            for input in &argv[i..] {
                if !inv.source.as_os_str().is_empty() {
                    return Err(format!("more than one input ({} and {input})", inv.source.display()));
                }
                inv.source = abs(input);
            }
            break;
        }
        let Some(f) = a.strip_prefix('/').or_else(|| a.strip_prefix('-')).filter(|_| !(a.starts_with('/') && Path::new(&a).is_file())) else {
            if !inv.source.as_os_str().is_empty() {
                return Err(format!("more than one input ({} and {a})", inv.source.display()));
            }
            inv.source = abs(&a);
            continue;
        };
        if f.starts_with("Yu") || f.starts_with("Yc") {
            // A precompiled header stands for the headers its source
            // includes, which the scan reads as usual.
            continue;
        }
        if f == "sourceDependencies" {
            i += 1;
            continue;
        }
        if cl_dropped(f) {
            continue;
        }
        if let Some(v) = value(f, "external:I", &mut i)? {
            inv.system_dirs.push(Dir::Path(abs(&v)));
        } else if let Some(var) = f.strip_prefix("external:env:") {
            let dirs = env.var(var).unwrap_or_default();
            inv.system_dirs.extend(dirs.split(';').filter(|d| !d.is_empty()).map(|d| Dir::Path(abs(d))));
        } else if let Some(v) = value(f, "imsvc", &mut i)? {
            inv.system_dirs.push(Dir::Path(abs(&v)));
        } else if let Some(v) = value(f, "ivfsoverlay", &mut i)? {
            inv.overlays.push(abs(&v));
        } else if let Some(v) = f.strip_prefix("vfsoverlay:") {
            inv.overlays.push(abs(v));
        } else if let Some(v) = value(f, "FI", &mut i)? {
            // cl searches it as `#include "..."` in the source; clang-cl
            // takes it as `-include`.
            inv.forced.push(if Path::new(&v).is_absolute() {
                Forced::Path(abs(&v))
            } else if inv.style == Style::Msvc {
                Forced::Msvc(v)
            } else {
                Forced::Gcc { cwd: cwd.to_path_buf(), name: v }
            });
        } else if let Some(v) = value(f, "Tp", &mut i)? {
            inv.source = abs(&v);
            lang = Some("c++".into());
        } else if let Some(v) = value(f, "Tc", &mut i)? {
            inv.source = abs(&v);
            lang = Some("c".into());
        } else if f == "TP" {
            lang = Some("c++".into());
        } else if f == "TC" {
            lang = Some("c".into());
        } else if f == "X" {
            inv.no_include_env = true;
        } else if let Some(v) = value(f, "I", &mut i)? {
            inv.user_dirs.push(Dir::Path(abs(&v)));
        } else if let Some(v) = value(f, "D", &mut i)? {
            // cl's `/DNAME#VALUE` is `/DNAME=VALUE`.
            inv.commands.push(CommandMacro::Define(if v.contains('=') { v } else { v.replacen('#', "=", 1) }));
        } else if let Some(v) = value(f, "U", &mut i)? {
            inv.commands.push(CommandMacro::Undef(v));
        } else if f.starts_with("clang:") && (f.contains("prefix-map") || f.contains("compilation-dir") || f.starts_with("clang:-M")) {
            // Paths and dependency files: no effect on macros.
        } else if f == "Xclang" || f == "target" {
            let v = argv.get(i).cloned().unwrap_or_default();
            i += 1;
            // `-Xclang -ivfsoverlay -Xclang <file>`: an overlay too.
            if f == "Xclang" && v == "-ivfsoverlay" && argv.get(i).is_some_and(|x| x == "-Xclang") {
                if let Some(o) = argv.get(i + 1) {
                    inv.overlays.push(abs(o));
                }
                inv.probe_args.extend([a.clone(), v, argv[i].clone(), argv.get(i + 1).cloned().unwrap_or_default()]);
                i += 2;
                continue;
            }
            inv.probe_args.push(a.clone());
            inv.probe_args.push(v);
        } else {
            if f == "Zc:preprocessor" || f == "experimental:preprocessor" {
                inv.conforming = true;
            }
            if f == "Zc:preprocessor-" {
                inv.conforming = false;
            }
            inv.probe_args.push(a.clone());
        }
    }
    let inv = finish(inv, lang)?;
    // C11 and later use the conforming preprocessor by default.
    let c_std = inv.probe_args.iter().any(|a| matches!(a.trim_start_matches(['/', '-']), "std:c11" | "std:c17" | "std:clatest"));
    Ok(Invocation { conforming: inv.conforming || (inv.lang == "c" && c_std), ..inv })
}

/// What a compiler predefines and searches, for a set of flags.
pub struct Profile {
    /// The `#define` lines `predefined` comes from.
    predefines: String,
    pub predefined: Arc<MacroTable>,
    pub operators: Arc<HashSet<intern::Sym>>,
    pub dialect: crate::expr::Dialect,
    pub builtin_dirs: Vec<Dir>,
    /// Files every compile reads first (see `probe::Probe::implicit`).
    pub implicit: Vec<PathBuf>,
    /// Query answers learned so far.
    answers: Mutex<HashMap<(String, String), Option<i64>>>,
    /// Held while the compiler answers queries, so that concurrent scans
    /// don't ask the same ones.
    asking: Mutex<()>,
    /// When the compiler was probed, in seconds since the Unix epoch.
    probed: u64,
}

const PROFILE_SIGNATURE: &str = "cpp-deps profile 2";

/// Profiles unused for this long are removed when another is saved.
const PROFILE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Profiles are probed again after this long, however often used: what the
/// key doesn't see (a clang configuration file's contents, an SDK installed
/// behind a shim) is then stale for a day at most.
const PROFILE_LIFETIME: Duration = Duration::from_secs(24 * 3600);

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Profile {
    fn new(predefines: String, operators: Vec<String>, dialect: crate::expr::Dialect, builtin_dirs: Vec<Dir>, implicit: Vec<PathBuf>) -> Profile {
        Profile {
            predefined: Arc::new(scan::macro_table(&predefines)),
            predefines,
            operators: Arc::new(operators.iter().map(|o| intern::intern(o)).collect()),
            dialect,
            builtin_dirs,
            implicit,
            answers: Mutex::default(),
            asking: Mutex::default(),
            probed: unix_secs(SystemTime::now()),
        }
    }

    /// The file of the profile with `key` in `dir`.
    fn file(dir: &Path, key: &str) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let hash = |salt: u8| {
            let mut h = std::hash::DefaultHasher::new();
            (salt, key).hash(&mut h);
            h.finish()
        };
        dir.join(format!("{:016x}{:016x}.profile", hash(0), hash(1)))
    }

    /// Saves the profile with `key` (which it's loaded with): line-based,
    /// the `#define` lines last.
    fn save(&self, dir: &Path, key: &str) -> std::io::Result<()> {
        let bad = |s: &str| s.contains(['\n', '\r', '\t']);
        if bad(key) {
            return Ok(());
        }
        let mut out = format!(
            "{PROFILE_SIGNATURE}\nkey\t{key}\nprobed\t{}\ndialect\t{}\t{}\n",
            self.probed, self.dialect.true_false as u8, self.dialect.alt_operators as u8
        );
        let mut operators: Vec<&str> = self.operators.iter().map(|o| o.as_str()).collect();
        operators.sort();
        for o in operators {
            out.push_str(&format!("operator\t{o}\n"));
        }
        for d in &self.builtin_dirs {
            let (kind, p) = match d {
                Dir::Path(p) => ("dir", p),
                Dir::Framework(p) => ("framework", p),
            };
            out.push_str(&format!("{kind}\t{}\n", p.display()));
        }
        for f in &self.implicit {
            out.push_str(&format!("implicit\t{}\n", f.display()));
        }
        let mut answers: Vec<_> = self.answers.lock().unwrap().iter().map(|(q, v)| (q.clone(), *v)).collect();
        answers.sort();
        for ((op, arg), v) in answers {
            if !bad(&op) && !bad(&arg) {
                out.push_str(&format!("answer\t{op}\t{arg}\t{}\n", v.map_or("-".to_string(), |v| v.to_string())));
            }
        }
        out.push_str("predefines\n");
        out.push_str(&self.predefines);
        std::fs::create_dir_all(dir)?;
        let file = Profile::file(dir, key);
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = file.with_extension(format!("tmp{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &file)?;
        // Profiles of compilers gone or changed since.
        let now = SystemTime::now();
        for e in std::fs::read_dir(dir)?.flatten() {
            let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > PROFILE_MAX_AGE);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(())
    }

    /// The profile with `key` saved in `dir`, if any.
    fn load(dir: &Path, key: &str) -> Option<Profile> {
        let file = Profile::file(dir, key);
        let text = std::fs::read_to_string(&file).ok()?;
        let (head, predefines) = text.split_once("\npredefines\n")?;
        let mut lines = head.lines();
        if lines.next() != Some(PROFILE_SIGNATURE) || lines.next()?.strip_prefix("key\t") != Some(key) {
            return None;
        }
        let (mut probed, mut dialect, mut operators, mut dirs, mut implicit, mut answers) = (None, None, Vec::new(), Vec::new(), Vec::new(), HashMap::new());
        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["probed", t] => probed = Some(t.parse::<u64>().ok()?),
                ["dialect", t, a] => dialect = Some(crate::expr::Dialect { true_false: *t == "1", alt_operators: *a == "1" }),
                ["operator", o] => operators.push(o.to_string()),
                ["dir", p] => dirs.push(Dir::Path(PathBuf::from(p))),
                ["framework", p] => dirs.push(Dir::Framework(PathBuf::from(p))),
                ["implicit", p] => implicit.push(PathBuf::from(p)),
                ["answer", op, arg, v] => {
                    let v = if *v == "-" { None } else { Some(v.parse().ok()?) };
                    answers.insert((op.to_string(), arg.to_string()), v);
                }
                _ => return None,
            }
        }
        let probed = probed?;
        if unix_secs(SystemTime::now()).saturating_sub(probed) > PROFILE_LIFETIME.as_secs() {
            return None;
        }
        let mut profile = Profile::new(predefines.to_string(), operators, dialect?, dirs, implicit);
        profile.probed = probed;
        *profile.answers.lock().unwrap() = answers;
        // Used: kept by the clean-up of `save`.
        if let Ok(f) = std::fs::File::options().append(true).open(&file) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(profile)
    }
}

struct ProfileOracle<'a>(&'a Profile);

impl Oracle for ProfileOracle<'_> {
    fn query(&self, op: &str, arg: &str) -> Option<i64> {
        self.0.answers.lock().unwrap().get(&(op.to_string(), arg.to_string())).copied().flatten()
    }
}

/// Something loaded once (a profile, an overlay, a header map), or why it
/// couldn't be (not tried again); concurrent users wait for the first.
type Loaded<T> = Arc<OnceLock<Result<Arc<T>, String>>>;

fn load_once<K: std::hash::Hash + Eq, T>(map: &Mutex<HashMap<K, Loaded<T>>>, key: K, load: impl FnOnce() -> Result<Arc<T>, String>) -> Result<Arc<T>, String> {
    let cell = map.lock().unwrap().entry(key).or_default().clone();
    cell.get_or_init(load).clone()
}

/// Scans compile commands, with everything learned about compilers cached.
pub struct Driver {
    pub cache: Cache,
    profiles: Mutex<HashMap<String, Loaded<Profile>>>,
    /// Compilers' fingerprints (`probe::fingerprint`), by path, taken once
    /// per session (build).
    fingerprints: Mutex<HashMap<PathBuf, (u64, String)>>,
    overlays: Mutex<HashMap<PathBuf, Loaded<Overlay>>>,
    header_maps: Mutex<HashMap<PathBuf, Loaded<HeaderMap>>>,
}

impl Driver {
    pub fn new(cache: Cache) -> Self {
        Driver { cache, profiles: Mutex::default(), fingerprints: Mutex::default(), overlays: Mutex::default(), header_maps: Mutex::default() }
    }

    /// A compiler's fingerprint, taken once per session.
    fn fingerprint(&self, inv: &Invocation, cwd: &Path, env: &Environment) -> String {
        let session = self.cache.session();
        if let Some((s, f)) = self.fingerprints.lock().unwrap().get(&inv.compiler)
            && *s == session
        {
            return f.clone();
        }
        let f = probe::fingerprint(&env.compiler(inv, cwd));
        self.fingerprints.lock().unwrap().insert(inv.compiler.clone(), (session, f.clone()));
        f
    }

    /// The key of a compiler's profile for an invocation's flags: what
    /// may change its answers, the compiler's version included (its file,
    /// and its banner: a wrapper's file doesn't change with the compiler).
    fn profile_key(&self, inv: &Invocation, cwd: &Path, env: &Environment) -> String {
        let fingerprint = self.fingerprint(inv, cwd, env);
        let meta = std::fs::metadata(&inv.compiler).ok();
        let mtime = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        let vars: Vec<String> = PROFILE_VARS.iter().filter_map(|v| env.var(v).map(|val| format!("{v}={val}"))).collect();
        format!(
            "{}\u{1}{}\u{1}{mtime}\u{1}{:?}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{:016x}",
            inv.compiler.display(),
            meta.map_or(0, |m| m.len()),
            inv.style,
            inv.lang,
            cwd.display(),
            inv.conforming,
            inv.no_include_env,
            vars.join("\u{1}"),
            inv.probe_args.join("\u{1}"),
            {
                use std::hash::{Hash, Hasher};
                let mut h = std::hash::DefaultHasher::new();
                fingerprint.hash(&mut h);
                h.finish()
            }
        )
    }

    /// The profile of a compiler for an invocation's flags: probed once,
    /// or loaded from the profile directory.
    pub fn profile(&self, inv: &Invocation, cwd: &Path, env: &Environment) -> Result<Arc<Profile>, String> {
        let key = self.profile_key(inv, cwd, env);
        load_once(&self.profiles, key.clone(), || {
            if let Some(p) = env.profile_dir.as_deref().and_then(|d| Profile::load(d, &key)) {
                return Ok(Arc::new(p));
            }
            let compiler = env.compiler(inv, cwd);
            let p = probe::probe(&compiler)?;
            let builtin_dirs = match inv.style {
                Style::Msvc if inv.no_include_env => Vec::new(),
                Style::Msvc => {
                    env.var("INCLUDE").unwrap_or_default().split(';').filter(|d| !d.is_empty()).map(|d| Dir::Path(scan::normalize(&cwd.join(d)))).collect()
                }
                _ => probe::builtin_dirs(&compiler)?
                    .into_iter()
                    .map(|(d, framework)| {
                        let d = scan::normalize(&cwd.join(d));
                        if framework { Dir::Framework(d) } else { Dir::Path(d) }
                    })
                    .collect(),
            };
            let mut predefines = p.predefines;
            if inv.style == Style::Msvc {
                // The probe used the conforming preprocessor.
                let traditional = if inv.conforming { "0" } else { "1" };
                predefines.push_str(&format!("#define _MSVC_TRADITIONAL {traditional}\n"));
            }
            let implicit = p.implicit.iter().map(|f| scan::normalize(&cwd.join(f))).collect();
            let profile = Profile::new(predefines, p.operators, p.dialect, builtin_dirs, implicit);
            if let Some(d) = &env.profile_dir {
                let _ = profile.save(d, &key);
            }
            Ok(Arc::new(profile))
        })
    }

    fn overlay(&self, path: &Path, cwd: &Path) -> Result<Arc<Overlay>, String> {
        load_once(&self.overlays, path.to_path_buf(), || Overlay::load(path, cwd).map(Arc::new))
    }

    /// A search directory: a header map when the path is one.
    fn search_dir(&self, d: &Dir, cwd: &Path) -> Result<SearchDir, String> {
        Ok(match d {
            Dir::Framework(p) => SearchDir::Framework(p.clone()),
            Dir::Path(p) if p.is_file() && HeaderMap::is_header_map(p) => {
                SearchDir::HeaderMap(load_once(&self.header_maps, p.clone(), || HeaderMap::load(p, cwd).map(Arc::new))?)
            }
            Dir::Path(p) => SearchDir::Dir(p.clone()),
        })
    }

    /// Scans a compile command run in `cwd`. Unknown queries are asked to
    /// the compiler, then the scan runs again.
    pub fn scan(&self, argv: &[String], cwd: &Path) -> Result<Output, String> {
        self.scan_in(argv, cwd, &Environment::default())
    }

    /// `scan`, running compilers in `env`.
    pub fn scan_in(&self, argv: &[String], cwd: &Path, env: &Environment) -> Result<Output, String> {
        let inv = parse_in(argv, cwd, env)?;
        let profile = self.profile(&inv, cwd, env)?;
        let dirs = |ds: &[Dir]| ds.iter().map(|d| self.search_dir(d, cwd)).collect::<Result<Vec<_>, _>>();
        let (quote, user) = if inv.style == Style::Gcc {
            gcc_search_lists(&inv.quote_dirs, &inv.user_dirs, [&inv.system_dirs[..], &profile.builtin_dirs, &inv.after_dirs])
        } else {
            (inv.quote_dirs.clone(), inv.user_dirs.clone())
        };
        let mut system: Vec<Dir> = Vec::new();
        for d in inv.system_dirs.iter().chain(&profile.builtin_dirs).chain(&inv.after_dirs) {
            if inv.style != Style::Gcc || !system.iter().any(|s| same_dir(s, d)) {
                system.push(d.clone());
            }
        }
        let mut angled = dirs(&user)?;
        angled.extend(dirs(&system)?);
        let cfg = Config {
            dialect: profile.dialect,
            quote_dirs: dirs(&quote)?,
            angled_dirs: angled,
            embed_dirs: inv.embed_dirs.clone(),
            overlays: inv.overlays.iter().map(|o| self.overlay(o, cwd)).collect::<Result<_, _>>()?,
            msvc_lookup: inv.style != Style::Gcc,
            predefined: profile.predefined.clone(),
            commands: inv.commands.clone(),
            forced: profile.implicit.iter().cloned().map(Forced::Path).chain(inv.forced.iter().cloned()).collect(),
            operators: profile.operators.clone(),
        };
        // Each answer may reveal more queries; a few rounds of asking settle
        // them. Concurrent scans share the answers: a scan whose unknown
        // queries were answered meanwhile (by another scan) scans again.
        let (mut asked, mut rounds) = (0, 0);
        loop {
            rounds += 1;
            let out = scan::scan(&self.cache, &cfg, &ProfileOracle(&profile), &inv.source);
            if out.unknown.is_empty() || rounds > 10 {
                return Ok(out);
            }
            let unanswered = |out: &Output| -> Vec<(String, String)> {
                let answers = profile.answers.lock().unwrap();
                out.unknown.iter().filter(|q| !answers.contains_key(q)).cloned().collect()
            };
            // Answered now (with a value): scan again.
            let answered = || {
                let answers = profile.answers.lock().unwrap();
                out.unknown.iter().any(|q| matches!(answers.get(q), Some(Some(_))))
            };
            if unanswered(&out).is_empty() {
                if answered() {
                    continue;
                }
                // The compiler can't answer them: the result stays inexact.
                return Ok(out);
            }
            // One scan asks at a time; the others then find their answers.
            let _asking = profile.asking.lock().unwrap();
            let unknown = unanswered(&out);
            if unknown.is_empty() {
                continue;
            }
            if asked == 4 {
                return Ok(out);
            }
            asked += 1;
            let values = probe::answer(&env.compiler(&inv, cwd), &unknown)?;
            let mut answers = profile.answers.lock().unwrap();
            for (q, v) in unknown.into_iter().zip(values) {
                answers.insert(q, v);
            }
            drop(answers);
            if let Some(d) = &env.profile_dir {
                let _ = profile.save(d, &self.profile_key(&inv, cwd, env));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn gcc_search_list_rules() {
        let d = |s: &str| Dir::Path(p(s));
        // Duplicates searched at their first place; user directories that
        // are system ones (`-I/usr/include`) only at the system place; the
        // last -iquote one not when it's the first -I one.
        let (quote, user) = gcc_search_lists(
            &[d("/q"), d("/q"), d("/sys"), d("/u")],
            &[d("/u"), d("/builtin"), d("/u"), d("/v"), d("/after")],
            [&[d("/sys")], &[d("/builtin")], &[d("/after")]],
        );
        assert_eq!(quote, vec![d("/q")]);
        assert_eq!(user, vec![d("/u"), d("/v")]);
        // A framework directory isn't the directory of the same path.
        let (_, user) = gcc_search_lists(&[], &[Dir::Framework(p("/f"))], [&[d("/f")], &[], &[]]);
        assert_eq!(user, vec![Dir::Framework(p("/f"))]);
    }

    #[test]
    fn parses_gcc_commands() {
        let cwd = Path::new("/b");
        let inv = parse(
            &s(&[
                "/tc/clang++",
                "--target=x86_64-linux-gnu",
                "-I",
                "inc",
                "-I/abs",
                "-F",
                "fw",
                "-isystem",
                "/sys",
                "-iframework",
                "/sysfw",
                "-iquote",
                "q",
                "-DA=1",
                "-D",
                "B",
                "-UC",
                "-include",
                "pre.h",
                "-ivfsoverlay",
                "o.yaml",
                "--embed-dir=data",
                "-O2",
                "-g",
                "-std=c++17",
                "-MD",
                "-MF",
                "x.d",
                "-MT",
                "x.o",
                "-Wall",
                "-ffile-prefix-map=/a=/b",
                "-o",
                "x.o",
                "-c",
                "../src/x.cpp",
            ]),
            cwd,
        )
        .unwrap();
        assert_eq!(inv.style, Style::Gcc);
        assert_eq!(inv.source, p("/src/x.cpp"));
        assert_eq!(inv.lang, "c++");
        assert_eq!(inv.user_dirs, vec![Dir::Path(p("/b/inc")), Dir::Path(p("/abs")), Dir::Framework(p("/b/fw"))]);
        assert_eq!(inv.system_dirs, vec![Dir::Path(p("/sys")), Dir::Framework(p("/sysfw"))]);
        assert_eq!(inv.quote_dirs, vec![Dir::Path(p("/b/q"))]);
        assert_eq!(inv.overlays, vec![p("/b/o.yaml")]);
        assert_eq!(inv.embed_dirs, vec![p("/b/data")]);
        assert_eq!(inv.commands, vec![CommandMacro::Define("A=1".into()), CommandMacro::Define("B".into()), CommandMacro::Undef("C".into())]);
        assert_eq!(inv.forced, vec![Forced::Gcc { cwd: cwd.to_path_buf(), name: "pre.h".into() }]);
        assert_eq!(inv.probe_args, s(&["--target=x86_64-linux-gnu", "-O2", "-std=c++17"]));
        assert!(parse(&s(&["cc", "-iprefix", "p/", "-c", "a.c"]), cwd).is_err());
        assert!(parse(&s(&["cc", "-c", "a.f90"]), cwd).is_err());
        assert_eq!(parse(&s(&["cc", "-x", "c", "-c", "a.inc"]), cwd).unwrap().lang, "c");
        // CMake's precompiled headers with Clang: made from a header, then
        // used with it forced.
        let pch = parse(
            &s(&[
                "clang++",
                "-Xclang",
                "-fno-pch-timestamp",
                "-Xclang",
                "-emit-pch",
                "-Xclang",
                "-include",
                "-Xclang",
                "/b/pch.hxx",
                "-x",
                "c++-header",
                "-c",
                "pch.hxx.cxx",
            ]),
            cwd,
        )
        .unwrap();
        // An absolute path (only on Unix hosts without a drive).
        let header = || {
            if Path::new("/b/pch.hxx").is_absolute() {
                Forced::Path(p("/b/pch.hxx"))
            } else {
                Forced::Gcc { cwd: cwd.to_path_buf(), name: "/b/pch.hxx".into() }
            }
        };
        assert_eq!((pch.lang.as_str(), pch.forced.clone(), pch.probe_args.clone()), ("c++", vec![header()], Vec::<String>::new()));
        let user = parse(
            &s(&[
                "clang++",
                "-Xclang",
                "-include-pch",
                "-Xclang",
                "/b/pch.hxx.pch",
                "-Xclang",
                "-include",
                "-Xclang",
                "/b/pch.hxx",
                "-Xclang",
                "-fno-x",
                "-c",
                "a.cpp",
            ]),
            cwd,
        )
        .unwrap();
        assert_eq!((user.forced.clone(), user.probe_args.clone()), (vec![header()], s(&["-Xclang", "-fno-x"])));
        // Without the header it was made from, its macros are unknown.
        assert!(parse(&s(&["clang++", "-include-pch", "/b/pch.hxx.pch", "-c", "a.cpp"]), cwd).is_err());
        assert!(parse(&s(&["clang++", "-Xclang", "-include-pch", "-Xclang", "/b/pch.hxx.pch", "-c", "a.cpp"]), cwd).is_err());
        assert_eq!(parse(&s(&["cc", "-c", "blake3.S"]), cwd).unwrap().lang, "assembler-with-cpp");
    }

    #[test]
    fn parses_cl_commands() {
        let cwd = Path::new("C:/b");
        let inv = parse(
            &s(&[
                "C:/VC/bin/cl.exe",
                "/nologo",
                "/TP",
                "/Iinc",
                "/I",
                "C:/abs",
                "/external:I",
                "C:/ext",
                "/external:W0",
                "-imsvc",
                "C:/sdk",
                "/DA=1",
                "/DB#2",
                "/D",
                "C",
                "/UD",
                "/FIpre.h",
                "/std:c++20",
                "/EHsc",
                "/MDd",
                "/Zc:preprocessor",
                "/W4",
                "/Zi",
                "/FdC:/b/x.pdb",
                "/FoC:/b/x.obj",
                "/showIncludes",
                "/X",
                "/c",
                "src/x.cc",
            ]),
            cwd,
        )
        .unwrap();
        assert_eq!(inv.style, Style::Msvc);
        assert_eq!(inv.lang, "c++");
        assert_eq!(inv.source, p("C:/b/src/x.cc"));
        // Drive paths are only absolute on Windows hosts.
        let abs = |d: &str| if cfg!(windows) { p(d) } else { p(&format!("C:/b/{d}")) };
        assert_eq!(inv.user_dirs, vec![Dir::Path(p("C:/b/inc")), Dir::Path(abs("C:/abs"))]);
        assert_eq!(inv.system_dirs, vec![Dir::Path(abs("C:/ext")), Dir::Path(abs("C:/sdk"))]);
        assert_eq!(
            inv.commands,
            vec![CommandMacro::Define("A=1".into()), CommandMacro::Define("B=2".into()), CommandMacro::Define("C".into()), CommandMacro::Undef("D".into())]
        );
        assert_eq!(inv.forced, vec![Forced::Msvc("pre.h".into())]);
        assert!(inv.no_include_env && inv.conforming);
        // The probe passes the language itself.
        assert_eq!(inv.probe_args, s(&["/std:c++20", "/EHsc", "/MDd", "/Zc:preprocessor"]));
        let clang_cl = parse(&s(&["clang-cl", "/clang:-ffile-prefix-map=/a=/b", "/vfsoverlay:o.yaml", "/std:c17", "/c", "--", "x.c"]), cwd).unwrap();
        assert_eq!(clang_cl.source, p("C:/b/x.c"));
        assert_eq!(clang_cl.style, Style::ClangCl);
        assert_eq!(clang_cl.lang, "c");
        assert_eq!(clang_cl.overlays, vec![p("C:/b/o.yaml")]);
        assert!(clang_cl.conforming);
        assert_eq!(clang_cl.probe_args, s(&["/std:c17"]));
    }

    /// The whole driver against cl.exe, when `CPP_DEPS_TEST_CL` names one
    /// (with INCLUDE set for its headers).
    #[test]
    fn real_cl() {
        let Some(cl) = std::env::var_os("CPP_DEPS_TEST_CL") else { return };
        let dir = std::env::temp_dir().join(format!("cpp_deps_driver_cl_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("inc")).unwrap();
        std::fs::write(
            dir.join("main.cc"),
            "#include <a.h>\n#if _MSC_VER >= 1900 and defined(_CPPUNWIND) and _MSVC_TRADITIONAL\n#include \"yes.h\"\n#endif\n\
             #if __has_cpp_attribute(nodiscard)\n#include \"attr.h\"\n#endif\n#if _HAS_CXX20\n#include \"std20.h\"\n#endif\n",
        )
        .unwrap();
        std::fs::write(dir.join("inc/a.h"), "#define _HAS_CXX20 (_MSVC_LANG >= 202002L)\n").unwrap();
        for f in ["yes.h", "attr.h", "std20.h"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let argv = s(&[&cl.to_string_lossy(), "/nologo", "/std:c++20", "/EHsc", "/Iinc", "/c", "main.cc"]);
        let driver = Driver::new(Cache::new(vec![], true));
        let out = driver.scan(&argv, &dir).unwrap();
        let names: Vec<String> = out.files.iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["main.cc", "a.h", "yes.h", "attr.h", "std20.h"], "{:?}", out.inexact);
        assert!(out.exact(), "{:?} {:?}", out.inexact, out.unknown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profiles_round_trip() {
        let dir = std::env::temp_dir().join(format!("cpp_deps_profiles_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Profile::new(
            "#define __GNUC__ 13\n#define __cplusplus 201703L\n".into(),
            vec!["__has_include".into(), "__has_builtin".into()],
            crate::expr::Dialect { true_false: true, alt_operators: true },
            vec![Dir::Path(p("/usr/include")), Dir::Framework(p("/Library/Frameworks"))],
            vec![p("/usr/include/stdc-predef.h")],
        );
        p.answers.lock().unwrap().insert(("__has_builtin".into(), "__builtin_expect".into()), Some(1));
        p.answers.lock().unwrap().insert(("__has_builtin".into(), "(".into()), None);
        p.save(&dir, "k\u{1}1").unwrap();
        assert!(Profile::load(&dir, "k\u{1}2").is_none());
        let q = Profile::load(&dir, "k\u{1}1").unwrap();
        assert_eq!(q.predefines, p.predefines);
        assert_eq!(q.operators, p.operators);
        assert_eq!((q.dialect.true_false, q.dialect.alt_operators), (true, true));
        assert_eq!(q.builtin_dirs, p.builtin_dirs);
        assert_eq!(q.implicit, p.implicit);
        assert_eq!(*q.answers.lock().unwrap(), *p.answers.lock().unwrap());
        assert!(q.predefined.contains_key(&intern::intern("__GNUC__")));
        assert_eq!(q.probed, p.probed);
        // Probed over a day ago: probed again, used or not.
        let mut old = q;
        old.probed -= PROFILE_LIFETIME.as_secs() + 60;
        old.save(&dir, "k\u{1}1").unwrap();
        assert!(Profile::load(&dir, "k\u{1}1").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn environments() {
        let env = Environment { vars: Some(Arc::new(vec![("INCLUDE".into(), "C:/inc".into()), ("Path".into(), "x".into())])), profile_dir: None };
        assert_eq!(env.var("INCLUDE").as_deref(), Some("C:/inc"));
        assert_eq!(env.var("PATH").as_deref(), if cfg!(windows) { Some("x") } else { None });
        assert_eq!(env.var("CPATH"), None);
        let cwd = Path::new("C:/b");
        let inv = parse_in(&s(&["cl.exe", "/external:env:INCLUDE", "/c", "x.cc"]), cwd, &env).unwrap();
        assert_eq!(inv.system_dirs.len(), 1);
    }
}
