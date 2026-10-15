//! Scanning a compile command: its flags turned into a scan configuration,
//! the compiler probed once per compiler and set of flags that matter
//! (predefined macros, operators, dialect, trigraphs, built-in include
//! directories), and queries answered by probing the compiler, then
//! scanning again.
//!
//! GCC and Clang command lines, and `cl`-style ones (`cl.exe`, `clang-cl`).
//! Every flag is either taken by the scan (directories, macros, forced
//! includes, the language), given to the probe (what may change what the
//! compiler predefines, searches or answers), dropped (outputs, diagnostics,
//! per-object settings), or unsupported: a flag changing preprocessing
//! otherwise, or one no list names, makes the scan fail, and another
//! scanner runs.
//!
//! Profiles can be saved in a directory (`Environment::profile_dir`), so
//! that later processes don't probe the compiler again: a file per profile,
//! with its whole key (the compiler's path, size and modification time, the
//! flags and the variables that matter) checked when loading.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::external::{is_cl_style, split_windows};
use crate::hmap::HeaderMap;
use crate::intern;
use crate::probe::{self, Style};
use crate::scan::{self, Cache, CommandMacro, Config, Family, Forced, MacroTable, Oracle, Output, SearchDir};
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
    /// `-imacros`: processed before every `-include`.
    pub imacros: Vec<Forced>,
    /// `-include`, `/FI`.
    pub forced: Vec<Forced>,
    /// `cl.exe`: its built-in directories are `INCLUDE`'s, unless `/X`.
    pub no_include_env: bool,
    /// `cl.exe`: the conforming preprocessor (`/Zc:preprocessor`), whose
    /// `_MSVC_TRADITIONAL` is 0.
    pub conforming: bool,
    /// Clang's MSVC compatibility (its lookups), when flags set it
    /// (`-f[no-]ms-compatibility`, `-fno-ms-extensions`): the scan takes it
    /// from the predefined macros, which must agree.
    pub ms_compat: Option<bool>,
    /// The flags that may change predefined macros, built-in include
    /// directories and the answers to queries (the probe's flags).
    pub probe_args: Vec<String>,
}

/// Whether a GCC or Clang driver compiles C sources as C++ (`g++`,
/// `clang++`, `c++`, versioned or prefixed with a target).
fn cxx_driver(program: &str) -> bool {
    let name = Path::new(&program.replace('\\', "/")).file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-').ends_with("++")
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
        imacros: Vec::new(),
        forced: Vec::new(),
        no_include_env: false,
        conforming: false,
        ms_compat: None,
        probe_args: Vec::new(),
    })
}

/// How a compiler merges its include chains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Merge {
    /// GCC's `merge_include_chains`: a directory given twice is searched at
    /// its first place only; an `-I` or `-iquote` one that is also a
    /// system directory (`-isystem`, built in, `-idirafter`) only at its
    /// place among those; the last `-iquote` one not if it's the first
    /// `-I` one.
    Gcc,
    /// Clang's (`clang-cl`'s too): the same, except that `-iquote`
    /// directories are only checked against each other.
    Clang,
    /// cl's: an `/I` directory that is also an `/external:I` one is
    /// searched there only (`INCLUDE`'s don't count).
    Msvc,
}

/// A directory's identity: its kind, and its real path (compilers compare
/// directories, not their spellings).
type DirId = (bool, PathBuf);

/// The `-iquote`, `-I` and system lists as the compiler searches them (see
/// `Merge`); `id` tells which directories are the same.
fn search_lists(merge: Merge, quote: &[Dir], user: &[Dir], system: [&[Dir]; 3], id: impl Fn(&Dir) -> DirId) -> (Vec<Dir>, Vec<Dir>, Vec<Dir>) {
    let ids = |dirs: &[Dir]| dirs.iter().map(|d| (d.clone(), id(d))).collect::<Vec<_>>();
    // First places only, without the directories of `skip`.
    let dedup = |dirs: Vec<(Dir, DirId)>, skip: &HashSet<DirId>| {
        let mut seen = HashSet::new();
        dirs.into_iter().filter(|(_, i)| !skip.contains(i) && seen.insert(i.clone())).collect::<Vec<_>>()
    };
    let all_system: Vec<(Dir, DirId)> = system.iter().flat_map(|s| ids(s)).collect();
    let none = HashSet::new();
    let (system, sys_ids): (Vec<(Dir, DirId)>, HashSet<DirId>) = match merge {
        Merge::Msvc => (all_system, ids(system[0]).into_iter().map(|(_, i)| i).collect()),
        Merge::Gcc | Merge::Clang => {
            let system = dedup(all_system, &none);
            let sys_ids = system.iter().map(|(_, i)| i.clone()).collect();
            (system, sys_ids)
        }
    };
    let user = dedup(ids(user), &sys_ids);
    let mut quote = dedup(ids(quote), if merge == Merge::Gcc { &sys_ids } else { &none });
    if merge == Merge::Gcc
        && let (Some((_, last)), Some((_, first))) = (quote.last(), user.first())
        && last == first
    {
        quote.pop();
    }
    let dirs = |v: Vec<(Dir, DirId)>| v.into_iter().map(|(d, _)| d).collect();
    (dirs(quote), dirs(user), dirs(system))
}

/// Finishes an invocation: its language from `lang` (`-x`, `/TP`...), else
/// from the source's extension (C++ for C sources when `cxx`).
fn finish(mut inv: Invocation, lang: Option<String>, cxx: bool) -> Result<Invocation, String> {
    if inv.source.as_os_str().is_empty() {
        return Err("no source file".into());
    }
    inv.lang = match lang.as_deref() {
        // A header (a precompiled one's): preprocessed as its language.
        Some("c-header") => "c".into(),
        Some("c++-header") => "c++".into(),
        Some(l) => l.to_string(),
        None => match lang_of(&inv.source).ok_or_else(|| format!("unknown language of {}", inv.source.display()))? {
            "c" if cxx => "c++".into(),
            l => l.to_string(),
        },
    };
    if !matches!(inv.lang.as_str(), "c" | "c++" | "assembler-with-cpp") {
        return Err(format!("unsupported language {}", inv.lang));
    }
    Ok(inv)
}

/// What a flag does to a scan, besides what the parsers take from it (the
/// directories, macros, forced includes, language and source).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    /// Outputs, dependency files, diagnostics, debug information, linking,
    /// per-object settings: nothing the preprocessor does depends on it, so
    /// neither the probe nor the profile key has it.
    Drop,
    /// May change the predefined macros, the built-in directories or the
    /// answers to queries: the probe runs with it.
    Probe,
    /// Changes preprocessing in ways the scan doesn't model (lexing,
    /// lookups, modules): another scanner is used.
    Unsupported,
}

/// GCC and Clang flags with a separate argument that the parser takes.
const SCAN_WITH_ARG: &[&str] =
    &["-I", "-F", "-iquote", "-isystem", "-iframework", "-idirafter", "-ivfsoverlay", "--embed-dir", "-D", "-U", "-include", "-imacros", "-x"];

/// The other GCC and Clang flags with a separate argument.
const WITH_ARG: &[(&str, Effect)] = &[
    ("-o", Effect::Drop),
    ("-MF", Effect::Drop),
    ("-MT", Effect::Drop),
    ("-MQ", Effect::Drop),
    ("-MJ", Effect::Drop),
    ("-dependency-file", Effect::Drop),
    ("--serialize-diagnostics", Effect::Drop),
    ("--param", Effect::Drop),
    ("-Xlinker", Effect::Drop),
    ("-Xassembler", Effect::Drop),
    ("-Xanalyzer", Effect::Drop),
    ("-L", Effect::Drop),
    ("-l", Effect::Drop),
    ("-T", Effect::Drop),
    ("-u", Effect::Drop),
    ("-z", Effect::Drop),
    ("-framework", Effect::Drop),
    ("-target", Effect::Probe),
    ("--target", Effect::Probe),
    ("-isysroot", Effect::Probe),
    ("--sysroot", Effect::Probe),
    ("-arch", Effect::Probe),
    ("-mllvm", Effect::Probe),
    ("-resource-dir", Effect::Probe),
    ("-stdlib", Effect::Probe),
    ("-stdlib++-isystem", Effect::Probe),
    ("-gcc-toolchain", Effect::Probe),
    ("--gcc-toolchain", Effect::Probe),
    ("--config", Effect::Probe),
    ("-B", Effect::Probe),
    ("-imultilib", Effect::Probe),
    ("-imultiarch", Effect::Probe),
    ("-Xpreprocessor", Effect::Unsupported),
    ("-iprefix", Effect::Unsupported),
    ("-iwithprefix", Effect::Unsupported),
    ("-iwithprefixbefore", Effect::Unsupported),
    ("-iwithsysroot", Effect::Unsupported),
    ("-isystem-after", Effect::Unsupported),
    ("-cxx-isystem", Effect::Unsupported),
    ("-working-directory", Effect::Unsupported),
];

/// GCC and Clang flags (without a separate argument) the scan doesn't
/// model: lexing (`-traditional-cpp`, charsets, `$`), lookups (`-I-`,
/// `-iprefix`...), modules and header units, offloading (several
/// compiles), assertions (`-A`).
const UNSUPPORTED: &[&str] = &[
    "-traditional",
    "-traditional-cpp",
    "-fpreprocessed",
    "-fdirectives-only",
    "-fno-dollars-in-identifiers",
    "-fmodules",
    "-fcxx-modules",
    "-fmodules-ts",
    "-fimplicit-module-maps",
    "-fbuiltin-module-map",
    "-fmodule-header",
    "-fsycl",
    "-I-",
];
const UNSUPPORTED_PREFIXES: &[&str] = &[
    "-fmodule-map-file=",
    "-fmodule-header=",
    "-fmodule-mapper=",
    "-fopenmp-targets=",
    "-fsycl-",
    "-foffload",
    "--offload",
    "--cuda-",
    "-Xopenmp-target",
    "-Xarch_",
    "-Xcuda-",
    "-A",
    "-iprefix",
    "-iwithprefix",
    "-iwithsysroot",
    "-isystem-after",
    "-cxx-isystem",
    "-working-directory",
    "--driver-mode=",
];

/// Flags that change nothing the preprocessor does.
const DROPPED: &[&str] = &[
    "-c",
    "-S",
    "-E",
    "-P",
    "-C",
    "-CC",
    "-H",
    "-v",
    "--verbose",
    "-###",
    "-pipe",
    "-w",
    "-pedantic",
    "-pedantic-errors",
    "--pedantic",
    "--pedantic-errors",
    "-Qunused-arguments",
    "-Qn",
    "-Qy",
    "-s",
    "-shared",
    "-shared-libgcc",
    "-static",
    "-static-libgcc",
    "-static-libstdc++",
    "-static-pie",
    "-rdynamic",
    "-pie",
    "-no-pie",
    "-nostdlib",
    "-nostdlib++",
    "-nodefaultlibs",
    "-nostartfiles",
    "-nolibc",
    "-time",
    "-emit-llvm",
    "-integrated-as",
    "-no-integrated-as",
];
const DROPPED_PREFIXES: &[&str] = &[
    // Outputs and dependency files.
    "-o",
    "-M",
    "-save-temps",
    "--save-temps",
    "-print-",
    "--print-",
    "-dump",
    "-d",
    "-frandom-seed=",
    "-ftime-trace",
    "-ftime-report",
    "-fdump-",
    "-fsave-optimization-record",
    "-foptimization-record-",
    "-fstack-usage",
    "-fcallgraph-info",
    "-fdeps-",
    "-fmodule-output",
    "-fmodules-cache-path=",
    "-fcrash-diagnostics",
    "-fno-crash-diagnostics",
    "-fproc-stat-report",
    "-fsyntax-only",
    // Diagnostics.
    "-W",
    "-fcolor-diagnostics",
    "-fno-color-diagnostics",
    "-fdiagnostics-",
    "-fno-diagnostics-",
    "-fmessage-length=",
    "-fansi-escape-codes",
    "-fcaret-diagnostics",
    "-fno-caret-diagnostics",
    "-fshow-",
    "-fno-show-",
    "-ferror-limit=",
    "-fmax-errors=",
    "-ftemplate-backtrace-limit=",
    "-fmacro-backtrace-limit=",
    "-fconstexpr-backtrace-limit=",
    "-fspell-checking",
    "-fno-spell-checking",
    "-felide-type",
    "-fno-elide-type",
    "-fopt-info",
    "-fverbose-asm",
    "-fno-verbose-asm",
    "-ftrack-macro-expansion",
    "-ftabstop=",
    // Debug information and paths in outputs.
    "-g",
    "-fdebug-",
    "-fno-debug-",
    "-ffile-prefix-map=",
    "-fmacro-prefix-map=",
    "-ffile-compilation-dir=",
    "-fcoverage-compilation-dir=",
    "-fcoverage-prefix-map=",
    "-fworking-directory",
    "-fno-working-directory",
    // Files the compile reads besides its sources, and precompiled
    // headers (which stand for headers the scan reads).
    "-fprofile-use=",
    "-fprofile-instr-use=",
    "-fprofile-sample-use=",
    "-fprofile-generate=",
    "-fprofile-instr-generate=",
    "-fprofile-dir=",
    "-fprofile-list=",
    "-fprofile-remapping-file=",
    "-fprofile-exclude-files=",
    "-fprofile-filter-files=",
    "-fprofile-note=",
    "-fprofile-prefix-path=",
    "-fprofile-prefix-map=",
    "-fprofile-update=",
    "-fcs-profile-generate=",
    "-fsanitize-ignorelist=",
    "-fsanitize-blacklist=",
    "-fsanitize-system-ignorelist=",
    "-fsanitize-coverage-allowlist=",
    "-fsanitize-coverage-ignorelist=",
    "-fxray-always-instrument=",
    "-fxray-never-instrument=",
    "-fxray-attr-list=",
    "-fpch-",
    "-fno-pch-",
    // Linking and assembling.
    "-Wa,",
    "-Wl,",
    "-l",
    "-L",
    "-fuse-ld=",
    "-fuse-linker-plugin",
    "-fno-use-linker-plugin",
    "-rtlib=",
    "--rtlib=",
    "-unwindlib=",
    "--unwindlib=",
    "--ld-path=",
    "-fintegrated-as",
    "-fno-integrated-as",
    "-fintegrated-cc1",
    "-fno-integrated-cc1",
    // Limits.
    "-fbracket-depth=",
    "-ftemplate-depth=",
    "-fconstexpr-depth=",
    "-fconstexpr-steps=",
    "-fconstexpr-ops-limit=",
    "-fconstexpr-loop-limit=",
];

/// Flags that may change the predefined macros, the built-in directories or
/// the answers to queries: the language and target (`-std`, `-m...`,
/// `-target`, sysroots, toolchains), optimization, and code generation
/// (`-f...`, which no list above names).
const PROBED: &[&str] = &[
    "-ansi",
    "-pthread",
    "-pthreads",
    "-undef",
    "-trigraphs",
    "-nostdinc",
    "-nostdinc++",
    "-nostdlibinc",
    "-nobuiltininc",
    "-ibuiltininc",
    "--no-default-config",
    "-no-canonical-prefixes",
    "-p",
    "-pg",
    "--coverage",
    "-coverage",
    "--analyze",
    "-disable-O0-optnone",
    "-disable-llvm-passes",
    "-disable-llvm-optzns",
];
const PROBED_PREFIXES: &[&str] = &[
    "-std=",
    "--std=",
    "-O",
    "-m",
    "-f",
    "-specs=",
    "--specs=",
    "-B",
    "--target=",
    "--sysroot=",
    "--gcc-toolchain=",
    "--gcc-install-dir=",
    "--config=",
    "-stdlib=",
    "-isysroot",
    "-resource-dir=",
];

/// What a GCC or Clang flag (without a separate argument) does to a scan.
/// Flags no list names are unsupported.
fn gcc_effect(a: &str) -> Effect {
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| a.starts_with(p));
    if UNSUPPORTED.contains(&a) || starts(UNSUPPORTED_PREFIXES) {
        return Effect::Unsupported;
    }
    if let Some(charset) = a.strip_prefix("-finput-charset=") {
        // UTF-8 is the default: what the lexer reads.
        return if charset.eq_ignore_ascii_case("utf-8") || charset.eq_ignore_ascii_case("utf8") { Effect::Drop } else { Effect::Unsupported };
    }
    if let Some(module) = a.strip_prefix("-fmodule-file=") {
        // A named module's (`name=file`) exports no macros; a header
        // unit's does.
        return if module.contains('=') { Effect::Drop } else { Effect::Unsupported };
    }
    if DROPPED.contains(&a) || starts(DROPPED_PREFIXES) {
        return Effect::Drop;
    }
    if PROBED.contains(&a) || starts(PROBED_PREFIXES) {
        return Effect::Probe;
    }
    Effect::Unsupported
}

fn parse_gcc(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
    let abs = |p: &str| -> PathBuf { scan::normalize(&cwd.join(p)) };
    let forced = |p: &str| if Path::new(p).is_absolute() { Forced::Path(abs(p)) } else { Forced::Gcc { cwd: cwd.to_path_buf(), name: p.to_string() } };
    let unsupported = |a: &str| Err(format!("unsupported flag {a}"));
    let mut inv = new_invocation(argv, Style::Gcc)?;
    let mut cxx = cxx_driver(&argv[0]);
    // The `-x` in effect, and the one the source came after.
    let (mut x, mut lang): (Option<String>, Option<String>) = (None, None);
    // `-f[no-]ms-compatibility` and `-f[no-]ms-extensions`, the last ones.
    let (mut ms_compat, mut ms_ext): (Option<bool>, Option<bool>) = (None, None);
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
                lang = x.clone();
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
                ("-include", Some(f)) => {
                    inv.forced.push(forced(f));
                    i += 3;
                }
                ("-imacros", Some(f)) => {
                    inv.imacros.push(forced(f));
                    i += 3;
                }
                ("-include-pch", Some(_)) => {
                    include_pch = true;
                    i += 3;
                }
                ("-emit-pch" | "-fno-pch-timestamp" | "-fno-validate-pch", _) => i += 1,
                // cc1 takes them without the driver knowing.
                (v, _) if ms_compat_flag(v) => return unsupported(&format!("-Xclang {v}")),
                (v, _) => match gcc_effect(v) {
                    Effect::Drop => i += 1,
                    Effect::Probe => {
                        inv.probe_args.extend([a.clone(), v.to_string()]);
                        i += 1;
                    }
                    Effect::Unsupported => return unsupported(&format!("-Xclang {v}")),
                },
            }
            continue;
        }
        if a == "-include-pch" {
            include_pch = true;
            i += 1;
            continue;
        }
        if let Some(&flag) = SCAN_WITH_ARG.iter().find(|f| a == *f) {
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
                "-include" => inv.forced.push(forced(&v)),
                "-imacros" => inv.imacros.push(forced(&v)),
                _ => x = Some(v).filter(|v| v != "none"),
            }
            continue;
        }
        if let Some(&(flag, effect)) = WITH_ARG.iter().find(|(f, _)| a == f) {
            let v = argv.get(i).ok_or_else(|| format!("{flag} needs an argument"))?.clone();
            i += 1;
            match effect {
                Effect::Drop => {}
                Effect::Probe => inv.probe_args.extend([a.clone(), v]),
                Effect::Unsupported => return unsupported(a),
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
        } else if let Some(v) = joined("-include") {
            inv.forced.push(forced(v));
        } else if let Some(v) = joined("-imacros") {
            inv.imacros.push(forced(v));
        } else if let Some(v) = joined("-I").filter(|v| *v != "-") {
            inv.user_dirs.push(Dir::Path(abs(v)));
        } else if let Some(v) = joined("-F") {
            inv.user_dirs.push(Dir::Framework(abs(v)));
        } else if let Some(v) = joined("-D") {
            inv.commands.push(CommandMacro::Define(v.to_string()));
        } else if let Some(v) = joined("-U") {
            inv.commands.push(CommandMacro::Undef(v.to_string()));
        } else if let Some(v) = joined("-x") {
            x = Some(v.to_string()).filter(|v| v != "none");
        } else if let Some(mode) = a.strip_prefix("--driver-mode=").filter(|m| matches!(*m, "g++" | "gcc")) {
            cxx = mode == "g++";
        } else if let Some(opts) = a.strip_prefix("-Wp,") {
            // Options for the preprocessor itself: dependency files only
            // (Linux's `-Wp,-MMD,file`), which the probe mustn't write.
            let mut opts = opts.split(',');
            while let Some(o) = opts.next() {
                match o {
                    "-MD" | "-MMD" | "-MF" | "-MT" | "-MQ" => {
                        opts.next();
                    }
                    "-M" | "-MM" | "-MP" | "-MG" => {}
                    _ => return unsupported(a),
                }
            }
        } else if a.starts_with('-') {
            // Clang's MSVC compatibility changes lookups: the predefined
            // macros must show it (see `Invocation::ms_compat`).
            match a.as_str() {
                "-fms-compatibility" => ms_compat = Some(true),
                "-fno-ms-compatibility" => ms_compat = Some(false),
                "-fms-extensions" => ms_ext = Some(true),
                "-fno-ms-extensions" => ms_ext = Some(false),
                _ => {}
            }
            match gcc_effect(a) {
                Effect::Drop => {}
                Effect::Probe => inv.probe_args.push(a.clone()),
                Effect::Unsupported => return unsupported(a),
            }
        } else {
            if !inv.source.as_os_str().is_empty() {
                return Err(format!("more than one input ({} and {a})", inv.source.display()));
            }
            inv.source = abs(a);
            lang = x.clone();
        }
    }
    if include_pch && inv.forced.is_empty() {
        return Err("a precompiled header without the header it was made from".into());
    }
    // Without `-fms-compatibility`, `-fno-ms-extensions` turns it off.
    inv.ms_compat = ms_compat.or(ms_ext.filter(|e| !e));
    finish(inv, lang, cxx)
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
        "interface",
        "internalPartition",
        "ftime-trace",
        "frandom-seed=",
    ];
    // Warnings: /W0 to /W4, /Wall, /WX, /Wv:, /w, /w1 to /w4, clang-cl's
    // -Wfoo and -Wno-foo (not /winsdkdir and the like).
    let warning = f == "w"
        || f.starts_with("WX")
        || f.starts_with("Wv")
        || f == "Wall"
        || (f.len() == 2 && (f.starts_with('W') || f.starts_with('w')) && f.as_bytes()[1].is_ascii_digit())
        || (f.starts_with('W') && f.as_bytes().get(1).is_some_and(u8::is_ascii_lowercase));
    // Preprocessing to a file or stdout (which the probe mustn't).
    let preprocess = matches!(f, "P" | "E" | "EP" | "C");
    warning || preprocess || f == "c" || f == "showIncludes" || f == "nologo" || f == "Gy" || f == "Gw" || PREFIXES.iter().any(|p| f.starts_with(p))
}

/// cl's flags (without `/` or `-`) that change preprocessing in ways the
/// scan doesn't model: C++/CLI and C++/CX (`#using`), header units, other
/// source charsets, clang-cl without its MSVC compatibility.
fn cl_unsupported(f: &str) -> bool {
    const PREFIXES: &[&str] =
        &["clr", "ZW", "AI", "FU", "headerUnit", "headerName", "translateInclude", "exportHeader", "fno-ms-compatibility", "fno-ms-extensions"];
    let charset = f.strip_prefix("source-charset:").is_some_and(|c| !c.eq_ignore_ascii_case("utf-8"));
    charset || PREFIXES.iter().any(|p| f.starts_with(p))
}

/// cl's flags (without `/` or `-`) whose effects the parser takes from the
/// command line: directories, macros, forced includes, the language, the
/// preprocessor.
fn cl_parsed(f: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "I",
        "FI",
        "D",
        "U",
        "T",
        "external:I",
        "external:env:",
        "imsvc",
        "ivfsoverlay",
        "vfsoverlay:",
        "Zc:preprocessor",
        "experimental:preprocessor",
        "Xclang",
        "clang:",
    ];
    f == "X" || matches!(f, "std:c11" | "std:c17" | "std:clatest") || PREFIXES.iter().any(|p| f.starts_with(p))
}

/// Clang's flags turning its MSVC compatibility on or off.
fn ms_compat_flag(a: &str) -> bool {
    matches!(a, "-fms-compatibility" | "-fno-ms-compatibility" | "-fms-extensions" | "-fno-ms-extensions")
}

fn parse_cl(argv: &[String], cwd: &Path, env: &Environment) -> Result<Invocation, String> {
    let abs = |p: &str| -> PathBuf { scan::normalize(&cwd.join(p)) };
    let unsupported = |a: &str| Err(format!("unsupported flag {a}"));
    // cl and clang-cl take flags from `CL` before the command's, and from
    // `_CL_` after: the probe sees them, the scan doesn't.
    for var in ["CL", "_CL_"] {
        for a in env.var(var).map(|v| split_windows(&v)).unwrap_or_default() {
            if a.strip_prefix('/').or_else(|| a.strip_prefix('-')).is_none_or(|f| cl_parsed(f) || cl_unsupported(f)) {
                return Err(format!("{var} has {a}"));
            }
        }
    }
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
        if matches!(f, "sourceDependencies" | "sourceDependencies:directives" | "scanDependencies" | "reference" | "ifcOutput" | "ifcSearchDir") {
            // Outputs, and named modules (which export no macros), with
            // their argument.
            i += 1;
            continue;
        }
        if cl_dropped(f) {
            continue;
        }
        if cl_unsupported(f) {
            return unsupported(&a);
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
        } else if let Some(g) = f.strip_prefix("clang:") {
            // A GCC-style flag: those the parser takes can't come this way.
            if let Some(&(_, effect)) = WITH_ARG.iter().find(|(w, _)| *w == g) {
                // Its argument follows as `/clang:<argument>`.
                let v = argv.get(i).cloned().unwrap_or_default();
                i += 1;
                match effect {
                    Effect::Drop => {}
                    Effect::Probe => inv.probe_args.extend([a.clone(), v]),
                    Effect::Unsupported => return unsupported(&a),
                }
                continue;
            }
            match gcc_effect(g) {
                _ if ms_compat_flag(g) && g.starts_with("-fno-") => return unsupported(&a),
                Effect::Drop => {}
                Effect::Probe => inv.probe_args.push(a.clone()),
                Effect::Unsupported => return unsupported(&a),
            }
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
            if f == "Xclang" {
                match gcc_effect(&v) {
                    _ if ms_compat_flag(&v) => return unsupported(&format!("{a} {v}")),
                    Effect::Drop => continue,
                    Effect::Probe => {}
                    Effect::Unsupported => return unsupported(&format!("{a} {v}")),
                }
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
    let inv = finish(inv, lang, false)?;
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
    /// Trigraphs are replaced.
    pub trigraphs: bool,
    /// From the predefined macros: Clang (`__clang__`), cl (`_MSC_VER`
    /// alone), else GCC.
    pub family: Family,
    /// Clang compiling for the MSVC ABI (`_MSC_VER` too), whose MSVC
    /// compatibility searches includers' directories as cl does.
    pub ms_compat: bool,
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

const PROFILE_SIGNATURE: &str = "cpp-deps profile 3";

/// Profiles unused for this long are removed when another is saved (at
/// most once a day: see `CLEANED`).
const PROFILE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// The file of a profile directory whose modification time is the last
/// clean-up's.
const CLEANED: &str = "cleaned";
const CLEAN_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Profiles are probed again after this long, however often used: what the
/// key doesn't see (a clang configuration file's contents, an SDK installed
/// behind a shim) is then stale for a day at most.
const PROFILE_LIFETIME: Duration = Duration::from_secs(24 * 3600);

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Profile {
    fn new(
        predefines: String,
        operators: Vec<String>,
        dialect: crate::expr::Dialect,
        trigraphs: bool,
        builtin_dirs: Vec<Dir>,
        implicit: Vec<PathBuf>,
    ) -> Profile {
        let predefined = scan::macro_table(&predefines);
        let defined = |m: &str| predefined.contains_key(&intern::intern(m));
        let (clang, msc) = (defined("__clang__"), defined("_MSC_VER"));
        Profile {
            predefined: Arc::new(predefined),
            predefines,
            operators: Arc::new(operators.iter().map(|o| intern::intern(o)).collect()),
            dialect,
            trigraphs,
            family: if clang {
                Family::Clang
            } else if msc {
                Family::Msvc
            } else {
                Family::Gcc
            },
            ms_compat: clang && msc,
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
            "{PROFILE_SIGNATURE}\nkey\t{key}\nprobed\t{}\ndialect\t{}\t{}\ntrigraphs\t{}\n",
            self.probed, self.dialect.true_false as u8, self.dialect.alt_operators as u8, self.trigraphs as u8
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
        // Profiles of compilers gone or changed since, looked for once a
        // day (profiles are saved often: per new set of flags, and as
        // queries are answered).
        let now = SystemTime::now();
        let age = |t: SystemTime| now.duration_since(t).unwrap_or_default();
        let cleaned = dir.join(CLEANED);
        if std::fs::metadata(&cleaned).and_then(|m| m.modified()).is_ok_and(|t| age(t) < CLEAN_INTERVAL) {
            return Ok(());
        }
        std::fs::write(&cleaned, "")?;
        for e in std::fs::read_dir(dir)?.flatten() {
            if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| age(t) > PROFILE_MAX_AGE) {
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
        let (mut probed, mut dialect, mut trigraphs, mut operators, mut dirs, mut implicit, mut answers) =
            (None, None, None, Vec::new(), Vec::new(), Vec::new(), HashMap::new());
        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["probed", t] => probed = Some(t.parse::<u64>().ok()?),
                ["dialect", t, a] => dialect = Some(crate::expr::Dialect { true_false: *t == "1", alt_operators: *a == "1" }),
                ["trigraphs", t] => trigraphs = Some(*t == "1"),
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
        let mut profile = Profile::new(predefines.to_string(), operators, dialect?, trigraphs?, dirs, implicit);
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
    /// Search directories' real paths, and whether they're header maps, by
    /// path, taken once per session (see `real_dir`).
    real_dirs: RwLock<HashMap<PathBuf, (u64, PathBuf, bool)>>,
}

impl Driver {
    pub fn new(cache: Cache) -> Self {
        Driver {
            cache,
            profiles: Mutex::default(),
            fingerprints: Mutex::default(),
            overlays: Mutex::default(),
            header_maps: Mutex::default(),
            real_dirs: RwLock::default(),
        }
    }

    /// A search directory's real path (compilers tell directories apart by
    /// what they are, not how they're spelled) and whether it's a header
    /// map file, taken once per session; a path that doesn't exist is
    /// looked at again.
    fn real_dir(&self, p: &Path) -> (PathBuf, bool) {
        let session = self.cache.session();
        if let Some((s, real, header_map)) = self.real_dirs.read().unwrap().get(p)
            && *s == session
        {
            return (real.clone(), *header_map);
        }
        let (Ok(meta), Ok(real)) = (std::fs::metadata(p), std::fs::canonicalize(p)) else {
            return (p.to_path_buf(), false);
        };
        let header_map = meta.is_file() && HeaderMap::is_header_map(p);
        self.real_dirs.write().unwrap().insert(p.to_path_buf(), (session, real.clone(), header_map));
        (real, header_map)
    }

    fn dir_id(&self, d: &Dir) -> DirId {
        match d {
            Dir::Path(p) => (false, self.real_dir(p).0),
            Dir::Framework(p) => (true, self.real_dir(p).0),
        }
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
            let profile = Profile::new(predefines, p.operators, p.dialect, p.trigraphs, builtin_dirs, implicit);
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
            Dir::Path(p) if self.real_dir(p).1 => SearchDir::HeaderMap(load_once(&self.header_maps, p.clone(), || HeaderMap::load(p, cwd).map(Arc::new))?),
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
        if profile.family == Family::Clang && inv.ms_compat.is_some_and(|c| c != profile.ms_compat) {
            return Err("MSVC compatibility flags the predefined macros don't show".into());
        }
        let dirs = |ds: &[Dir]| ds.iter().map(|d| self.search_dir(d, cwd)).collect::<Result<Vec<_>, _>>();
        let merge = match (inv.style, profile.family) {
            (Style::Msvc, _) => Merge::Msvc,
            (Style::Gcc, Family::Gcc) => Merge::Gcc,
            _ => Merge::Clang,
        };
        let system = [&inv.system_dirs[..], &profile.builtin_dirs, &inv.after_dirs];
        let (quote, user, system) = search_lists(merge, &inv.quote_dirs, &inv.user_dirs, system, |d| self.dir_id(d));
        let mut angled = dirs(&user)?;
        angled.extend(dirs(&system)?);
        let cfg = Config {
            family: profile.family,
            dialect: profile.dialect,
            trigraphs: profile.trigraphs,
            quote_dirs: dirs(&quote)?,
            angled_dirs: angled,
            embed_dirs: inv.embed_dirs.clone(),
            overlays: inv.overlays.iter().map(|o| self.overlay(o, cwd)).collect::<Result<_, _>>()?,
            // Clang for the MSVC ABI searches as cl does.
            msvc_lookup: inv.style != Style::Gcc || profile.ms_compat,
            predefined: profile.predefined.clone(),
            commands: inv.commands.clone(),
            // GCC and Clang process every `-imacros` first, then what the
            // compiler reads first (`stdc-predef.h`), then `-include`s.
            forced: inv.imacros.iter().cloned().chain(profile.implicit.iter().cloned().map(Forced::Path)).chain(inv.forced.iter().cloned()).collect(),
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
    fn search_list_rules() {
        let d = |s: &str| Dir::Path(p(s));
        let id = |d: &Dir| match d {
            Dir::Path(p) => (false, p.clone()),
            Dir::Framework(p) => (true, p.clone()),
        };
        let quote = [d("/q"), d("/q"), d("/sys"), d("/u")];
        let user = [d("/u"), d("/builtin"), d("/u"), d("/v"), d("/after")];
        let system: [&[Dir]; 3] = [&[d("/sys"), d("/builtin")], &[d("/builtin"), d("/b2")], &[d("/after")]];
        // GCC: duplicates searched at their first place; user directories
        // that are system ones (`-I/usr/include`) only at the system place;
        // the last -iquote one not when it's the first -I one.
        let (q, u, sys) = search_lists(Merge::Gcc, &quote, &user, system, id);
        assert_eq!(q, vec![d("/q")]);
        assert_eq!(u, vec![d("/u"), d("/v")]);
        assert_eq!(sys, vec![d("/sys"), d("/builtin"), d("/b2"), d("/after")]);
        // Clang: -iquote directories only against each other.
        let (q, u, sys) = search_lists(Merge::Clang, &quote, &user, system, id);
        assert_eq!(q, vec![d("/q"), d("/sys"), d("/u")]);
        assert_eq!(u, vec![d("/u"), d("/v")]);
        assert_eq!(sys, vec![d("/sys"), d("/builtin"), d("/b2"), d("/after")]);
        // cl: /I directories that are /external:I ones, not INCLUDE's.
        let (_, u, sys) = search_lists(Merge::Msvc, &[], &[d("/ext"), d("/inc"), d("/v")], [&[d("/ext")], &[d("/inc"), d("/inc")], &[]], id);
        assert_eq!(u, vec![d("/inc"), d("/v")]);
        assert_eq!(sys, vec![d("/ext"), d("/inc"), d("/inc")]);
        // A framework directory isn't the directory of the same path.
        let (_, u, _) = search_lists(Merge::Gcc, &[], &[Dir::Framework(p("/f"))], [&[d("/f")], &[], &[]], id);
        assert_eq!(u, vec![Dir::Framework(p("/f"))]);
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
            true,
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
        assert!(q.trigraphs);
        assert_eq!((q.family, q.ms_compat), (Family::Gcc, false));
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
    fn profile_families() {
        let family = |predefines: &str| {
            let p = Profile::new(predefines.into(), vec![], crate::expr::Dialect::default(), false, vec![], vec![]);
            (p.family, p.ms_compat)
        };
        assert_eq!(family("#define __GNUC__ 13\n"), (Family::Gcc, false));
        assert_eq!(family("#define __GNUC__ 4\n#define __clang__ 1\n"), (Family::Clang, false));
        assert_eq!(family("#define __clang__ 1\n#define _MSC_VER 1944\n"), (Family::Clang, true));
        assert_eq!(family("#define _MSC_VER 1944\n"), (Family::Msvc, false));
    }

    #[test]
    fn profiles_cleaned_daily() {
        let dir = std::env::temp_dir().join(format!("cpp_deps_profiles_cleaned_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = Profile::new("#define __GNUC__ 13\n".into(), vec![], crate::expr::Dialect::default(), false, vec![], vec![]);
        let age = |f: &Path, days: u64| {
            let t = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
            std::fs::File::options().write(true).open(f).unwrap().set_modified(t).unwrap();
        };
        let stale = |name: &str| {
            let f = dir.join(name);
            std::fs::write(&f, "").unwrap();
            age(&f, 31);
            f
        };
        // The first save looks for profiles unused for a month.
        let a = stale("a.profile");
        p.save(&dir, "k").unwrap();
        assert!(!a.exists());
        // The next ones that day don't (saves list the directory once a day).
        let b = stale("b.profile");
        p.save(&dir, "k").unwrap();
        assert!(b.exists());
        // A day later they do.
        age(&dir.join(CLEANED), 2);
        p.save(&dir, "k").unwrap();
        assert!(!b.exists());
        assert!(Profile::load(&dir, "k").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn languages() {
        let cwd = Path::new("/b");
        let lang = |argv: &[&str]| parse(&s(argv), cwd).unwrap().lang;
        // C++ drivers compile C sources as C++.
        for cxx in ["clang++", "/usr/bin/g++-13", "x86_64-linux-gnu-g++", "c++", "C:\\LLVM\\bin\\clang++.exe", "clang++-18"] {
            assert_eq!(lang(&[cxx, "-c", "a.c"]), "c++", "{cxx}");
        }
        for c in ["clang", "gcc-13", "cc", "clang-18"] {
            assert_eq!(lang(&[c, "-c", "a.c"]), "c", "{c}");
        }
        assert_eq!(lang(&["clang", "--driver-mode=g++", "-c", "a.c"]), "c++");
        assert_eq!(lang(&["clang++", "--driver-mode=gcc", "-c", "a.c"]), "c");
        // -x applies to the inputs after it.
        assert_eq!(lang(&["clang++", "-x", "c", "-c", "a.c"]), "c");
        assert_eq!(lang(&["clang", "-c", "a.c", "-x", "c++"]), "c");
        assert_eq!(lang(&["clang", "-xc++", "-x", "none", "-c", "a.c"]), "c");
        assert_eq!(lang(&["gcc", "-x", "c++", "-c", "--", "a.c"]), "c++");
    }

    #[test]
    fn flag_policy() {
        let cwd = Path::new("/b");
        let parsed = |extra: &[&str]| {
            let mut argv = vec!["clang++"];
            argv.extend(extra);
            argv.extend(["-c", "a.cc"]);
            parse(&s(&argv), cwd)
        };
        let probe = |extra: &[&str]| parsed(extra).unwrap().probe_args;
        // Outputs, diagnostics and per-object flags don't reach the probe
        // (nor the profile's key).
        let dropped = [
            "-frandom-seed=a.o",
            "-Wp,-MD,a.d",
            "-Wp,-MMD,a.d",
            "-MJ",
            "a.json",
            "-MJa.json",
            "-ftime-trace=a.json",
            "-fmodule-file=m=m.pcm",
            "-fdiagnostics-color=always",
            "-gsplit-dwarf",
            "-Werror",
            "-pedantic",
            "-save-temps=obj",
            "-fprofile-use=a.profdata",
            "-finput-charset=UTF-8",
            "-Xclang",
            "-fcolor-diagnostics",
        ];
        assert_eq!(probe(&dropped), Vec::<String>::new());
        // What may change predefines, built-in directories or answers does.
        let probed = [
            "-O2",
            "-march=native",
            "-std=c++20",
            "-fno-exceptions",
            "-pthread",
            "--sysroot=/s",
            "-isysroot",
            "/sdk",
            "-nostdinc++",
            "-fms-compatibility-version=19.40",
        ];
        assert_eq!(probe(&probed), s(&probed));
        // What changes preprocessing otherwise falls back, as do flags no
        // list names.
        for bad in [
            &["-fmodules"][..],
            &["-fmodule-map-file=m.modulemap"],
            &["-fmodule-file=x.pcm"],
            &["-traditional-cpp"],
            &["-finput-charset=latin1"],
            &["-fno-dollars-in-identifiers"],
            &["-Wp,-DX"],
            &["-Xpreprocessor", "-DX"],
            &["-Xclang", "-I"],
            &["-Xclang", "-fms-compatibility"],
            &["-iwithprefix", "x"],
            &["-Apred=answer"],
            &["-fsycl"],
            &["--offload-arch=gfx90a"],
            &["-I-"],
            &["--unknown-flag"],
            &["-unknown"],
        ] {
            assert!(parsed(bad).is_err(), "{bad:?}");
        }
        // Clang's MSVC compatibility, as flags set it.
        let ms = |extra: &[&str]| parsed(extra).unwrap().ms_compat;
        assert_eq!(ms(&[]), None);
        assert_eq!(ms(&["-fms-extensions"]), None);
        assert_eq!(ms(&["-fms-compatibility"]), Some(true));
        assert_eq!(ms(&["-fno-ms-extensions"]), Some(false));
        assert_eq!(ms(&["-fno-ms-extensions", "-fms-compatibility"]), Some(true));
        // -imacros apart from -include (processed first).
        let inv = parsed(&["-include", "a.h", "-imacros", "b.h", "-includec.h", "-imacrosd.h"]).unwrap();
        let gcc = |n: &str| Forced::Gcc { cwd: cwd.to_path_buf(), name: n.into() };
        assert_eq!((inv.forced, inv.imacros), (vec![gcc("a.h"), gcc("c.h")], vec![gcc("b.h"), gcc("d.h")]));
    }

    #[test]
    fn cl_flag_policy() {
        let cwd = Path::new("C:/b");
        let env =
            |vars: &[(&str, &str)]| Environment { vars: Some(Arc::new(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())), profile_dir: None };
        let parsed = |argv: &[&str], env: &Environment| parse_in(&s(argv), cwd, env);
        // The probe sees CL's and _CL_'s flags, the scan doesn't: those it
        // would take from the command line fall back.
        assert!(parsed(&["cl.exe", "/c", "x.cc"], &env(&[("CL", "/MP /W4 /utf-8")])).is_ok());
        for (var, flags) in [("CL", "/Iinc"), ("_CL_", "/FIpre.h"), ("CL", "/DX=1"), ("CL", "-X"), ("_CL_", "/TP"), ("CL", "/clr"), ("CL", "extra.cc")] {
            assert!(parsed(&["cl.exe", "/c", "x.cc"], &env(&[(var, flags)])).is_err(), "{var}={flags}");
        }
        // C++/CLI, header units, other charsets, clang-cl without MSVC
        // compatibility, GCC-style flags the parser would take.
        for bad in [
            &["/clr"][..],
            &["/headerUnit", "a.h=a.ifc"],
            &["/translateInclude"],
            &["/source-charset:windows-1252"],
            &["-fno-ms-compatibility"],
            &["/clang:-Iinc"],
            &["/clang:-fmodules"],
            &["/clang:-fno-ms-extensions"],
            &["-Xclang", "-include"],
        ] {
            let mut argv = vec!["clang-cl"];
            argv.extend(bad);
            argv.extend(["/c", "x.cc"]);
            assert!(parsed(&argv, &env(&[])).is_err(), "{bad:?}");
        }
        // Outputs and per-object flags don't reach the probe.
        let argv = [
            "clang-cl",
            "/utf-8",
            "/source-charset:utf-8",
            "/clang:-MD",
            "/clang:-MF",
            "/clang:x.d",
            "/clang:-frandom-seed=x",
            "-ftime-trace=x.json",
            "/reference",
            "m=m.ifc",
            "/ifcOutput",
            "x.ifc",
            "/interface",
            "/P",
            "/clang:-O2",
            "/c",
            "x.cc",
        ];
        assert_eq!(parsed(&argv, &env(&[])).unwrap().probe_args, s(&["/utf-8", "/source-charset:utf-8", "/clang:-O2"]));
    }

    /// A driver scanning `argv` with a profile saved beforehand (no
    /// compiler runs), and its environment.
    fn saved_profile(argv: &[String], cwd: &Path, predefines: &str, builtin: Vec<Dir>) -> (Driver, Environment) {
        let driver = Driver::new(Cache::new(vec![], false));
        let env = Environment { vars: Some(Arc::new(vec![])), profile_dir: Some(cwd.join("profiles")) };
        let inv = parse_in(argv, cwd, &env).unwrap();
        let key = driver.profile_key(&inv, cwd, &env);
        let profile = Profile::new(predefines.into(), vec!["__has_include".into()], crate::expr::Dialect::default(), false, builtin, vec![]);
        profile.save(&cwd.join("profiles"), &key).unwrap();
        (driver, env)
    }

    #[test]
    fn profiles_decide_lookups() {
        let dir = std::env::temp_dir().join(format!("cpp_deps_driver_lookups_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = [
            ("src/main.cc", "#include <sub/a.h>\n#include \"x.h\"\n"),
            ("inc/sub/a.h", "#include \"b.h\"\n"),
            ("src/b.h", ""),
            ("other/b.h", ""),
            ("sys/x.h", ""),
            ("inc/x.h", ""),
            ("macros.h", "#define FROM_MACROS 1\n"),
            ("forced.h", "#ifdef FROM_MACROS\n#include \"yes.h\"\n#endif\n"),
            ("yes.h", ""),
        ];
        for (f, text) in files {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), text).unwrap();
        }
        let compiler = if cfg!(windows) { "C:/no/such/clang++.exe" } else { "/no/such/clang++" };
        let scanned = |flags: &[&str], predefines: &str| {
            let argv: Vec<String> = std::iter::once(compiler).chain(flags.iter().copied()).chain(["-c", "src/main.cc"]).map(String::from).collect();
            let (driver, env) = saved_profile(&argv, &dir, predefines, vec![Dir::Path(dir.join("sys"))]);
            let out = driver.scan_in(&argv, &dir, &env)?;
            assert!(out.exact(), "{:?}", out.inexact);
            Ok::<_, String>(out.files.iter().map(|f| f.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>())
        };
        let (gcc, clang, clang_msvc) = ("#define __GNUC__ 13\n", "#define __clang__ 1\n", "#define __clang__ 1\n#define _MSC_VER 1944\n");
        // Clang for the MSVC ABI searches the includers' directories.
        let flags = ["-Iinc", "-Iother"];
        assert_eq!(scanned(&flags, clang_msvc).unwrap(), vec!["src/main.cc", "inc/sub/a.h", "src/b.h", "inc/x.h"]);
        assert_eq!(scanned(&flags, gcc).unwrap(), vec!["src/main.cc", "inc/sub/a.h", "other/b.h", "inc/x.h"]);
        // Flags turning MSVC compatibility on or off must agree.
        assert!(scanned(&["-Iinc", "-fno-ms-compatibility"], clang_msvc).is_err());
        assert!(scanned(&["-Iinc", "-Iother", "-fms-compatibility"], clang).is_err());
        assert!(scanned(&["-Iinc", "-Iother", "-fms-extensions"], gcc).is_ok());
        // Clang keeps an -iquote directory that's also a system one, GCC
        // doesn't.
        let flags = ["-iquote", "sys", "-Iinc", "-Iother"];
        assert_eq!(scanned(&flags, clang).unwrap()[3], "sys/x.h");
        assert_eq!(scanned(&flags, gcc).unwrap()[3], "inc/x.h");
        // Every -imacros before any -include.
        let files = scanned(&["-Iinc", "-Iother", "-include", "forced.h", "-imacros", "macros.h"], gcc).unwrap();
        assert_eq!(files[..3], ["macros.h", "forced.h", "yes.h"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_dirs_once_per_session() {
        let dir = std::env::temp_dir().join(format!("cpp_deps_real_dirs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        let driver = Driver::new(Cache::new(vec![], false));
        let real_a = std::fs::canonicalize(&a).unwrap();
        assert_eq!(driver.real_dir(&a), (real_a.clone(), false));
        // Taken once per session (scans don't canonicalize every pair).
        std::fs::remove_dir(&a).unwrap();
        assert_eq!(driver.real_dir(&a), (real_a, false));
        driver.cache.new_session();
        assert_eq!(driver.real_dir(&a), (a, false));
        // A directory that doesn't exist is looked at again.
        assert_eq!(driver.real_dir(&b), (b.clone(), false));
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(driver.real_dir(&b), (std::fs::canonicalize(&b).unwrap(), false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A differential case: its files, and the arguments after the
    /// compiler (`{d}` is its directory).
    type Case = (&'static str, &'static [(&'static str, &'static str)], &'static [&'static str]);

    /// Scans each case and checks it against the compiler's own list of the
    /// files it reads: a scan that says it's exact names the same files
    /// (but those `__has_include` found, which GCC doesn't list). Returns
    /// the cases scanned exactly.
    fn differential(compiler: &str, cases: &[Case]) -> Vec<&'static str> {
        use std::collections::BTreeSet;
        let root = std::env::temp_dir().join(format!("cpp_deps_differential_{}_{}", cases[0].0, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let driver = Driver::new(Cache::new(vec![], cfg!(windows)));
        let key = |p: &PathBuf| {
            let s = scan::normalize(p).to_string_lossy().into_owned();
            if cfg!(windows) { s.replace('\\', "/").to_lowercase() } else { s }
        };
        let mut exact = Vec::new();
        for &(name, files, args) in cases {
            let dir = root.join(name);
            for (f, text) in files {
                std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
                std::fs::write(dir.join(f), text).unwrap();
            }
            let argv: Vec<String> = std::iter::once(compiler.to_string()).chain(args.iter().map(|a| a.replace("{d}", &dir.to_string_lossy()))).collect();
            let theirs: BTreeSet<String> =
                crate::external::compiler_deps(&argv, &dir, None).unwrap_or_else(|e| panic!("{compiler}, {name}: {e}")).iter().map(key).collect();
            // Unsupported (another scanner runs), or inexact.
            let Ok(out) = driver.scan(&argv, &dir) else { continue };
            if !(out.exact() && out.missing.is_empty()) {
                continue;
            }
            let ours: BTreeSet<String> = out.files.iter().map(key).collect();
            let found: BTreeSet<String> = out.found.iter().map(key).collect();
            assert!(
                ours.is_subset(&theirs) && theirs.iter().all(|f| ours.contains(f) || found.contains(f)),
                "{compiler}, {name}: scanned {ours:?}, the compiler read {theirs:?}"
            );
            exact.push(name);
        }
        let _ = std::fs::remove_dir_all(&root);
        exact
    }

    /// GCC-style differential cases (the second review's, and more).
    const GCC_CASES: &[Case] = &[
        ("plain", &[("main.c", "#include \"a.h\"\n"), ("a.h", "")], &["-c", "main.c"]),
        ("cr_only_line_endings", &[("main.c", "#pragma once\r#include \"a.h\"\r#include \"b.h\"\rint x;\r"), ("a.h", ""), ("b.h", "")], &["-c", "main.c"]),
        ("cr_only_text_first", &[("main.c", "int y;\r#include \"a.h\"\r"), ("a.h", "")], &["-c", "main.c"]),
        (
            "include_next_from_iquote",
            &[("main.c", "#include \"x.h\"\n"), ("q1/x.h", "#include_next <x.h>\n"), ("q2/x.h", ""), ("inc/x.h", "")],
            &["-iquote", "q1", "-iquote", "q2", "-I", "inc", "-c", "main.c"],
        ),
        (
            "include_next_from_includer_dir",
            &[("main.c", "#include \"y.h\"\n"), ("y.h", "#include_next <y.h>\n"), ("q/y.h", ""), ("inc/y.h", "")],
            &["-iquote", "q", "-I", "inc", "-c", "main.c"],
        ),
        (
            "counter_in_if",
            &[
                ("main.c", "int a = __COUNTER__;\n#if __COUNTER__ == 1\n#include \"one.h\"\n#else\n#include \"zero.h\"\n#endif\n"),
                ("one.h", ""),
                ("zero.h", ""),
            ],
            &["-c", "main.c"],
        ),
        (
            "define_value_backslash",
            &[("main.c", "#ifdef Y\n#include \"y.h\"\n#else\n#include \"n.h\"\n#endif\n"), ("y.h", ""), ("n.h", "")],
            &["-DX=a\\", "-DY", "-c", "main.c"],
        ),
        (
            "pragma_push_pop_from_macro",
            &[
                (
                    "main.c",
                    "#define PUSH _Pragma(\"push_macro(\\\"M\\\")\")\n#define POP _Pragma(\"pop_macro(\\\"M\\\")\")\n#define M 1\nPUSH\n#undef M\n#define M 2\nPOP\n\
                     #if M == 1\n#include \"one.h\"\n#else\n#include \"two.h\"\n#endif\n",
                ),
                ("one.h", ""),
                ("two.h", ""),
            ],
            &["-c", "main.c"],
        ),
        ("trigraph_c11", &[("main.c", "??=include \"t.h\"\n#include \"a.h\"\n"), ("t.h", ""), ("a.h", "")], &["-x", "c", "-std=c11", "-c", "main.c"]),
        ("trigraph_gnu11", &[("main.c", "#include \"a.h\"\n"), ("a.h", "")], &["-x", "c", "-std=gnu11", "-c", "main.c"]),
        ("trigraph_splice_comment_end", &[("main.c", "/* comment *??/\n/\n#include \"t.h\"\n/* */\n"), ("t.h", "")], &["-x", "c", "-std=c11", "-c", "main.c"]),
        (
            "query_operand_macro",
            &[
                ("main.c", "#define B __builtin_expect\n#if __has_builtin(B)\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n"),
                ("yes.h", ""),
                ("no.h", ""),
            ],
            &["-c", "main.c"],
        ),
        (
            "imacros_after_include_order",
            &[
                ("main.c", "int z;\n"),
                ("a.h", "#ifdef FROM_B\n#include \"yes.h\"\n#else\n#include \"no.h\"\n#endif\n"),
                ("b.h", "#define FROM_B 1\n"),
                ("yes.h", ""),
                ("no.h", ""),
            ],
            &["-include", "a.h", "-imacros", "b.h", "-c", "main.c"],
        ),
        (
            "quote_dir_also_system",
            &[("main.c", "#include \"x.h\"\n"), ("sys/x.h", ""), ("inc/x.h", "")],
            &["-iquote", "sys", "-isystem", "sys", "-I", "inc", "-c", "main.c"],
        ),
        (
            "includer_dirs",
            &[("src/main.cc", "#include <sub/a.h>\n"), ("inc/sub/a.h", "#include \"b.h\"\n"), ("src/b.h", ""), ("other/b.h", "")],
            &["-Iinc", "-Iother", "-c", "src/main.cc"],
        ),
        (
            "forced_include_includer_dirs",
            &[("src/main.cc", "int m;\n"), ("inc/pre.h", "#include \"x.h\"\n"), ("src/x.h", ""), ("other/x.h", "")],
            &["-include", "pre.h", "-Iinc", "-Iother", "-c", "src/main.cc"],
        ),
        (
            "line_directive_in_if",
            &[("main.c", "#line 100\n#if __LINE__ == 100\n#include \"a.h\"\n#else\n#include \"b.h\"\n#endif\n"), ("a.h", ""), ("b.h", "")],
            &["-c", "main.c"],
        ),
        ("joined_include_flag", &[("main.c", "int q;\n"), ("pre.h", "#include \"inner.h\"\n"), ("inner.h", "")], &["-includepre.h", "-c", "main.c"]),
        (
            "c_source_as_cxx",
            &[("main.c", "#ifdef __cplusplus\n#include \"cxx.h\"\n#else\n#include \"c.h\"\n#endif\n"), ("cxx.h", ""), ("c.h", "")],
            &["-c", "main.c"],
        ),
        (
            "pragma_include_alias",
            &[("main.c", "#pragma include_alias(\"alias.h\", \"real.h\")\n#include \"alias.h\"\n"), ("alias.h", ""), ("real.h", "")],
            &["-c", "main.c"],
        ),
        (
            "per_object_flags",
            &[("main.c", "#include \"a.h\"\n"), ("a.h", "")],
            &["-frandom-seed=main.o", "-MD", "-MF", "main.d", "-c", "main.c", "-o", "main.o"],
        ),
    ];

    /// cl-style differential cases.
    const CL_CASES: &[Case] = &[
        ("plain_cl", &[("main.cc", "#include \"a.h\"\n"), ("a.h", "")], &["/nologo", "/c", "main.cc"]),
        (
            "external_i_after_i",
            &[("main.cc", "#include <x.h>\n"), ("ext/x.h", ""), ("inc/x.h", "")],
            &["/nologo", "/external:I", "ext", "/I", "inc", "/c", "main.cc"],
        ),
        (
            "user_dir_also_external",
            &[("main.cc", "#include <x.h>\n"), ("sdk/x.h", ""), ("other/x.h", "")],
            &["/nologo", "/I", "sdk", "/I", "other", "/external:I", "sdk", "/c", "main.cc"],
        ),
        (
            "forced_include_includer_stack",
            &[("src/main.cc", "int m;\n"), ("inc/pre.h", "#include \"x.h\"\n"), ("src/x.h", ""), ("other/x.h", "")],
            &["/nologo", "/FIpre.h", "/Iinc", "/Iother", "/c", "src/main.cc"],
        ),
        (
            "pragma_include_alias_cl",
            &[("main.cc", "#pragma include_alias(\"alias.h\", \"real.h\")\n#include \"alias.h\"\n"), ("alias.h", ""), ("real.h", "")],
            &["/nologo", "/c", "main.cc"],
        ),
        (
            "counter_in_if_cl",
            &[
                ("main.cc", "int a = __COUNTER__;\n#if __COUNTER__ == 1\n#include \"one.h\"\n#else\n#include \"zero.h\"\n#endif\n"),
                ("one.h", ""),
                ("zero.h", ""),
            ],
            &["/nologo", "/c", "main.cc"],
        ),
        ("cr_only_cl", &[("main.cc", "#include \"a.h\"\r#include \"b.h\"\rint x;\r"), ("a.h", ""), ("b.h", "")], &["/nologo", "/c", "main.cc"]),
        ("trigraphs_cl", &[("main.cc", "??=include \"t.h\"\n#include \"a.h\"\n"), ("t.h", ""), ("a.h", "")], &["/nologo", "/Zc:trigraphs", "/c", "main.cc"]),
    ];

    /// The cases against GCC or Clang, when `CPP_DEPS_TEST_CXX` names one.
    #[test]
    fn real_differential() {
        let Some(cxx) = std::env::var_os("CPP_DEPS_TEST_CXX") else { return };
        let exact = differential(&cxx.to_string_lossy(), GCC_CASES);
        for name in ["plain", "cr_only_line_endings", "include_next_from_iquote", "imacros_after_include_order", "per_object_flags"] {
            assert!(exact.contains(&name), "{name} isn't exact: {exact:?}");
        }
    }

    /// The cl-style cases against `CPP_DEPS_TEST_CL` (cl.exe, with INCLUDE
    /// set) and `CPP_DEPS_TEST_CLANG_CL`.
    #[test]
    fn real_cl_differential() {
        for var in ["CPP_DEPS_TEST_CL", "CPP_DEPS_TEST_CLANG_CL"] {
            let Some(cl) = std::env::var_os(var) else { continue };
            let exact = differential(&cl.to_string_lossy(), CL_CASES);
            for name in ["plain_cl", "external_i_after_i", "forced_include_includer_stack"] {
                assert!(exact.contains(&name), "{var}: {name} isn't exact: {exact:?}");
            }
        }
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
