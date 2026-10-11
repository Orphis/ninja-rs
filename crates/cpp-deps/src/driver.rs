//! Scanning a GCC or Clang command line: its flags turned into a scan
//! configuration, the compiler probed once per compiler and set of flags
//! that matter (predefined macros, operators, built-in include
//! directories), and queries answered by probing the compiler, then
//! scanning again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::expr::Dialect;
use crate::intern;
use crate::probe;
use crate::scan::{self, Cache, CommandMacro, Config, MacroTable, Oracle, Output};

/// A compile command, as the scanner needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invocation {
    pub compiler: PathBuf,
    pub source: PathBuf,
    /// `c` or `c++`.
    pub lang: String,
    pub quote_dirs: Vec<PathBuf>,
    /// `-I` directories (searched before `-isystem`).
    pub user_dirs: Vec<PathBuf>,
    pub system_dirs: Vec<PathBuf>,
    pub after_dirs: Vec<PathBuf>,
    pub commands: Vec<CommandMacro>,
    pub forced: Vec<PathBuf>,
    /// The flags that may change predefined macros and built-in include
    /// directories (the probe's flags).
    pub probe_args: Vec<String>,
}

fn lang_of(source: &Path) -> Option<&'static str> {
    let ext = source.extension()?.to_string_lossy().to_ascii_lowercase();
    Some(match ext.as_str() {
        "c" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "cp" | "mm" => "c++",
        "m" => "objective-c",
        _ if source.extension()?.to_string_lossy() == "C" => "c++",
        _ => return None,
    })
}

/// Flags with a separate argument, and whether the probe keeps them.
const WITH_ARG: &[(&str, bool)] = &[
    ("-o", false),
    ("-MF", false),
    ("-MT", false),
    ("-MQ", false),
    ("-I", false),
    ("-iquote", false),
    ("-isystem", false),
    ("-idirafter", false),
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

/// Parses a GCC or Clang compile command (response files expanded).
pub fn parse(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
    let abs = |p: &str| -> PathBuf { scan::normalize(&cwd.join(p)) };
    let mut inv = Invocation { compiler: PathBuf::from(argv.first().ok_or("empty command")?), ..Default::default() };
    let mut lang: Option<String> = None;
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        // Unsupported: what changes file lookup or macros beyond flags.
        for bad in ["-ivfsoverlay", "-include-pch", "-F", "-iframework", "-iprefix", "-iwithprefix", "-iwithprefixbefore", "-I-", "--driver-mode=cl"] {
            if a == bad || (a.starts_with(bad) && bad.len() > 2 && !bad.ends_with('=')) {
                return Err(format!("unsupported flag {a}"));
            }
        }
        if a.starts_with("-F") && a.len() > 2 {
            return Err(format!("unsupported flag {a}"));
        }
        if let Some(&(flag, keep)) = WITH_ARG.iter().find(|(f, _)| a == f) {
            let v = argv.get(i).ok_or_else(|| format!("{flag} needs an argument"))?.clone();
            i += 1;
            match flag {
                "-I" => inv.user_dirs.push(abs(&v)),
                "-iquote" => inv.quote_dirs.push(abs(&v)),
                "-isystem" => inv.system_dirs.push(abs(&v)),
                "-idirafter" => inv.after_dirs.push(abs(&v)),
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
        if let Some(v) = joined("-I") {
            inv.user_dirs.push(abs(v));
        } else if let Some(v) = joined("-iquote") {
            inv.quote_dirs.push(abs(v));
        } else if let Some(v) = joined("-isystem") {
            inv.system_dirs.push(abs(v));
        } else if let Some(v) = joined("-idirafter") {
            inv.after_dirs.push(abs(v));
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

/// What a compiler predefines and searches, for a set of flags.
pub struct Profile {
    pub predefined: Arc<MacroTable>,
    pub operators: Arc<std::collections::HashSet<intern::Sym>>,
    pub builtin_dirs: Vec<PathBuf>,
    /// Files every compile reads first (see `probe::Probe::implicit`).
    pub implicit: Vec<PathBuf>,
    /// Query answers learned so far.
    answers: Mutex<HashMap<(String, String), Option<i64>>>,
}

struct ProfileOracle<'a>(&'a Profile);

impl Oracle for ProfileOracle<'_> {
    fn query(&self, op: &str, arg: &str) -> Option<i64> {
        self.0.answers.lock().unwrap().get(&(op.to_string(), arg.to_string())).copied().flatten()
    }
}

/// The built-in include directories, from `-E -v`.
fn builtin_dirs(compiler: &Path, args: &[String], lang: &str, cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let out = std::process::Command::new(compiler)
        .args(args)
        .args(["-x", lang, "-E", "-v", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }, "-"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("running {}: {e}", compiler.display()))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let mut dirs = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("#include <...> search starts here:") {
            inside = true;
        } else if line.starts_with("End of search list.") {
            break;
        } else if inside {
            let d = line.trim().trim_end_matches(" (framework directory)");
            dirs.push(scan::normalize(&cwd.join(d)));
        }
    }
    if !inside {
        return Err(format!("{} -v printed no search list", compiler.display()));
    }
    Ok(dirs)
}

/// A profile, or why the compiler couldn't be probed (not asked again).
type Probed = Arc<Result<Arc<Profile>, String>>;

/// Scans compile commands, with everything learned about compilers cached.
pub struct Driver {
    pub cache: Cache,
    profiles: Mutex<HashMap<String, Probed>>,
}

impl Driver {
    pub fn new(cache: Cache) -> Self {
        Driver { cache, profiles: Mutex::default() }
    }

    /// The profile of a compiler for an invocation's flags (probed once).
    pub fn profile(&self, inv: &Invocation, cwd: &Path) -> Result<Arc<Profile>, String> {
        let key = format!("{}\u{1}{}\u{1}{}\u{1}{}", inv.compiler.display(), inv.lang, cwd.display(), inv.probe_args.join("\u{1}"));
        if let Some(p) = self.profiles.lock().unwrap().get(&key) {
            return (**p).clone();
        }
        let made = (|| {
            let p = probe::probe(&inv.compiler, &inv.probe_args, &inv.lang, cwd)?;
            let dirs = builtin_dirs(&inv.compiler, &inv.probe_args, &inv.lang, cwd)?;
            Ok(Arc::new(Profile {
                predefined: Arc::new(scan::macro_table(&p.predefines)),
                operators: Arc::new(p.operators.iter().map(|o| intern::intern(o)).collect()),
                builtin_dirs: dirs,
                implicit: p.implicit.iter().map(|f| scan::normalize(&cwd.join(f))).collect(),
                answers: Mutex::default(),
            }))
        })();
        self.profiles.lock().unwrap().insert(key, Arc::new(made.clone()));
        made
    }

    /// Scans a compile command run in `cwd`. Unknown queries are asked to
    /// the compiler, then the scan runs again.
    pub fn scan(&self, argv: &[String], cwd: &Path) -> Result<Output, String> {
        let inv = parse(argv, cwd)?;
        let profile = self.profile(&inv, cwd)?;
        let cxx = inv.lang == "c++";
        let cfg = Config {
            dialect: if cxx { Dialect::CXX } else { Dialect::C },
            quote_dirs: inv.quote_dirs.clone(),
            angled_dirs: inv.user_dirs.iter().chain(&inv.system_dirs).chain(&profile.builtin_dirs).chain(&inv.after_dirs).cloned().collect(),
            msvc_lookup: false,
            predefined: profile.predefined.clone(),
            commands: inv.commands.clone(),
            forced: profile.implicit.iter().chain(&inv.forced).cloned().collect(),
            operators: profile.operators.clone(),
        };
        for _ in 0..3 {
            let out = scan::scan(&self.cache, &cfg, &ProfileOracle(&profile), &inv.source);
            let unknown: Vec<(String, String)> = {
                let answers = profile.answers.lock().unwrap();
                out.unknown.iter().filter(|q| !answers.contains_key(q)).cloned().collect()
            };
            if unknown.is_empty() {
                return Ok(out);
            }
            let values = probe::answer(&inv.compiler, &inv.probe_args, &inv.lang, cwd, &unknown)?;
            let mut answers = profile.answers.lock().unwrap();
            for (q, v) in unknown.into_iter().zip(values) {
                answers.insert(q, v);
            }
        }
        Ok(scan::scan(&self.cache, &cfg, &ProfileOracle(&profile), &inv.source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
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
                "-isystem",
                "/sys",
                "-iquote",
                "q",
                "-DA=1",
                "-D",
                "B",
                "-UC",
                "-include",
                "pre.h",
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
        assert_eq!(inv.source, PathBuf::from("/src/x.cpp"));
        assert_eq!(inv.lang, "c++");
        assert_eq!(inv.user_dirs, vec![PathBuf::from("/b/inc"), PathBuf::from("/abs")]);
        assert_eq!(inv.system_dirs, vec![PathBuf::from("/sys")]);
        assert_eq!(inv.quote_dirs, vec![PathBuf::from("/b/q")]);
        assert_eq!(inv.commands, vec![CommandMacro::Define("A=1".into()), CommandMacro::Define("B".into()), CommandMacro::Undef("C".into())]);
        assert_eq!(inv.forced, vec![PathBuf::from("/b/pre.h")]);
        assert_eq!(inv.probe_args, s(&["--target=x86_64-linux-gnu", "-O2", "-std=c++17"]));
        assert!(parse(&s(&["cc", "-include-pch", "x.pch", "-c", "a.c"]), cwd).is_err());
        assert!(parse(&s(&["cc", "-c", "a.f90"]), cwd).is_err());
        assert_eq!(parse(&s(&["cc", "-x", "c", "-c", "a.inc"]), cwd).unwrap().lang, "c");
    }
}
