//! Scanning a compile command: its flags turned into a scan configuration,
//! the compiler probed once per compiler and set of flags that matter
//! (predefined macros, operators, dialect, built-in include directories),
//! and queries answered by probing the compiler, then scanning again.
//!
//! GCC and Clang command lines, and `cl`-style ones (`cl.exe`, `clang-cl`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::external::is_cl_style;
use crate::hmap::HeaderMap;
use crate::intern;
use crate::probe::{self, Style};
use crate::scan::{self, Cache, CommandMacro, Config, MacroTable, Oracle, Output, SearchDir};
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
    pub forced: Vec<PathBuf>,
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
        "c" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "cp" | "ixx" | "cppm" => "c++",
        _ => return None,
    })
}

/// Parses a compile command (response files expanded).
pub fn parse(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
    if is_cl_style(argv) { parse_cl(argv, cwd) } else { parse_gcc(argv, cwd) }
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

fn finish(mut inv: Invocation, lang: Option<String>) -> Result<Invocation, String> {
    if inv.source.as_os_str().is_empty() {
        return Err("no source file".into());
    }
    inv.lang = match lang {
        Some(l) => l,
        None => lang_of(&inv.source).ok_or_else(|| format!("unknown language of {}", inv.source.display()))?.to_string(),
    };
    if !matches!(inv.lang.as_str(), "c" | "c++") {
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
    ("-Xclang", true),
    ("-mllvm", true),
    ("-resource-dir", true),
    ("-stdlib", true),
];

fn parse_gcc(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
    let abs = |p: &str| -> PathBuf { scan::normalize(&cwd.join(p)) };
    let mut inv = new_invocation(argv, Style::Gcc)?;
    let mut lang: Option<String> = None;
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
        // Unsupported: what changes file lookup or macros beyond flags.
        for bad in ["-include-pch", "-iprefix", "-iwithprefix", "-iwithprefixbefore", "-I-", "--driver-mode=cl"] {
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
                "-include" | "-imacros" => inv.forced.push(abs(&v)),
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

fn parse_cl(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
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
            let dirs = std::env::var(var).unwrap_or_default();
            inv.system_dirs.extend(dirs.split(';').filter(|d| !d.is_empty()).map(|d| Dir::Path(abs(d))));
        } else if let Some(v) = value(f, "imsvc", &mut i)? {
            inv.system_dirs.push(Dir::Path(abs(&v)));
        } else if let Some(v) = value(f, "ivfsoverlay", &mut i)? {
            inv.overlays.push(abs(&v));
        } else if let Some(v) = f.strip_prefix("vfsoverlay:") {
            inv.overlays.push(abs(v));
        } else if let Some(v) = value(f, "FI", &mut i)? {
            inv.forced.push(abs(&v));
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
    overlays: Mutex<HashMap<PathBuf, Loaded<Overlay>>>,
    header_maps: Mutex<HashMap<PathBuf, Loaded<HeaderMap>>>,
}

impl Driver {
    pub fn new(cache: Cache) -> Self {
        Driver { cache, profiles: Mutex::default(), overlays: Mutex::default(), header_maps: Mutex::default() }
    }

    /// The profile of a compiler for an invocation's flags (probed once).
    pub fn profile(&self, inv: &Invocation, cwd: &Path) -> Result<Arc<Profile>, String> {
        let include_env = if inv.style == Style::Msvc && !inv.no_include_env { std::env::var("INCLUDE").unwrap_or_default() } else { String::new() };
        let key = format!(
            "{}\u{1}{:?}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            inv.compiler.display(),
            inv.style,
            inv.lang,
            cwd.display(),
            inv.conforming,
            include_env,
            inv.probe_args.join("\u{1}")
        );
        load_once(&self.profiles, key, || {
            let p = probe::probe(&inv.compiler, &inv.probe_args, inv.style, &inv.lang, cwd)?;
            let builtin_dirs = match inv.style {
                Style::Msvc => include_env.split(';').filter(|d| !d.is_empty()).map(|d| Dir::Path(scan::normalize(&cwd.join(d)))).collect(),
                _ => probe::builtin_dirs(&inv.compiler, &inv.probe_args, inv.style, &inv.lang, cwd)?
                    .into_iter()
                    .map(|(d, framework)| {
                        let d = scan::normalize(&cwd.join(d));
                        if framework { Dir::Framework(d) } else { Dir::Path(d) }
                    })
                    .collect(),
            };
            let mut predefined = scan::macro_table(&p.predefines);
            if inv.style == Style::Msvc {
                // The probe used the conforming preprocessor.
                let traditional = if inv.conforming { "0" } else { "1" };
                predefined.extend(scan::macro_table(&format!("#define _MSVC_TRADITIONAL {traditional}\n")));
            }
            Ok(Arc::new(Profile {
                predefined: Arc::new(predefined),
                operators: Arc::new(p.operators.iter().map(|o| intern::intern(o)).collect()),
                dialect: p.dialect,
                builtin_dirs,
                implicit: p.implicit.iter().map(|f| scan::normalize(&cwd.join(f))).collect(),
                answers: Mutex::default(),
                asking: Mutex::default(),
            }))
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
        let inv = parse(argv, cwd)?;
        let profile = self.profile(&inv, cwd)?;
        let dirs = |ds: &[Dir]| ds.iter().map(|d| self.search_dir(d, cwd)).collect::<Result<Vec<_>, _>>();
        let mut angled = dirs(&inv.user_dirs)?;
        angled.extend(dirs(&inv.system_dirs)?);
        angled.extend(dirs(&profile.builtin_dirs)?);
        angled.extend(dirs(&inv.after_dirs)?);
        let cfg = Config {
            dialect: profile.dialect,
            quote_dirs: dirs(&inv.quote_dirs)?,
            angled_dirs: angled,
            embed_dirs: inv.embed_dirs.clone(),
            overlays: inv.overlays.iter().map(|o| self.overlay(o, cwd)).collect::<Result<_, _>>()?,
            msvc_lookup: inv.style != Style::Gcc,
            predefined: profile.predefined.clone(),
            commands: inv.commands.clone(),
            forced: profile.implicit.iter().chain(&inv.forced).cloned().collect(),
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
            let values = probe::answer(&inv.compiler, &inv.probe_args, inv.style, &inv.lang, cwd, &unknown)?;
            let mut answers = profile.answers.lock().unwrap();
            for (q, v) in unknown.into_iter().zip(values) {
                answers.insert(q, v);
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
        assert_eq!(inv.forced, vec![p("/b/pre.h")]);
        assert_eq!(inv.probe_args, s(&["--target=x86_64-linux-gnu", "-O2", "-std=c++17"]));
        assert!(parse(&s(&["cc", "-include-pch", "x.pch", "-c", "a.c"]), cwd).is_err());
        assert!(parse(&s(&["cc", "-c", "a.f90"]), cwd).is_err());
        assert_eq!(parse(&s(&["cc", "-x", "c", "-c", "a.inc"]), cwd).unwrap().lang, "c");
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
        assert_eq!(inv.forced, vec![p("C:/b/pre.h")]);
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
}
