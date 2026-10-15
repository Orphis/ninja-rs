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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// Some file or directory is named by a relative path (see
    /// `Config::relative_paths`).
    pub relative_paths: bool,
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
        probe::Compiler {
            path: &inv.compiler,
            args: &inv.probe_args,
            style: inv.style,
            lang: &inv.lang,
            cwd,
            env: self.vars.as_ref().map(|v| v.as_slice()),
            timeout: probe::TIMEOUT,
        }
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
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
    "DRIVERKIT_DEPLOYMENT_TARGET",
    // Whether Clang reads its default configuration files.
    "CLANG_NO_DEFAULT_CONFIG",
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
        relative_paths: false,
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
/// `Merge`); `id` tells which directories are the same (None: not a
/// directory, which GCC drops, and the others search in vain or read as a
/// header map).
fn search_lists(merge: Merge, quote: &[Dir], user: &[Dir], system: [&[Dir]; 3], id: impl Fn(&Dir) -> Option<DirId>) -> (Vec<Dir>, Vec<Dir>, Vec<Dir>) {
    let ids = |dirs: &[Dir]| {
        dirs.iter()
            .map(|d| {
                let spelled = || match d {
                    Dir::Path(p) => (false, p.clone()),
                    Dir::Framework(p) => (true, p.clone()),
                };
                (d.clone(), if merge == Merge::Gcc { id(d) } else { Some(id(d).unwrap_or_else(spelled)) })
            })
            .collect::<Vec<_>>()
    };
    // GCC's `remove_duplicates`: first places only, without the
    // directories of `skip`, and without the list's last one (before
    // anything is removed) when it's `join`.
    let dedup = |dirs: Vec<(Dir, Option<DirId>)>, skip: &HashSet<DirId>, join: Option<&DirId>| {
        let last = dirs.len().checked_sub(1);
        let mut seen = HashSet::new();
        dirs.into_iter()
            .enumerate()
            .filter_map(|(n, (d, i))| {
                let i = i?;
                let dropped = skip.contains(&i) || (Some(n) == last && join == Some(&i));
                (!dropped && seen.insert(i.clone())).then_some((d, i))
            })
            .collect::<Vec<_>>()
    };
    let all_system: Vec<(Dir, Option<DirId>)> = system.iter().flat_map(|s| ids(s)).collect();
    let none = HashSet::new();
    let (system, sys_ids): (Vec<(Dir, DirId)>, HashSet<DirId>) = match merge {
        Merge::Msvc => (all_system.into_iter().filter_map(|(d, i)| Some((d, i?))).collect(), ids(system[0]).into_iter().filter_map(|(_, i)| i).collect()),
        Merge::Gcc | Merge::Clang => {
            let system = dedup(all_system, &none, None);
            let sys_ids = system.iter().map(|(_, i)| i.clone()).collect();
            (system, sys_ids)
        }
    };
    let user = dedup(ids(user), &sys_ids, None);
    // GCC's last `-iquote` directory isn't searched when the chain it joins
    // starts with it.
    let join = if merge == Merge::Gcc { user.first().or(system.first()).map(|(_, i)| i) } else { None };
    let quote = dedup(ids(quote), if merge == Merge::Gcc { &sys_ids } else { &none }, join);
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
    ("-iframeworkwithsysroot", Effect::Unsupported),
    ("-isystem-after", Effect::Unsupported),
    ("-cxx-isystem", Effect::Unsupported),
    ("-working-directory", Effect::Unsupported),
];

/// cc1's flags with a separate argument, given through `-Xclang` (each
/// part after its own `-Xclang`).
const XCLANG_WITH_ARG: &[(&str, Effect)] = &[
    ("-fdebug-compilation-dir", Effect::Drop),
    ("-fcoverage-compilation-dir", Effect::Drop),
    ("-main-file-name", Effect::Drop),
    ("-dwarf-debug-flags", Effect::Drop),
    ("-dwarf-debug-producer", Effect::Drop),
    ("-split-dwarf-file", Effect::Drop),
    ("-split-dwarf-output", Effect::Drop),
    ("-coverage-notes-file", Effect::Drop),
    ("-coverage-data-file", Effect::Drop),
    ("-ferror-limit", Effect::Drop),
    ("-fmessage-length", Effect::Drop),
    ("-ftemplate-depth", Effect::Drop),
    ("-fconstexpr-depth", Effect::Drop),
    ("-mllvm", Effect::Probe),
    ("-triple", Effect::Probe),
    ("-target-cpu", Effect::Probe),
    ("-tune-cpu", Effect::Probe),
    ("-target-feature", Effect::Probe),
    ("-target-abi", Effect::Probe),
    ("-mrelocation-model", Effect::Probe),
    ("-pic-level", Effect::Probe),
    ("-mcode-model", Effect::Probe),
    ("-stack-protector", Effect::Probe),
    // Plugins may do anything (handle pragmas, watch the preprocessor).
    ("-load", Effect::Unsupported),
    ("-plugin", Effect::Unsupported),
    ("-add-plugin", Effect::Unsupported),
];

/// cc1's flags (through `-Xclang`) with a joined value that change nothing
/// the preprocessor does: libraries for the linker (CMake's
/// `-Xclang --dependent-lib=msvcrt` for Clang targeting the MSVC ABI).
const XCLANG_DROPPED_PREFIXES: &[&str] = &["--dependent-lib=", "--linker-option="];

/// Flags naming a file or directory the compile reads besides its sources
/// and headers (profiles, lists, plugins, configuration files), joined
/// with `=`: inputs no scan finds (see `read_paths`), and what they do to
/// a scan. A profile used defines `__LLVM_INSTR_PROFILE_USE`.
const READS: &[(&str, Effect)] = &[
    ("-fprofile-use=", Effect::Probe),
    ("-fprofile-instr-use=", Effect::Probe),
    ("-fprofile-sample-use=", Effect::Drop),
    ("-fauto-profile=", Effect::Drop),
    ("-fprofile-list=", Effect::Drop),
    ("-fprofile-remapping-file=", Effect::Drop),
    ("-fsanitize-ignorelist=", Effect::Drop),
    ("-fsanitize-blacklist=", Effect::Drop),
    ("-fsanitize-system-ignorelist=", Effect::Drop),
    ("-fsanitize-coverage-allowlist=", Effect::Drop),
    ("-fsanitize-coverage-ignorelist=", Effect::Drop),
    ("-fsanitize-coverage-whitelist=", Effect::Drop),
    ("-fsanitize-coverage-blacklist=", Effect::Drop),
    ("-fxray-always-instrument=", Effect::Drop),
    ("-fxray-never-instrument=", Effect::Drop),
    ("-fxray-attr-list=", Effect::Drop),
    ("-fpass-plugin=", Effect::Drop),
    ("-fbasic-block-sections=list=", Effect::Drop),
    ("-fplugin=", Effect::Probe),
    ("-specs=", Effect::Probe),
    ("--specs=", Effect::Probe),
    ("--config=", Effect::Probe),
    // Parsed (see `parse_gcc`).
    ("-fmodule-file=", Effect::Drop),
    ("-ivfsoverlay=", Effect::Drop),
];

/// Flags with a separate argument naming a file the compile reads (see
/// `READS`), also spelled joined without `=`.
const READS_WITH_ARG: &[&str] = &["--config", "-include-pch", "-ivfsoverlay", "-vfsoverlay", "--vfsoverlay"];

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

/// Flags that change nothing the preprocessor does (`dropped_flags_keep_
/// predefines` checks them against compilers).
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
    "-Qunused-arguments",
    "-Qn",
    "-Qy",
    "-s",
    "-shared",
    "-shared-libgcc",
    "-static-libgcc",
    "-static-libstdc++",
    "-static-pie",
    "-rdynamic",
    "-pie",
    "-no-pie",
    "-nostdlib++",
    "-nodefaultlibs",
    "-nostartfiles",
    "-nolibc",
    "-time",
    "-emit-llvm",
    "-integrated-as",
    "-no-integrated-as",
    // What -E prints (macros, includes).
    "-dM",
    "-dD",
    "-dN",
    "-dI",
    "-dU",
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
    // Where profiles go, which profiles say, and precompiled headers
    // (which stand for headers the scan reads); files the compile reads
    // are in `READS`.
    "-fprofile-dir=",
    "-fprofile-exclude-files=",
    "-fprofile-filter-files=",
    "-fprofile-note=",
    "-fprofile-prefix-path=",
    "-fprofile-prefix-map=",
    "-fprofile-update=",
    "-fpch-",
    "-fno-pch-",
    // Linking and assembling.
    "--param=",
    "-Wa,",
    "-Wl,",
    "-l",
    "-L",
    "-dylib",
    "-dylinker",
    "-dynamiclib",
    "-dead_strip",
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
/// (`-f...`, which no list above names). Some linking ones do: `-static`
/// (`__STATIC__` for Apple targets), `-nostdlib` (Ubuntu's GCC then has no
/// `__SSP_STRONG__`), and `-pedantic` (g++'s `__cpp_runtime_arrays`).
const PROBED: &[&str] = &[
    "-ansi",
    "-pedantic",
    "-pedantic-errors",
    "--pedantic",
    "--pedantic-errors",
    "-static",
    "-dynamic",
    "-nostdlib",
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
    "-B",
    "--target=",
    "--sysroot=",
    "--gcc-toolchain=",
    "--gcc-install-dir=",
    "-stdlib=",
    "--stdlib=",
    "-isysroot",
    "-resource-dir=",
];

/// Probed flags with a separate argument naming a file or directory, made
/// absolute (see `absolute_arg`).
const PROBED_PATH_WITH_ARG: &[&str] = &[
    "--sysroot",
    "-isysroot",
    "-resource-dir",
    "-gcc-toolchain",
    "--gcc-toolchain",
    "-B",
    "/winsysroot",
    "-winsysroot",
    "/vctoolsdir",
    "-vctoolsdir",
    "/winsdkdir",
    "-winsdkdir",
];

/// Probed flags with a joined value naming a file or directory.
const PROBED_PATH_PREFIXES: &[&str] = &[
    "--sysroot=",
    "--gcc-toolchain=",
    "--gcc-install-dir=",
    "-resource-dir=",
    "-fprofile-use=",
    "-fprofile-instr-use=",
    "-fplugin=",
    "-specs=",
    "--specs=",
    "-isysroot",
    "-B",
    "/winsysroot",
    "-winsysroot",
    "/vctoolsdir",
    "-vctoolsdir",
    "/winsdkdir",
    "-winsdkdir",
];

/// A probed flag (`flag` with a separate value `v`, or joined when `v` is
/// None) with the file or directory it names relative to `cwd` made
/// absolute: the profile is then the same in every working directory
/// (CMake's `try_compile` makes one per check). A configuration file named
/// without a directory is searched for, not relative.
fn absolute_arg(flag: &str, v: Option<&str>, cwd: &Path) -> String {
    let absolute = |p: &str| absolute_path(p, cwd);
    let config = |p: &str| if p.contains(['/', '\\']) { absolute(p) } else { p.to_string() };
    match v {
        Some(v) if flag == "--config" => config(v),
        Some(v) if PROBED_PATH_WITH_ARG.contains(&flag) => absolute(v),
        Some(v) => v.to_string(),
        None => {
            if let Some(c) = flag.strip_prefix("--config=") {
                return format!("--config={}", config(c));
            }
            match PROBED_PATH_PREFIXES.iter().find(|p| flag.starts_with(*p)) {
                // (`-B` may be a prefix of programs' names.)
                Some(&"-B") if !flag.contains(['/', '\\']) => flag.to_string(),
                Some(p) => format!("{p}{}", absolute(&flag[p.len()..])),
                None => flag.to_string(),
            }
        }
    }
}

/// `p`, relative to `cwd`, made absolute.
fn absolute_path(p: &str, cwd: &Path) -> String {
    if Path::new(p).is_relative() && !p.is_empty() { scan::normalize(&cwd.join(p)).to_string_lossy().into_owned() } else { p.to_string() }
}

/// Profiles made: their files don't change what the compiler predefines
/// (`__LLVM_INSTR_PROFILE_GENERATE`), so the probe has them without (one
/// profile for every object).
const PROFILE_GENERATE: &[&str] = &["-fprofile-generate=", "-fprofile-instr-generate=", "-fcs-profile-generate="];

/// The CPU flags whose `native` the compiler resolves on the machine it runs
/// on: the probe's (here) and the remote compile's may differ.
fn native_cpu(a: &str) -> bool {
    ["-march=", "-mcpu=", "-mtune="].iter().any(|p| a.strip_prefix(p) == Some("native"))
}

/// What a GCC or Clang flag (without a separate argument) does to a scan.
/// Flags no list names are unsupported.
fn gcc_effect(a: &str) -> Effect {
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| a.starts_with(p));
    // Options for the preprocessor itself: see `wp_options`.
    if UNSUPPORTED.contains(&a) || starts(UNSUPPORTED_PREFIXES) || native_cpu(a) || a.starts_with("-Wp,") {
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
    if let Some(&(_, effect)) = READS.iter().find(|(p, _)| a.starts_with(p)) {
        return effect;
    }
    if starts(PROFILE_GENERATE) {
        return Effect::Probe;
    }
    if DROPPED.contains(&a) || starts(DROPPED_PREFIXES) {
        return Effect::Drop;
    }
    if PROBED.contains(&a) || starts(PROBED_PREFIXES) {
        return Effect::Probe;
    }
    Effect::Unsupported
}

/// What `-Xclang <v>` does to a scan, and how many arguments it takes after
/// `-Xclang`: cc1's flags with an argument take `-Xclang <operand>` too.
fn xclang_effect(v: &str, operand: Option<&str>) -> (usize, Effect) {
    // cc1 takes them without the driver knowing.
    if ms_compat_flag(v) {
        return (1, Effect::Unsupported);
    }
    if let (Some(&(_, effect)), Some(operand)) = (XCLANG_WITH_ARG.iter().find(|(f, _)| *f == v), operand) {
        // See `native_cpu`.
        return (3, if operand == "native" && v.ends_with("-cpu") { Effect::Unsupported } else { effect });
    }
    // (cc1's `-W` are all warnings: it takes `-Wp,-DX` as an unknown one.)
    if XCLANG_DROPPED_PREFIXES.iter().any(|p| v.starts_with(p)) || v.starts_with("-W") {
        return (1, Effect::Drop);
    }
    (1, gcc_effect(v))
}

/// `-Xclang` and what follows (see `xclang_effect`), as the probe has them.
fn xclang_probe_args(args: &[String], cwd: &Path) -> Vec<String> {
    args.iter().enumerate().map(|(i, a)| if i == 1 { probe_arg(a, cwd) } else { a.clone() }).collect()
}

/// A flag as the probe has it: without the files of profiles made, with
/// those it reads absolute.
fn probe_arg(a: &str, cwd: &Path) -> String {
    match PROFILE_GENERATE.iter().find(|p| a.starts_with(*p)) {
        Some(p) => p.trim_end_matches('=').to_string(),
        None => absolute_arg(a, None, cwd),
    }
}

/// The sysroot of headers, which `=` at the start of a directory stands
/// for: the last `-isysroot`, else the last `--sysroot`.
fn header_sysroot(argv: &[String]) -> Option<String> {
    let (mut sysroot, mut isysroot) = (None, None);
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        i += 1;
        match a {
            "--sysroot" => sysroot = argv.get(i).cloned(),
            "-isysroot" => isysroot = argv.get(i).cloned(),
            _ => {
                if let Some(v) = a.strip_prefix("--sysroot=") {
                    sysroot = Some(v.to_string());
                } else if let Some(v) = a.strip_prefix("-isysroot").filter(|v| !v.is_empty()) {
                    isysroot = Some(v.to_string());
                }
                if !(a == "-Xclang" || SCAN_WITH_ARG.contains(&a) || WITH_ARG.iter().any(|(f, _)| *f == a)) {
                    continue;
                }
            }
        }
        i += 1;
    }
    isysroot.or(sysroot)
}

/// `p` relative to `cwd`, with `..` applied lexically; the first `..`
/// after a symbolic link leading elsewhere (see `scan::dotdot_after_link`)
/// is noted in `linked`.
fn joined(cwd: &Path, p: &str, linked: &RefCell<Option<PathBuf>>) -> PathBuf {
    let path = cwd.join(p);
    if let Some(link) = scan::dotdot_after_link(&path) {
        linked.borrow_mut().get_or_insert(link);
    }
    scan::normalize(&path)
}

/// The compiler of a command: a relative path made absolute (the profile's
/// key and the probe then don't depend on the working directory); a name
/// alone is found in `PATH`.
fn compiler_path(program: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(program);
    if p.is_relative() && p.components().count() > 1 { scan::normalize(&cwd.join(p)) } else { p.to_path_buf() }
}

/// What `-Wp,` options do, split at commas as GCC does: dependency files
/// (Linux's `-Wp,-MMD,file`), which the probe mustn't write, and macros and
/// directories, which GCC and Clang take after the command line's other
/// `-D`, `-U` and `-I`. None for options the scan doesn't model.
fn wp_options(opts: &str, commands: &mut Vec<CommandMacro>, dirs: &mut Vec<String>) -> Option<()> {
    let mut opts = opts.split(',');
    while let Some(o) = opts.next() {
        // Joined (`-DX`), or the next option (`-D,X`).
        let mut value = |flag: &str| -> Option<Option<String>> {
            let v = o.strip_prefix(flag)?;
            Some(if v.is_empty() { opts.next().map(str::to_string) } else { Some(v.to_string()) })
        };
        if let Some(v) = value("-D") {
            commands.push(CommandMacro::Define(v?));
        } else if let Some(v) = value("-U") {
            commands.push(CommandMacro::Undef(v?));
        } else if let Some(v) = value("-I") {
            dirs.push(v.filter(|v| v != "-")?);
        } else {
            match o {
                "-MD" | "-MMD" | "-MF" | "-MT" | "-MQ" => {
                    opts.next();
                }
                "-M" | "-MM" | "-MP" | "-MG" => {}
                _ => return None,
            }
        }
    }
    Some(())
}

fn parse_gcc(argv: &[String], cwd: &Path) -> Result<Invocation, String> {
    // Whether the command line names some file or directory by a relative
    // path (see `Config::relative_paths`), and a `..` after a symbolic link
    // (which compilers resolve elsewhere than remote workers).
    let (relative, linked) = (Cell::new(false), RefCell::new(None));
    let abs = |p: &str| -> PathBuf {
        relative.set(relative.get() || Path::new(p).is_relative());
        joined(cwd, p, &linked)
    };
    // A directory: `=` at its start stands for the sysroot, given one
    // (GCC's `$SYSROOT` too; Clang takes them literally otherwise, GCC as
    // its configured sysroot: unknown).
    let sysroot = header_sysroot(argv);
    let dir = |v: &str| -> Result<PathBuf, String> {
        if let (Some(rest), Some(root)) = (v.strip_prefix('='), &sysroot)
            && rest.starts_with(['/', '\\'])
        {
            return Ok(abs(&format!("{root}{rest}")));
        }
        if v.starts_with('=') || v.starts_with("$SYSROOT") {
            return Err(format!("unsupported directory {v} (in the sysroot)"));
        }
        Ok(abs(v))
    };
    let forced = |p: &str| if Path::new(p).is_absolute() { Forced::Path(abs(p)) } else { Forced::Gcc { cwd: cwd.to_path_buf(), name: p.to_string() } };
    let unsupported = |a: &str| Err(format!("unsupported flag {a}"));
    let mut inv = new_invocation(argv, Style::Gcc)?;
    inv.compiler = compiler_path(&argv[0], cwd);
    let mut cxx = cxx_driver(&argv[0]);
    // The `-x` in effect, and the one the source came after.
    let (mut x, mut lang): (Option<String>, Option<String>) = (None, None);
    // `-f[no-]ms-compatibility` and `-f[no-]ms-extensions`, the last ones.
    let (mut ms_compat, mut ms_ext): (Option<bool>, Option<bool>) = (None, None);
    // A precompiled header stands for the header it was made of, which
    // must be forced too: its macros and files are unknown otherwise.
    let mut include_pch = false;
    // `-Wp,` macros and directories, which come after the others.
    let (mut wp_commands, mut wp_dirs) = (Vec::new(), Vec::new());
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
                (v, operand) => {
                    let (taken, effect) = xclang_effect(v, operand.map(String::as_str));
                    match effect {
                        Effect::Drop => {}
                        Effect::Probe => inv.probe_args.extend(xclang_probe_args(&argv[i - 1..i + taken], cwd)),
                        Effect::Unsupported => return unsupported(&format!("-Xclang {v}")),
                    }
                    i += taken;
                }
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
            if matches!(flag, "-F" | "-iframework" | "--embed-dir") && v.starts_with('=') {
                return unsupported(&format!("{flag} {v}"));
            }
            match flag {
                "-I" => inv.user_dirs.push(Dir::Path(dir(&v)?)),
                "-F" => inv.user_dirs.push(Dir::Framework(abs(&v))),
                "-iquote" => inv.quote_dirs.push(Dir::Path(dir(&v)?)),
                "-isystem" => inv.system_dirs.push(Dir::Path(dir(&v)?)),
                "-iframework" => inv.system_dirs.push(Dir::Framework(abs(&v))),
                "-idirafter" => inv.after_dirs.push(Dir::Path(dir(&v)?)),
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
                Effect::Probe => inv.probe_args.extend([a.clone(), absolute_arg(a, Some(&v), cwd)]),
                Effect::Unsupported => return unsupported(a),
            }
            continue;
        }
        let joined = |p: &str| a.strip_prefix(p).filter(|v| !v.is_empty());
        if ["-F", "-iframework", "--embed-dir="].iter().any(|f| joined(f).is_some_and(|v| v.starts_with('='))) {
            return unsupported(a);
        }
        if let Some(v) = joined("-iquote") {
            inv.quote_dirs.push(Dir::Path(dir(v)?));
        } else if let Some(v) = joined("-isystem") {
            inv.system_dirs.push(Dir::Path(dir(v)?));
        } else if a.starts_with("-iframeworkwithsysroot") {
            return unsupported(a);
        } else if let Some(v) = joined("-iframework") {
            inv.system_dirs.push(Dir::Framework(abs(v)));
        } else if let Some(v) = joined("-idirafter") {
            inv.after_dirs.push(Dir::Path(dir(v)?));
        } else if let Some(v) = joined("-ivfsoverlay") {
            inv.overlays.push(abs(v.trim_start_matches('=')));
        } else if let Some(v) = joined("--embed-dir=") {
            inv.embed_dirs.push(abs(v));
        } else if let Some(v) = joined("-include") {
            inv.forced.push(forced(v));
        } else if let Some(v) = joined("-imacros") {
            inv.imacros.push(forced(v));
        } else if let Some(v) = joined("-I").filter(|v| *v != "-") {
            inv.user_dirs.push(Dir::Path(dir(v)?));
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
            if wp_options(opts, &mut wp_commands, &mut wp_dirs).is_none() {
                return unsupported(a);
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
                Effect::Probe => inv.probe_args.push(probe_arg(a, cwd)),
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
    inv.commands.extend(wp_commands);
    for d in &wp_dirs {
        inv.user_dirs.push(Dir::Path(dir(d)?));
    }
    // Without `-fms-compatibility`, `-fno-ms-extensions` turns it off.
    inv.ms_compat = ms_compat.or(ms_ext.filter(|e| !e));
    inv.relative_paths = relative.get() || inv.forced.iter().chain(&inv.imacros).any(|f| matches!(f, Forced::Gcc { .. }));
    if let Some(link) = linked.take() {
        return Err(format!("`..` after the symbolic link {}", link.display()));
    }
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
    // Warnings: /WX, /Wv:, /w1 to /w4, clang-cl's -Wfoo and -Wno-foo (not
    // /winsdkdir and the like). Not the warning level (/W0 to /W4, /Wall,
    // /w, /external:W...): cl predefines it (`_MSVC_WARNING_LEVEL`).
    let warning = f.starts_with("WX")
        || (f.starts_with('w') && f.as_bytes().get(1).is_some_and(|c| (b'1'..=b'4').contains(c)))
        || (f.starts_with('W') && f.as_bytes().get(1).is_some_and(u8::is_ascii_lowercase) && f != "Wall");
    // Preprocessing to a file or stdout (which the probe mustn't).
    let preprocess = matches!(f, "P" | "E" | "EP" | "C");
    warning || preprocess || f == "c" || f == "showIncludes" || f == "nologo" || f == "Gy" || f == "Gw" || PREFIXES.iter().any(|p| f.starts_with(p))
}

/// cl's warning levels (without `/` or `-`), which cl predefines
/// (`_MSVC_WARNING_LEVEL`) and clang-cl doesn't.
fn cl_warning_level(f: &str) -> bool {
    f == "w" || f == "Wall" || f.starts_with("external:W") || (f.len() == 2 && f.starts_with('W') && f.as_bytes()[1].is_ascii_digit())
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

/// clang-cl's flags (without `/` or `-`) with a value joined or separate
/// that the parser doesn't take: where the MSVC headers are (probed), and
/// the object file.
const CL_WITH_ARG: &[(&str, Effect)] = &[
    ("winsysroot", Effect::Probe),
    ("vctoolsdir", Effect::Probe),
    ("vctoolsversion", Effect::Probe),
    ("winsdkdir", Effect::Probe),
    ("winsdkversion", Effect::Probe),
    ("o", Effect::Drop),
];

fn parse_cl(argv: &[String], cwd: &Path, env: &Environment) -> Result<Invocation, String> {
    let (relative, linked) = (Cell::new(false), RefCell::new(None));
    let abs = |p: &str| -> PathBuf {
        relative.set(relative.get() || Path::new(p).is_relative());
        joined(cwd, p, &linked)
    };
    let unsupported = |a: &str| Err(format!("unsupported flag {a}"));
    // cl and clang-cl take flags from `CL` before the command's, and from
    // `_CL_` after: the probe sees them, the scan doesn't.
    for var in ["CL", "_CL_"] {
        for a in env.var(var).map(|v| split_windows(&v, false)).unwrap_or_default() {
            if a.strip_prefix('/').or_else(|| a.strip_prefix('-')).is_none_or(|f| cl_parsed(f) || cl_unsupported(f)) {
                return Err(format!("{var} has {a}"));
            }
        }
    }
    let mut inv = new_invocation(
        argv,
        if Path::new(&argv[0].replace('\\', "/")).file_stem().is_some_and(|s| s.eq_ignore_ascii_case("cl")) { Style::Msvc } else { Style::ClangCl },
    )?;
    inv.compiler = compiler_path(&argv[0], cwd);
    let mut lang: Option<String> = None;
    // `/clang:-Wp,` macros and directories, which come after the others.
    let (mut wp_commands, mut wp_dirs) = (Vec::new(), Vec::new());
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
        if cl_dropped(f) || (inv.style == Style::ClangCl && cl_warning_level(f)) {
            continue;
        }
        if inv.style == Style::Msvc && (f.starts_with("clang:") || f == "Xclang") {
            // clang-cl's, which cl.exe ignores (taking what follows
            // `-Xclang` as its own).
            continue;
        }
        if cl_unsupported(f) || (inv.style == Style::Msvc && f.starts_with("imsvc")) {
            // (cl.exe has no `-imsvc`.)
            return unsupported(&a);
        }
        if let Some(&(name, effect)) = CL_WITH_ARG.iter().find(|(name, _)| f == *name) {
            // Separate (joined ones are probed as they are).
            let v = argv.get(i).ok_or_else(|| format!("/{name} needs an argument"))?.clone();
            i += 1;
            if effect == Effect::Probe {
                inv.probe_args.extend([a.clone(), absolute_arg(&a, Some(&v), cwd)]);
            }
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
        } else if let Some(g) = f.strip_prefix("clang:") {
            // A GCC-style flag: those the parser takes can't come this way.
            if let Some(&(_, effect)) = WITH_ARG.iter().find(|(w, _)| *w == g) {
                // Its argument follows as `/clang:<argument>`.
                let v = argv.get(i).cloned().unwrap_or_default();
                i += 1;
                match effect {
                    Effect::Drop => {}
                    Effect::Probe => {
                        let value = match v.split_at_checked(7) {
                            Some((prefix, value)) if prefix.eq_ignore_ascii_case("/clang:") || prefix == "-clang:" => {
                                format!("{prefix}{}", absolute_arg(g, Some(value), cwd))
                            }
                            _ => v,
                        };
                        inv.probe_args.extend([a.clone(), value]);
                    }
                    Effect::Unsupported => return unsupported(&a),
                }
                continue;
            }
            if let Some(opts) = g.strip_prefix("-Wp,") {
                if wp_options(opts, &mut wp_commands, &mut wp_dirs).is_none() {
                    return unsupported(&a);
                }
                continue;
            }
            match gcc_effect(g) {
                _ if ms_compat_flag(g) && g.starts_with("-fno-") => return unsupported(&a),
                Effect::Drop => {}
                Effect::Probe => inv.probe_args.push(format!("/clang:{}", probe_arg(g, cwd))),
                Effect::Unsupported => return unsupported(&a),
            }
        } else if f == "Xclang" || f == "target" {
            let v = argv.get(i).cloned().unwrap_or_default();
            // `-Xclang -ivfsoverlay -Xclang <file>`: an overlay too.
            if f == "Xclang" && v == "-ivfsoverlay" && argv.get(i + 1).is_some_and(|x| x == "-Xclang") {
                if let Some(o) = argv.get(i + 2) {
                    inv.overlays.push(abs(o));
                }
                inv.probe_args.extend(argv[i - 1..(i + 2).min(argv.len())].iter().cloned());
                inv.probe_args.extend(argv.get(i + 2).map(|o| absolute_path(o, cwd)));
                i += 3;
                continue;
            }
            if f == "Xclang" {
                let operand = (argv.get(i + 1).map(String::as_str) == Some("-Xclang")).then(|| argv.get(i + 2)).flatten();
                let (taken, effect) = xclang_effect(&v, operand.map(String::as_str));
                match effect {
                    Effect::Drop => {}
                    Effect::Probe => inv.probe_args.extend(xclang_probe_args(&argv[i - 1..i + taken], cwd)),
                    Effect::Unsupported => return unsupported(&format!("{a} {v}")),
                }
                i += taken;
                continue;
            }
            i += 1;
            inv.probe_args.push(a.clone());
            inv.probe_args.push(v);
        } else {
            if f == "Zc:preprocessor" || f == "experimental:preprocessor" {
                inv.conforming = true;
            }
            if f == "Zc:preprocessor-" {
                inv.conforming = false;
            }
            inv.probe_args.push(absolute_arg(&a, None, cwd));
        }
    }
    inv.commands.extend(wp_commands);
    inv.user_dirs.extend(wp_dirs.iter().map(|d| Dir::Path(abs(d))));
    inv.relative_paths = relative.get() || inv.forced.iter().any(|f| !matches!(f, Forced::Path(_)));
    if let Some(link) = linked.take() {
        return Err(format!("`..` after the symbolic link {}", link.display()));
    }
    let inv = finish(inv, lang, false)?;
    // C11 and later use the conforming preprocessor by default.
    let c_std = inv.probe_args.iter().any(|a| matches!(a.trim_start_matches(['/', '-']), "std:c11" | "std:c17" | "std:clatest"));
    Ok(Invocation { conforming: inv.conforming || (inv.lang == "c" && c_std), ..inv })
}

/// The files and directories a compile command's flags name that the
/// compile reads besides its sources and headers (see `READS`), and the
/// header maps it searches (`-I x.hmap`): inputs no scan finds, and those
/// it reads without listing them. Paths that don't exist are left out, but
/// precompiled headers and modules (which the build may have made
/// elsewhere). Returns (files, directories).
pub fn read_paths(argv: &[String], cwd: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let cl = is_cl_style(argv);
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    let mut add = |v: &str, made: bool| {
        let p = scan::normalize(&cwd.join(v));
        match std::fs::metadata(&p) {
            Ok(m) if m.is_dir() => dirs.push(p),
            Ok(_) => files.push(p),
            Err(_) if made => files.push(p),
            Err(_) => {}
        }
    };
    // clang-cl passes GCC-style flags on as `/clang:<flag>`.
    let arg = |i: usize| argv.get(i).map(|a| if cl { a.strip_prefix("/clang:").unwrap_or(a) } else { a.as_str() });
    // GCC's profiles beside the object (`-o`, else the source's name in the
    // working directory): `-fprofile-use` and `-fbranch-probabilities`
    // without a path read its `.gcda`.
    let (mut object, mut source, mut gcda) = (None, None, false);
    let mut i = 1;
    while let Some(a) = arg(i) {
        i += 1;
        if cl && matches!(a, "/reference" | "-reference" | "/ifcSearchDir" | "-ifcSearchDir") {
            // `name=file` or `file`; a directory of modules.
            if let Some(v) = arg(i) {
                let file = v.split_once('=').filter(|(name, _)| !name.contains(['/', '\\'])).map_or(v, |(_, f)| f);
                add(file, a.ends_with("reference"));
            }
            i += 1;
            continue;
        }
        if !cl {
            if a == "-o" {
                object = arg(i);
            } else if let Some(o) = a.strip_prefix("-o").filter(|o| !o.is_empty()) {
                object = Some(o);
            } else if !a.starts_with('-')
                && i >= 2
                && !matches!(arg(i - 2), Some(f) if f.starts_with('-') && (SCAN_WITH_ARG.contains(&f) || WITH_ARG.iter().any(|(w, _)| w == &f)))
            {
                source = Some(a);
            }
        }
        if a == "-Xclang" && arg(i + 1) == Some("-Xclang") && matches!(arg(i), Some("-ivfsoverlay" | "-include-pch")) {
            if let Some(v) = arg(i + 2) {
                add(v, arg(i) == Some("-include-pch"));
            }
            i += 3;
        } else if READS_WITH_ARG.contains(&a) || matches!(a, "-I" | "-iquote" | "/I") {
            if let Some(v) = arg(i).filter(|v| !matches!(a, "-I" | "-iquote" | "/I") || v.ends_with(".hmap")) {
                add(v, a == "-include-pch");
            }
            i += 1;
        } else if let Some((p, _)) = READS.iter().find(|(p, _)| a.starts_with(p)) {
            let v = &a[p.len()..];
            // `-fmodule-file=[<name>=]<file>`.
            match *p {
                "-fmodule-file=" => add(v.split_once('=').map_or(v, |(_, f)| f), true),
                _ => add(v, false),
            }
        } else if let Some(v) = a.strip_prefix("-ivfsoverlay").filter(|v| !v.is_empty()) {
            add(v, false);
        } else if let Some(v) = ["-I", "-iquote", "/I"].iter().find_map(|f| a.strip_prefix(f)).filter(|v| v.ends_with(".hmap")) {
            add(v, false);
        } else if matches!(a, "-fprofile-use" | "-fprofile-instr-use") {
            // Clang's default profile.
            add("default.profdata", false);
            gcda |= a == "-fprofile-use";
        } else if a == "-fbranch-probabilities" {
            gcda = true;
        } else if a == "-fauto-profile" {
            // GCC's default AutoFDO profile.
            add("fbdata.afdo", false);
        }
    }
    let object = object.map(PathBuf::from).or_else(|| source.and_then(|s| Path::new(s).file_name()).map(|n| Path::new(n).with_extension("o")));
    if let Some(object) = object.filter(|_| gcda) {
        add(&object.with_extension("gcda").to_string_lossy(), false);
    }
    (files, dirs)
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
    /// Asking the compiler failed or took too long: not again (its scans
    /// stay inexact rather than wait each time).
    gave_up: AtomicBool,
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
        // cl's integer suffixes: cl's, and Clang's with Microsoft extensions.
        let ms_suffixes = (msc && !clang) || (clang && defined("_MSC_EXTENSIONS"));
        Profile {
            predefined: Arc::new(predefined),
            predefines,
            operators: Arc::new(operators.iter().map(|o| intern::intern(o)).collect()),
            dialect: crate::expr::Dialect { ms_suffixes, ..dialect },
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
            gave_up: AtomicBool::new(false),
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
                ["dialect", t, a] => dialect = Some(crate::expr::Dialect { true_false: *t == "1", alt_operators: *a == "1", ms_suffixes: false }),
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

/// Something loaded (a profile, an overlay, a header map), or why it
/// couldn't be, with the session it was loaded in; concurrent users wait
/// for the first.
type Loaded<T> = Arc<OnceLock<(u64, Result<Arc<T>, String>)>>;

/// What `key` names, loaded once, and again when `stale` says what was
/// loaded (in the session it gives) is.
fn load_once<K: std::hash::Hash + Eq, T>(
    map: &Mutex<HashMap<K, Loaded<T>>>,
    key: K,
    session: u64,
    stale: impl Fn(u64, &Result<Arc<T>, String>) -> bool,
    load: impl FnOnce() -> Result<Arc<T>, String>,
) -> Result<Arc<T>, String> {
    let cell = {
        let mut map = map.lock().unwrap();
        let cell = map.entry(key).or_default();
        if cell.get().is_some_and(|(s, loaded)| stale(*s, loaded)) {
            *cell = Loaded::default();
        }
        cell.clone()
    };
    cell.get_or_init(|| (session, load())).1.clone()
}

/// A compiler's fingerprint, taken in a session (once: concurrent scans
/// wait for the first).
type Fingerprint = (u64, Arc<OnceLock<String>>);

/// What a search directory's path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKind {
    Missing,
    Dir,
    HeaderMap,
    /// Another file (which compilers ignore).
    File,
}

/// Scans compile commands, with everything learned about compilers cached.
pub struct Driver {
    pub cache: Cache,
    /// Probed again after `PROFILE_LIFETIME`, however long the process
    /// lives (the daemon's); one that failed, in the next session.
    profiles: Mutex<HashMap<String, Loaded<Profile>>>,
    /// Compilers' fingerprints (`probe::fingerprint`), by path, taken once
    /// per session (build); concurrent scans wait for the first.
    fingerprints: Mutex<HashMap<PathBuf, Fingerprint>>,
    /// Digests of configuration files and directories (see
    /// `config_digest`), by path, taken once per session.
    configs: Mutex<HashMap<PathBuf, (u64, u64)>>,
    /// The session the maps were last pruned in (see `prune`).
    pruned: AtomicU64,
    /// Read again every session.
    overlays: Mutex<HashMap<PathBuf, Loaded<Overlay>>>,
    header_maps: Mutex<HashMap<PathBuf, Loaded<HeaderMap>>>,
    /// Search directories' real paths, and what they are, by path, taken
    /// once per session (see `real_dir`).
    real_dirs: RwLock<HashMap<PathBuf, (u64, PathBuf, PathKind)>>,
}

impl Driver {
    pub fn new(cache: Cache) -> Self {
        Driver {
            cache,
            profiles: Mutex::default(),
            fingerprints: Mutex::default(),
            configs: Mutex::default(),
            pruned: AtomicU64::new(0),
            overlays: Mutex::default(),
            header_maps: Mutex::default(),
            real_dirs: RwLock::default(),
        }
    }

    /// A search directory's real path (compilers tell directories apart by
    /// what they are, not how they're spelled) and what it is, taken once
    /// per session; a path that doesn't exist is looked at again.
    fn real_dir(&self, p: &Path) -> (PathBuf, PathKind) {
        let session = self.cache.session();
        if let Some((s, real, kind)) = self.real_dirs.read().unwrap().get(p)
            && *s == session
        {
            return (real.clone(), *kind);
        }
        let (Ok(meta), Ok(real)) = (std::fs::metadata(p), std::fs::canonicalize(p)) else {
            return (p.to_path_buf(), PathKind::Missing);
        };
        let kind = if meta.is_dir() {
            PathKind::Dir
        } else if HeaderMap::is_header_map(p) {
            PathKind::HeaderMap
        } else {
            PathKind::File
        };
        self.real_dirs.write().unwrap().insert(p.to_path_buf(), (session, real.clone(), kind));
        (real, kind)
    }

    /// A directory's identity; None when it isn't one.
    fn dir_id(&self, d: &Dir) -> Option<DirId> {
        let (framework, p) = match d {
            Dir::Path(p) => (false, p),
            Dir::Framework(p) => (true, p),
        };
        let (real, kind) = self.real_dir(p);
        (kind == PathKind::Dir).then_some((framework, real))
    }

    /// A compiler's fingerprint, taken once per session.
    fn fingerprint(&self, inv: &Invocation, cwd: &Path, env: &Environment) -> String {
        let session = self.cache.session();
        let cell = {
            let mut fingerprints = self.fingerprints.lock().unwrap();
            let (s, cell) = fingerprints.entry(inv.compiler.clone()).or_insert_with(|| (session, Arc::default()));
            if *s != session {
                (*s, *cell) = (session, Arc::default());
            }
            cell.clone()
        };
        cell.get_or_init(|| probe::fingerprint(&env.compiler(inv, cwd))).clone()
    }

    /// What `digest` makes of `path`, once per session.
    fn digest_once(&self, path: &Path, digest: impl FnOnce() -> u64) -> u64 {
        let session = self.cache.session();
        if let Some((s, d)) = self.configs.lock().unwrap().get(path)
            && *s == session
        {
            return *d;
        }
        let d = digest();
        self.configs.lock().unwrap().insert(path.to_path_buf(), (session, d));
        d
    }

    /// The digest of the configuration files a compile's driver reads, whose
    /// contents its fingerprint doesn't show: Clang's beside it (`*.cfg`,
    /// read by default), those `--config` names (else searched for there),
    /// and GCC's `-specs=`.
    fn config_digest(&self, inv: &Invocation, clang: bool) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let contents = |p: &Path| {
            let mut h = DefaultHasher::new();
            std::fs::read(p).ok().hash(&mut h);
            h.finish()
        };
        let dir = inv.compiler.parent().filter(|d| !d.as_os_str().is_empty());
        let mut h = DefaultHasher::new();
        if clang && let Some(dir) = dir {
            self.digest_once(dir, || {
                let mut configs: Vec<PathBuf> = std::fs::read_dir(dir)
                    .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cfg"))).collect())
                    .unwrap_or_default();
                configs.sort();
                let mut h = DefaultHasher::new();
                for c in configs {
                    (c.file_name(), contents(&c)).hash(&mut h);
                }
                h.finish()
            })
            .hash(&mut h);
        }
        let args: Vec<&str> = inv.probe_args.iter().map(|a| a.strip_prefix("/clang:").unwrap_or(a)).collect();
        for (i, a) in args.iter().enumerate() {
            let named = if *a == "--config" { args.get(i + 1).copied() } else { ["--config=", "-specs=", "--specs="].iter().find_map(|p| a.strip_prefix(p)) };
            if let Some(f) = named {
                let f = Path::new(f);
                let path = match dir {
                    Some(dir) if f.components().count() == 1 => dir.join(f),
                    _ => f.to_path_buf(),
                };
                self.digest_once(&path, || contents(&path)).hash(&mut h);
            }
        }
        h.finish()
    }

    /// Drops what's kept for compilers and files not used lately (the
    /// daemon's driver lives for days): profiles probed over
    /// `PROFILE_LIFETIME` ago (probed again when used) or that failed in an
    /// earlier session, and what's taken once per session from earlier
    /// ones. At most once per session.
    fn prune(&self) {
        let session = self.cache.session();
        if self.pruned.swap(session, Ordering::Relaxed) == session {
            return;
        }
        let now = unix_secs(SystemTime::now());
        self.profiles.lock().unwrap().retain(|_, cell| match cell.get() {
            None => true,
            Some((_, Ok(p))) => now.saturating_sub(p.probed) <= PROFILE_LIFETIME.as_secs(),
            Some((s, Err(_))) => *s == session,
        });
        self.fingerprints.lock().unwrap().retain(|_, (s, _)| *s == session);
        self.configs.lock().unwrap().retain(|_, (s, _)| *s == session);
        self.overlays.lock().unwrap().retain(|_, cell| cell.get().is_none_or(|(s, _)| *s == session));
        self.header_maps.lock().unwrap().retain(|_, cell| cell.get().is_none_or(|(s, _)| *s == session));
        self.real_dirs.write().unwrap().retain(|_, (s, _, _)| *s == session);
    }

    /// The key of a compiler's profile for an invocation's flags: what
    /// may change its answers, the compiler's version included (its file,
    /// and its banner: a wrapper's file doesn't change with the compiler),
    /// and its configuration files. Not the working directory, which the
    /// probe's flags don't depend on (they name files by absolute paths:
    /// see `absolute_arg`), but for cl's relative `INCLUDE` directories.
    fn profile_key(&self, inv: &Invocation, cwd: &Path, env: &Environment) -> String {
        let fingerprint = self.fingerprint(inv, cwd, env);
        let configs = self.config_digest(inv, fingerprint.contains("clang"));
        let meta = std::fs::metadata(&inv.compiler).ok();
        let mtime = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        let vars: Vec<String> = PROFILE_VARS.iter().filter_map(|v| env.var(v).map(|val| format!("{v}={val}"))).collect();
        let relative_include =
            inv.style == Style::Msvc && env.var("INCLUDE").is_some_and(|v| v.split(';').any(|d| !d.is_empty() && Path::new(d).is_relative()));
        format!(
            "{}\u{1}{}\u{1}{mtime}\u{1}{:?}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{:016x}\u{1}{configs:016x}",
            inv.compiler.display(),
            meta.map_or(0, |m| m.len()),
            inv.style,
            inv.lang,
            if relative_include { cwd.display().to_string() } else { String::new() },
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
        let session = self.cache.session();
        let stale = |s: u64, loaded: &Result<Arc<Profile>, String>| match loaded {
            Ok(p) => unix_secs(SystemTime::now()).saturating_sub(p.probed) > PROFILE_LIFETIME.as_secs(),
            Err(_) => s != session,
        };
        load_once(&self.profiles, key.clone(), session, stale, || {
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
        let session = self.cache.session();
        load_once(&self.overlays, path.to_path_buf(), session, |s, _| s != session, || Overlay::load(path, cwd).map(Arc::new))
    }

    /// A search directory: a header map when the path is one.
    fn search_dir(&self, d: &Dir, cwd: &Path) -> Result<SearchDir, String> {
        let session = self.cache.session();
        Ok(match d {
            Dir::Framework(p) => SearchDir::Framework(p.clone()),
            Dir::Path(p) if self.real_dir(p).1 == PathKind::HeaderMap => {
                SearchDir::HeaderMap(load_once(&self.header_maps, p.clone(), session, |s, _| s != session, || HeaderMap::load(p, cwd).map(Arc::new))?)
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
        self.prune();
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
            relative_paths: inv.relative_paths,
            assembler: inv.lang == "assembler-with-cpp",
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
            if asked == 4 || profile.gave_up.load(Ordering::Relaxed) {
                return Ok(out);
            }
            asked += 1;
            let values = probe::answer(&env.compiler(&inv, cwd), &unknown).inspect_err(|_| {
                // Asked again, it would fail (or hang) again.
                profile.gave_up.store(true, Ordering::Relaxed);
            })?;
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
        // Directories whose name has `missing` don't exist.
        let id = |d: &Dir| match d {
            Dir::Path(p) if p.to_string_lossy().contains("missing") => None,
            Dir::Path(p) => Some((false, p.clone())),
            Dir::Framework(p) => Some((true, p.clone())),
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
        // GCC's last -iquote rule looks at the list as given: its last
        // directory, a duplicate, isn't the one dropped; nor is one followed
        // by a missing directory; the -I one it joins comes after missing
        // ones.
        let gcc = |quote: &[Dir], user: &[Dir]| search_lists(Merge::Gcc, quote, user, [&[d("/sys")], &[], &[]], id).0;
        assert_eq!(gcc(&[d("/a"), d("/a")], &[d("/a")]), vec![d("/a")]);
        assert_eq!(gcc(&[d("/a"), d("/missing")], &[d("/a")]), vec![d("/a")]);
        assert_eq!(gcc(&[d("/a")], &[d("/missing"), d("/a")]), Vec::<Dir>::new());
        // Clang keeps missing ones (which find nothing).
        assert_eq!(search_lists(Merge::Clang, &[], &[d("/missing"), d("/a")], [&[], &[], &[]], id).1, vec![d("/missing"), d("/a")]);
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
        assert_eq!(inv.system_dirs, vec![Dir::Path(abs("C:/ext"))]);
        assert_eq!(
            inv.commands,
            vec![CommandMacro::Define("A=1".into()), CommandMacro::Define("B=2".into()), CommandMacro::Define("C".into()), CommandMacro::Undef("D".into())]
        );
        assert_eq!(inv.forced, vec![Forced::Msvc("pre.h".into())]);
        assert!(inv.no_include_env && inv.conforming);
        // The probe passes the language itself. cl predefines the warning
        // level.
        assert_eq!(inv.probe_args, s(&["/external:W0", "/std:c++20", "/EHsc", "/MDd", "/Zc:preprocessor", "/W4"]));
        let clang_cl = parse(&s(&["clang-cl", "/clang:-ffile-prefix-map=/a=/b", "/vfsoverlay:o.yaml", "/std:c17", "/c", "--", "x.c"]), cwd).unwrap();
        assert_eq!(clang_cl.source, p("C:/b/x.c"));
        assert_eq!(clang_cl.style, Style::ClangCl);
        assert_eq!(clang_cl.lang, "c");
        assert_eq!(clang_cl.overlays, vec![p("C:/b/o.yaml")]);
        assert!(clang_cl.conforming);
        assert_eq!(clang_cl.probe_args, s(&["/std:c17"]));
        // clang-cl's -imsvc, which cl.exe doesn't have; its flags naming
        // the MSVC headers, joined or separate.
        let clang_cl =
            parse(&s(&["clang-cl", "-imsvc", "C:/sdk", "/winsysroot", "C:/ws", "-vctoolsdir", "C:/vc", "/winsdkdirC:/sdk", "-o", "x.obj", "/c", "x.cc"]), cwd)
                .unwrap();
        assert_eq!(clang_cl.system_dirs, vec![Dir::Path(abs("C:/sdk"))]);
        let absolute = |d: &str| abs(d).to_string_lossy().into_owned();
        assert_eq!(
            clang_cl.probe_args,
            s(&["/winsysroot", &absolute("C:/ws"), "-vctoolsdir", &absolute("C:/vc"), &format!("/winsdkdir{}", absolute("C:/sdk"))])
        );
        assert!(parse(&s(&["cl.exe", "-imsvc", "C:/sdk", "/c", "x.cc"]), cwd).is_err());
    }

    /// The whole driver against cl.exe: `CPP_DEPS_TEST_CL` (with INCLUDE
    /// set for its headers).
    #[test]
    #[cfg_attr(not(cpp_deps_test_cl), ignore = "needs CPP_DEPS_TEST_CL, a cl.exe with INCLUDE set")]
    fn real_cl() {
        let cl = crate::test_compiler("CPP_DEPS_TEST_CL");
        let dir = test_dirs::TestDir::new("cpp_deps_driver_cl");
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
    }

    #[test]
    fn profiles_round_trip() {
        let dir = test_dirs::base().join(format!("cpp_deps_profiles_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Profile::new(
            "#define __GNUC__ 13\n#define __cplusplus 201703L\n".into(),
            vec!["__has_include".into(), "__has_builtin".into()],
            crate::expr::Dialect::CXX,
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
        let dir = test_dirs::base().join(format!("cpp_deps_profiles_cleaned_{}", std::process::id()));
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
        // Loading one uses it: kept.
        p.save(&dir, "used").unwrap();
        let used = Profile::file(&dir, "used");
        age(&used, 31);
        assert!(Profile::load(&dir, "used").is_some());
        age(&dir.join(CLEANED), 2);
        p.save(&dir, "k").unwrap();
        assert!(used.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profiles_expire_in_memory() {
        let dir = test_dirs::TestDir::new("cpp_deps_profiles_expire");
        let argv = s(&[if cfg!(windows) { "C:/no/such/clang++.exe" } else { "/no/such/clang++" }, "-c", "a.cc"]);
        let (driver, env) = saved_profile(&argv, &dir, "#define __clang__ 1\n", vec![]);
        let inv = parse_in(&argv, &dir, &env).unwrap();
        let key = driver.profile_key(&inv, &dir, &env);
        // What the process holds in memory (`known`), when the profile
        // directory has a fresh profile.
        let held = |known: (u64, Result<Arc<Profile>, String>)| {
            let cell = Loaded::default();
            let _ = cell.set(known);
            driver.profiles.lock().unwrap().insert(key.clone(), cell);
            driver.profile(&inv, &dir, &env)
        };
        let session = driver.cache.session();
        // A profile probed over a day ago is probed again (here: loaded).
        let mut old = Profile::new("#define OLD 1\n".into(), vec![], crate::expr::Dialect::default(), false, vec![], vec![]);
        old.probed -= PROFILE_LIFETIME.as_secs() + 60;
        let fresh = held((session, Ok(Arc::new(old)))).unwrap();
        assert!(!fresh.predefined.contains_key(&intern::intern("OLD")));
        let recent = Profile::new("#define RECENT 1\n".into(), vec![], crate::expr::Dialect::default(), false, vec![], vec![]);
        assert!(held((session, Ok(Arc::new(recent)))).unwrap().predefined.contains_key(&intern::intern("RECENT")));
        // A probe that failed is tried again in the next session only.
        assert!(held((session, Err("failed".into()))).is_err());
        assert!(held((session - 1, Err("failed".into()))).is_ok());
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
            "-save-temps=obj",
            "-finput-charset=UTF-8",
            "-Xclang",
            "-fcolor-diagnostics",
            "--param=ssp-buffer-size=4",
            "--param",
            "max-inline-insns-single=100",
            "-fsanitize-ignorelist=ignore.txt",
            // CMake's for Clang targeting the MSVC ABI.
            "-Xclang",
            "--dependent-lib=msvcrt",
            "-Xclang",
            "-gcodeview",
            "-Xclang",
            "-fdebug-compilation-dir",
            "-Xclang",
            ".",
            "-Xclang",
            "-Wp,-DX",
            "-dM",
        ];
        assert_eq!(probe(&dropped), Vec::<String>::new());
        // What may change predefines, built-in directories or answers does.
        let probed = [
            "-O2",
            "-march=x86-64-v3",
            "-std=c++20",
            "-fno-exceptions",
            "-pthread",
            "--sysroot=/s",
            "-isysroot",
            "/sdk",
            "-nostdinc++",
            "-fms-compatibility-version=19.40",
            "--stdlib=libc++",
            "-pedantic",
            "-static",
            "-nostdlib",
            "-fprofile-use=/b/a.profdata",
            "-Xclang",
            "-target-feature",
            "-Xclang",
            "+avx2",
            "-Xclang",
            "-flto-visibility-public-std",
            "-disable-llvm-passes",
        ];
        // (Paths relative to the working directory made absolute; Windows'
        // without a drive are relative.)
        let host = |a: &str| if cfg!(windows) { a.replace("/s", "C:/s").replace("/b/a", "C:/b/a") } else { a.to_string() };
        let absolute = |a: &str| if cfg!(windows) { a.replace("C:/b/a", "C:\\b\\a") } else { a.to_string() };
        let cwd_args: Vec<String> = probed.iter().map(|a| host(a)).collect();
        let parsed_args = {
            let mut argv = vec!["clang++".to_string()];
            argv.extend(cwd_args.iter().map(|a| a.replace("/b/a.profdata", "a.profdata").replace("C:a.profdata", "a.profdata")));
            argv.extend(["-c".to_string(), "a.cc".to_string()]);
            parse(&argv, Path::new(if cfg!(windows) { "C:/b" } else { "/b" })).unwrap().probe_args
        };
        assert_eq!(parsed_args, cwd_args.iter().map(|a| absolute(a)).collect::<Vec<_>>());
        // Profiles made: their files don't make a profile per object.
        assert_eq!(
            probe(&["-fprofile-instr-generate=a.profraw", "-fprofile-generate=dir", "-fcs-profile-generate=dir"]),
            s(&["-fprofile-instr-generate", "-fprofile-generate", "-fcs-profile-generate"])
        );
        // What changes preprocessing otherwise falls back, as do flags no
        // list names.
        for bad in [
            &["-fmodules"][..],
            &["-fmodule-map-file=m.modulemap"],
            &["-fmodule-file=x.pcm"],
            &["-traditional-cpp"],
            &["-finput-charset=latin1"],
            &["-fno-dollars-in-identifiers"],
            &["-Wp,-include,a.h"],
            &["-Wp,-I-"],
            &["-Wp,-D"],
            &["-Xpreprocessor", "-DX"],
            &["-Xclang", "-I"],
            &["-Xclang", "-fms-compatibility"],
            &["-Xclang", "-load", "-Xclang", "plugin.so"],
            &["-iwithprefix", "x"],
            &["-iframeworkwithsysroot", "/fw"],
            &["-Apred=answer"],
            &["-fsycl"],
            &["--offload-arch=gfx90a"],
            &["-I-"],
            &["--unknown-flag"],
            &["-unknown"],
            // Resolved where the compiler runs (here for the probe).
            &["-march=native"],
            &["-mtune=native"],
            &["-mcpu=native"],
            &["-Xclang", "-target-cpu", "-Xclang", "native"],
        ] {
            assert!(parsed(bad).is_err(), "{bad:?}");
        }
        // -Wp, macros and directories, after the others (as GCC and Clang
        // take them).
        let inv = parsed(&["-Wp,-U_FORTIFY_SOURCE,-D_FORTIFY_SOURCE=3", "-DA=1", "-Wp,-DA=2", "-Iinc", "-Wp,-Iwp,-I,wp2", "-Ilast"]).unwrap();
        assert_eq!(
            inv.commands,
            vec![
                CommandMacro::Define("A=1".into()),
                CommandMacro::Undef("_FORTIFY_SOURCE".into()),
                CommandMacro::Define("_FORTIFY_SOURCE=3".into()),
                CommandMacro::Define("A=2".into())
            ]
        );
        assert_eq!(inv.user_dirs, ["inc", "last", "wp", "wp2"].map(|d| Dir::Path(scan::normalize(&cwd.join(d)))));
        assert!(inv.probe_args.is_empty());
        // Paths spelled relative, which `__FILE__` then is.
        let root = if cfg!(windows) { "C:/s" } else { "/s" };
        let relative = |argv: &[&str]| parse(&s(argv), cwd).unwrap().relative_paths;
        assert!(!relative(&["clang++", "-I", &format!("{root}/inc"), "-include", &format!("{root}/p.h"), "-c", &format!("{root}/a.cc")]));
        assert!(relative(&["clang++", "-c", "a.cc"]));
        assert!(relative(&["clang++", "-iquote", "q", "-c", &format!("{root}/a.cc")]));
        assert!(relative(&["clang++", "-include", "p.h", "-c", &format!("{root}/a.cc")]));
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

    /// Real compile commands: CMake's (GCC and Clang on Linux, Clang's
    /// GNU-style driver and clang-cl targeting the MSVC ABI, cl), the
    /// default flags of distributions' packages (Fedora, Arch, Debian,
    /// Ubuntu), Linux's, and hermetic-llvm's clang-cl. `{d}` is the source
    /// tree; the source is `a.c` or `a.cpp`, the object `a.o` or `a.obj`.
    const CORPUS: &[(&str, &str, &str)] = &[
        (
            "cmake-gcc",
            "/usr/bin/c++",
            "-DFOO=1 -I{d}/include -isystem {d}/third_party -O3 -DNDEBUG -std=gnu++17 -fPIC -fvisibility=hidden -fvisibility-inlines-hidden \
             -fdiagnostics-color=always -Winvalid-pch -include {d}/cmake_pch.hxx -MD -MT CMakeFiles/x.dir/a.cpp.o -MF CMakeFiles/x.dir/a.cpp.o.d \
             -o CMakeFiles/x.dir/a.cpp.o -c {d}/a.cpp",
        ),
        (
            "cmake-clang",
            "/usr/bin/clang++",
            "--target=x86_64-linux-gnu -I{d}/include -O2 -g -DNDEBUG -std=c++20 -fcolor-diagnostics -flto=thin -fPIE -pthread \
             -Xclang -include-pch -Xclang {d}/cmake_pch.hxx.pch -Xclang -include -Xclang {d}/cmake_pch.hxx \
             -MD -MT a.cpp.o -MF a.cpp.o.d -o a.cpp.o -c {d}/a.cpp",
        ),
        (
            "cmake-windows-clang",
            "C:/LLVM/bin/clang++.exe",
            "-DWIN32 -D_WINDOWS -I{d}/include -O0 -g -Xclang -gcodeview -D_DEBUG -D_DLL -D_MT -Xclang --dependent-lib=msvcrtd -std=gnu++17 \
             -MD -MT CMakeFiles/x.dir/a.cpp.obj -MF CMakeFiles/x.dir/a.cpp.obj.d -o CMakeFiles/x.dir/a.cpp.obj -c {d}/a.cpp",
        ),
        (
            "cmake-windows-clang-static",
            "C:/LLVM/bin/clang.exe",
            "-I{d}/include -O3 -DNDEBUG -Xclang -flto-visibility-public-std -D_MT -Xclang --dependent-lib=libcmt -std=gnu11 \
             -MD -MT a.c.obj -MF a.c.obj.d -o a.c.obj -c {d}/a.c",
        ),
        (
            "cmake-msvc",
            "C:/VS/bin/cl.exe",
            "/nologo /TP -DFOO -I{d}/include /external:I{d}/third_party /external:W0 /DWIN32 /D_WINDOWS /GR /EHsc /Zi /Ob0 /Od /RTC1 \
             -std:c++17 -MDd /showIncludes /FoCMakeFiles/x.dir/a.cpp.obj /FdCMakeFiles/x.dir/ /FS -c {d}/a.cpp",
        ),
        (
            "cmake-clang-cl",
            "C:/LLVM/bin/clang-cl.exe",
            "/nologo -TP -DFOO -I{d}/include -imsvc{d}/third_party /DWIN32 /D_WINDOWS /GR /EHsc /Zi /Ob0 /Od /RTC1 -std:c++17 -MDd \
             /showIncludes /FoCMakeFiles/x.dir/a.cpp.obj /FdTARGET_COMPILE_PDB -c -- {d}/a.cpp",
        ),
        (
            "hermetic-clang-cl",
            "C:/hermetic/bin/clang-cl.exe",
            "--target=x86_64-pc-windows-msvc /nologo -TP -DGREETER_STATIC -I{d}/include /FI{d}/include/__hermetic_llvm_libcxx_link.h \
             /D_LIBCPP_NO_AUTO_LINK /imsvc{d}/libcxx/include/c++/v1 -Wno-builtin-macro-redefined -D__DATE__=\"redacted\" \
             -D__TIMESTAMP__=\"redacted\" -D__TIME__=\"redacted\" /clang:-ffile-prefix-map={d}=/hermetic-llvm/cache -Xclang -object-file-name=- \
             -resource-dir={d}/resource /D_CRT_STDIO_ISO_WIDE_SPECIFIERS -nobuiltininc /imsvc{d}/resource/include -fms-compatibility-version=19.50.35717 \
             /vctoolsdir{d}/vc /winsdkdir{d}/sdk /winsdkversion10.0.26100.0 -Xclang -ivfsoverlay -Xclang {d}/overlay.yaml /Brepro \
             /clang:-gno-codeview-command-line /DWIN32 /D_WINDOWS /GR /EHsc /O2 /Ob2 /DNDEBUG -std:c++17 -MD -clang:-MD -clang:-MTa.cpp.obj \
             -clang:-MFa.cpp.obj.d /Foa.cpp.obj /Fdx.pdb -c -- {d}/a.cpp",
        ),
        (
            "fedora",
            "/usr/bin/g++",
            "-O2 -flto=auto -ffat-lto-objects -fexceptions -g -grecord-gcc-switches -pipe -Wall -Werror=format-security \
             -Wp,-U_FORTIFY_SOURCE,-D_FORTIFY_SOURCE=3 -Wp,-D_GLIBCXX_ASSERTIONS -specs=/usr/lib/rpm/redhat/redhat-hardened-cc1 \
             -fstack-protector-strong -specs=/usr/lib/rpm/redhat/redhat-annobin-cc1 -m64 -march=x86-64 -mtune=generic \
             -fasynchronous-unwind-tables -fstack-clash-protection -fcf-protection -mtls-dialect=gnu2 -fno-omit-frame-pointer \
             -mno-omit-leaf-frame-pointer -I{d}/include -c {d}/a.cpp -o a.o",
        ),
        (
            "arch",
            "/usr/bin/g++",
            "-march=x86-64 -mtune=generic -O2 -pipe -fno-plt -fexceptions -Wp,-D_FORTIFY_SOURCE=3 -Wformat -Werror=format-security \
             -fstack-clash-protection -fcf-protection -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer -Wp,-D_GLIBCXX_ASSERTIONS -g \
             -ffile-prefix-map=/build/pkg/src=/usr/src/debug/pkg -flto=auto -I{d}/include -c {d}/a.cpp -o a.o",
        ),
        (
            "debian",
            "/usr/bin/gcc",
            "-g -O2 -ffile-prefix-map=/build/pkg-1.0=. -fstack-protector-strong -fstack-clash-protection -Wformat -Werror=format-security \
             -fcf-protection -Wdate-time -D_FORTIFY_SOURCE=2 -I{d}/include -c {d}/a.c -o a.o",
        ),
        (
            "ubuntu",
            "/usr/bin/gcc",
            "-g -O2 -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer -ffile-prefix-map=/build/pkg-1.0=. -flto=auto -ffat-lto-objects \
             -fstack-protector-strong -fstack-clash-protection -Wformat -Werror=format-security -fcf-protection \
             -fdebug-prefix-map=/build/pkg-1.0=/usr/src/pkg-1.0 -Wdate-time -D_FORTIFY_SOURCE=3 -I{d}/include -c {d}/a.c -o a.o",
        ),
        (
            "linux",
            "/usr/bin/gcc",
            "-Wp,-MMD,dir/.a.o.d -nostdinc -I{d}/include -include {d}/include/kconfig.h -D__KERNEL__ -fmacro-prefix-map=./= -std=gnu11 \
             -fshort-wchar -funsigned-char -fno-common -fno-PIE -fno-strict-aliasing -mno-sse -mno-mmx -mno-sse2 -mno-3dnow -mno-avx \
             -fcf-protection=branch -fno-jump-tables -m64 -falign-jumps=1 -falign-loops=1 -mno-80387 -mno-fp-ret-in-387 \
             -mpreferred-stack-boundary=3 -mskip-rax-setup -mtune=generic -mno-red-zone -mcmodel=kernel -Wno-sign-compare \
             -fno-asynchronous-unwind-tables -mindirect-branch=thunk-extern -mindirect-branch-register -mfunction-return=thunk-extern \
             -fpatchable-function-entry=16,16 -fno-delete-null-pointer-checks -O2 -fno-allow-store-data-races -fstack-protector-strong \
             -fno-omit-frame-pointer -fno-optimize-sibling-calls -ftrivial-auto-var-init=zero -fno-stack-clash-protection -pg -mrecord-mcount \
             -mfentry -DCC_USING_FENTRY -falign-functions=16 -fno-strict-overflow -fno-stack-check -fconserve-stack -Wall \
             -DKBUILD_MODFILE=\"dir/a\" -DKBUILD_BASENAME=\"a\" -DKBUILD_MODNAME=\"a\" -D__KBUILD_MODNAME=kmod_a -c -o dir/a.o {d}/a.c",
        ),
    ];

    /// What `compiler` predefines with `args` (`extra` printing them), in
    /// `cwd`, sorted; None when it fails.
    fn predefines(compiler: &Path, style: Style, lang: &str, args: &[&str], extra: &[&str], cwd: &Path) -> Option<Vec<String>> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let c = probe::Compiler { path: compiler, args: &args, style, lang, cwd, env: None, timeout: probe::TIMEOUT };
        let mut lines: Vec<String> = probe::run(&c, extra, "").ok()?.stdout.lines().filter(|l| l.starts_with("#define")).map(String::from).collect();
        lines.sort();
        Some(lines)
    }

    /// What the probe doesn't have doesn't change what compilers predefine:
    /// `DROPPED` and samples of the prefixes', against `CPP_DEPS_TEST_CXX`
    /// (a Clang for a few targets too).
    #[test]
    #[cfg_attr(not(cpp_deps_test_cxx), ignore = "needs CPP_DEPS_TEST_CXX, a GCC or Clang")]
    fn dropped_flags_keep_predefines() {
        let cwd = test_dirs::TestDir::new("cpp_deps_dropped");
        let predefines = |compiler: &Path, style: Style, lang: &str, args: &[&str], extra: &[&str]| predefines(compiler, style, lang, args, extra, &cwd);
        let cxx = crate::test_compiler("CPP_DEPS_TEST_CXX");
        // What -E prints and how is the probe's business.
        let printing = ["-###", "-E", "-P", "-C", "-CC", "-H", "-v", "--verbose", "-time", "-dM", "-dD", "-dN", "-dI", "-dU"];
        let samples = [
            "-Werror",
            "-Wall",
            "-Wextra",
            "-g",
            "-g3",
            "-ggdb",
            "-gline-tables-only",
            "-fdebug-prefix-map=/a=/b",
            "-ffile-prefix-map=/a=/b",
            "-fmacro-prefix-map=/a=/b",
            "-fdiagnostics-color=always",
            "-fmessage-length=0",
            "-ferror-limit=5",
            "-fmax-errors=5",
            "-ftemplate-depth=100",
            "-fconstexpr-depth=100",
            "-fbracket-depth=300",
            "-fuse-ld=lld",
            "-rtlib=compiler-rt",
            "-L/x",
            "-lm",
            "-Wl,-x",
            "-Wa,-x",
            "-frandom-seed=x",
            "--param=ssp-buffer-size=4",
            "-fprofile-update=atomic",
            "-fprofile-exclude-files=x",
            "-fprofile-dir=x",
            "-fpch-preprocess",
            "-ftime-report",
            "-fno-crash-diagnostics",
        ];
        let flags: Vec<&str> = DROPPED.iter().copied().filter(|f| !printing.contains(f)).chain(samples).collect();
        assert!(flags.iter().all(|f| gcc_effect(f) == Effect::Drop));
        let clang = probe::fingerprint(&probe::Compiler { path: &cxx, args: &[], style: Style::Gcc, lang: "c", cwd: &cwd, env: None, timeout: probe::TIMEOUT })
            .contains("clang");
        let targets: &[&[&str]] = if clang { &[&[], &["--target=x86_64-linux-gnu"], &["--target=arm64-apple-macos14"]] } else { &[&[]] };
        for lang in ["c", "c++"] {
            for target in targets {
                let base = predefines(&cxx, Style::Gcc, lang, target, &["-dM", "-E"]).unwrap();
                for f in &flags {
                    let args: Vec<&str> = target.iter().copied().chain([*f]).collect();
                    if let Some(with) = predefines(&cxx, Style::Gcc, lang, &args, &["-dM", "-E"]) {
                        assert_eq!(with, base, "{f} changes what {} predefines ({lang}, {target:?})", cxx.display());
                    }
                }
            }
        }
    }

    /// The same for cl.exe's flags, against `CPP_DEPS_TEST_CL`.
    #[test]
    #[cfg_attr(not(cpp_deps_test_cl), ignore = "needs CPP_DEPS_TEST_CL, a cl.exe with INCLUDE set")]
    fn dropped_cl_flags_keep_predefines() {
        let cwd = test_dirs::TestDir::new("cpp_deps_dropped_cl");
        let predefines = |compiler: &Path, style: Style, lang: &str, args: &[&str], extra: &[&str]| predefines(compiler, style, lang, args, extra, &cwd);
        let cl = crate::test_compiler("CPP_DEPS_TEST_CL");
        let flags = [
            "/Zi",
            "/Z7",
            "/FS",
            "/MP",
            "/WX",
            "/Wv:19",
            "/wd4996",
            "/we4996",
            "/w14996",
            "/Gy",
            "/Gw",
            "/Brepro",
            "/diagnostics:caret",
            "/external:templates-",
            "/errorReport:none",
            "/FoC:/x.obj",
            "/FdC:/x.pdb",
            "/c",
            "/showIncludes",
        ];
        assert!(flags.iter().all(|f| cl_dropped(&f[1..])));
        let pd = ["/EP", "/Zc:preprocessor", "/PD"];
        let base = predefines(&cl, Style::Msvc, "c++", &[], &pd).unwrap();
        for f in flags {
            if let Some(with) = predefines(&cl, Style::Msvc, "c++", &[f], &pd) {
                assert_eq!(with, base, "{f} changes what cl predefines");
            }
        }
    }

    /// Every command of `CORPUS` scans exactly (with a profile saved
    /// beforehand: no compiler runs), and its object's name doesn't reach
    /// the probe (one profile for every object).
    #[test]
    fn real_command_lines() {
        let dir = test_dirs::TestDir::new("cpp_deps_corpus");
        let files = [
            ("a.cpp", "#include <x.h>\n#include \"y.h\"\n"),
            ("a.c", "#include <x.h>\n#include \"y.h\"\n"),
            ("y.h", ""),
            ("include/x.h", ""),
            ("include/kconfig.h", ""),
            ("cmake_pch.hxx", ""),
            ("include/__hermetic_llvm_libcxx_link.h", ""),
            ("overlay.yaml", "{ 'version': 0, 'roots': [] }\n"),
        ];
        for (f, text) in files {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), text).unwrap();
        }
        let d = dir.to_string_lossy().replace('\\', "/");
        for &(name, compiler, args) in CORPUS {
            let argv = |object: &str| -> Vec<String> {
                std::iter::once(compiler.to_string())
                    .chain(
                        args.split_whitespace()
                            .map(|a| a.replace("{d}", &d).replace("a.o", &format!("{object}.o")).replace("a.cpp.o", &format!("{object}.cpp.o"))),
                    )
                    .collect()
            };
            let inv = parse_in(&argv("a"), &dir, &Environment::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
            let other = parse_in(&argv("b"), &dir, &Environment::default()).unwrap();
            assert_eq!(inv.probe_args, other.probe_args, "{name}");
            let (driver, env) = saved_profile(&argv("a"), &dir, "#define __GNUC__ 13\n", vec![]);
            let out = driver.scan_in(&argv("a"), &dir, &env).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(out.exact() && out.missing.is_empty(), "{name}: {:?} {:?}", out.inexact, out.missing);
            assert!(out.files.iter().any(|f| f.ends_with("x.h")) && out.files.iter().any(|f| f.ends_with("y.h")), "{name}: {:?}", out.files);
        }
    }

    #[test]
    fn files_flags_read() {
        let dir = test_dirs::TestDir::new("cpp_deps_read_paths");
        let files = [
            "prof.afdo",
            "use.profdata",
            "cfg.cfg",
            "allow.txt",
            "ignore.txt",
            "list.txt",
            "xray.txt",
            "remap.txt",
            "o.yaml",
            "o2.yaml",
            "pch.pch",
            "m.pcm",
            "x.hmap",
            "spec.specs",
            "gcda/a.gcda",
        ];
        for f in files {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "").unwrap();
        }
        let read = |argv: &[&str]| {
            let (files, dirs) = read_paths(&s(argv), &dir);
            let rel = |v: Vec<PathBuf>| v.iter().map(|f| f.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();
            (rel(files), rel(dirs))
        };
        let (files, dirs) = read(&[
            "clang++",
            "-fprofile-sample-use=prof.afdo",
            "-fprofile-instr-use=use.profdata",
            "--config=cfg.cfg",
            "-fsanitize-coverage-allowlist=allow.txt",
            "-fsanitize-ignorelist=ignore.txt",
            "-fprofile-list=list.txt",
            "-fxray-always-instrument=xray.txt",
            "-fprofile-remapping-file=remap.txt",
            "-ivfsoverlay=o.yaml",
            "-Xclang",
            "-ivfsoverlay",
            "-Xclang",
            "o2.yaml",
            "-Xclang",
            "-include-pch",
            "-Xclang",
            "pch.pch",
            "-fmodule-file=m=m.pcm",
            "-I",
            "x.hmap",
            "-Iinc",
            "-specs=spec.specs",
            "-fprofile-use=gcda",
            "-fprofile-list=missing.txt",
            "-c",
            "a.cc",
        ]);
        assert_eq!(
            files,
            vec![
                "prof.afdo",
                "use.profdata",
                "cfg.cfg",
                "allow.txt",
                "ignore.txt",
                "list.txt",
                "xray.txt",
                "remap.txt",
                "o.yaml",
                "o2.yaml",
                "pch.pch",
                "m.pcm",
                "x.hmap",
                "spec.specs"
            ]
        );
        assert_eq!(dirs, vec!["gcda"]);
        // clang-cl's, through /clang: or not.
        let (files, _) = read(&["clang-cl", "/clang:-fprofile-use=use.profdata", "-fsanitize-ignorelist=ignore.txt", "/c", "a.cc"]);
        assert_eq!(files, vec!["use.profdata", "ignore.txt"]);
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
            &["/clang:-march=native"],
            &["/clang:-Wp,-include,a.h"],
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
        // The driver's -Wp, through /clang: (clang-cl's own -Wp... is a
        // warning flag).
        let inv = parsed(&["clang-cl", "/clang:-Wp,-DFOO", "-Wp,-DBAR", "/DA", "/c", "x.cc"], &env(&[])).unwrap();
        assert_eq!(inv.commands, vec![CommandMacro::Define("A".into()), CommandMacro::Define("FOO".into())]);
        // cl.exe ignores clang-cl's /clang: and -Xclang (not what follows).
        let inv = parsed(&["cl.exe", "/clang:-DFOO", "/clang:-Iinc", "-Xclang", "-DBAR", "/c", "x.cc"], &env(&[])).unwrap();
        assert_eq!(inv.commands, vec![CommandMacro::Define("BAR".into())]);
        // Profiles made, without their files.
        let inv = parsed(&["clang-cl", "/clang:-fprofile-instr-generate=x.profraw", "/c", "x.cc"], &env(&[])).unwrap();
        assert_eq!(inv.probe_args, s(&["/clang:-fprofile-instr-generate"]));
        // Warning levels, which cl predefines and clang-cl doesn't.
        let levels = ["/W4", "-W3", "/Wall", "/w", "/external:W0"];
        let probe = |compiler: &str| parsed(&[&[compiler][..], &levels, &["/c", "x.cc"]].concat(), &env(&[])).unwrap().probe_args;
        assert_eq!(probe("clang-cl"), Vec::<String>::new());
        assert_eq!(probe("cl.exe"), s(&levels));
    }

    /// `=` at the start of a directory stands for the sysroot (the last
    /// -isysroot, else --sysroot); without one, or with GCC's `$SYSROOT`,
    /// GCC and Clang differ: unsupported.
    #[test]
    fn sysroot_relative_dirs() {
        let cwd = Path::new(if cfg!(windows) { "C:/b" } else { "/b" });
        let dirs = |argv: &[&str]| parse(&s(argv), cwd).map(|inv| (inv.user_dirs, inv.system_dirs, inv.quote_dirs, inv.after_dirs));
        let p = |d: &str| vec![Dir::Path(scan::normalize(&cwd.join(d)))];
        let all = dirs(&["cc", "--sysroot=sr", "-I=/inc", "-isystem=/sys", "-iquote", "=/q", "-idirafter=/after", "-c", "a.c"]).unwrap();
        assert_eq!(all, (p("sr/inc"), p("sr/sys"), p("sr/q"), p("sr/after")));
        assert_eq!(dirs(&["cc", "--sysroot", "sr", "-isysroot", "isr", "-I=/inc", "-c", "a.c"]).unwrap().0, p("isr/inc"));
        assert_eq!(dirs(&["cc", "-I", "=/inc", "-isysroot", "isr", "-c", "a.c"]).unwrap().0, p("isr/inc"));
        assert_eq!(dirs(&["cc", "-Wp,-I=/inc", "--sysroot=sr", "-c", "a.c"]).unwrap().0, p("sr/inc"));
        for bad in [
            &["cc", "-I=/inc", "-c", "a.c"][..],
            &["cc", "--sysroot=sr", "-I=inc", "-c", "a.c"],
            &["cc", "-I$SYSROOT/inc", "-c", "a.c"],
            &["cc", "--sysroot=sr", "-F=/fw", "-c", "a.c"],
        ] {
            assert!(dirs(bad).is_err(), "{bad:?}");
        }
    }

    /// Profiles don't depend on the working directory (CMake's
    /// `try_compile` makes one per check): the probe's flags name files by
    /// absolute paths.
    #[test]
    fn profiles_shared_by_working_directories() {
        let root = test_dirs::TestDir::new("cpp_deps_profile_cwd");
        let (a, b) = (root.join("a"), root.join("b"));
        let compiler = if cfg!(windows) { "C:/no/such/clang++.exe" } else { "/no/such/clang++" };
        let driver = Driver::new(Cache::new(vec![], false));
        let env = Environment { vars: Some(Arc::new(vec![])), profile_dir: None };
        let key = |cwd: &Path, flags: &[&str]| {
            let argv: Vec<String> = std::iter::once(compiler).chain(flags.iter().copied()).chain(["-c", "x.cc"]).map(String::from).collect();
            driver.profile_key(&parse_in(&argv, cwd, &env).unwrap(), cwd, &env)
        };
        assert_eq!(key(&a, &["-O2", "-std=c++20"]), key(&b, &["-O2", "-std=c++20"]));
        // The same sysroot from both, and different ones.
        assert_eq!(key(&a, &["--sysroot=../sr", "-isysroot", "../sdk"]), key(&b, &["--sysroot=../sr", "-isysroot", "../sdk"]));
        assert_ne!(key(&a, &["--sysroot=sr"]), key(&b, &["--sysroot=sr"]));
        // A compiler named relative to it: another one in each.
        let relative = |cwd: &Path| parse_in(&s(&["tools/clang++", "-c", "x.cc"]), cwd, &env).unwrap();
        assert_eq!(relative(&a).compiler, scan::normalize(&a.join("tools/clang++")));
        assert_ne!(driver.profile_key(&relative(&a), &a, &env), driver.profile_key(&relative(&b), &b, &env));
    }

    /// A fake Clang in `dir` (`clang++`): it notes its runs in `runs.txt`
    /// and takes a moment to tell its version.
    fn fake_clang(dir: &Path) -> PathBuf {
        if cfg!(windows) {
            let c = dir.join("clang++.cmd");
            std::fs::write(&c, "@echo run>>\"%~dp0runs.txt\"\r\n@ping -n 2 127.0.0.1 >NUL\r\n@echo clang version 99\r\n").unwrap();
            c
        } else {
            let c = dir.join("clang++");
            std::fs::write(&c, "#!/bin/sh\necho run >> \"$(dirname \"$0\")/runs.txt\"\nsleep 0.5\necho clang version 99\n").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&c, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            c
        }
    }

    /// Concurrent scans take a compiler's fingerprint once.
    #[test]
    fn fingerprints_taken_once() {
        let dir = test_dirs::TestDir::new("cpp_deps_fingerprint_once");
        let compiler = fake_clang(&dir);
        let driver = Driver::new(Cache::new(vec![], false));
        let env = Environment::default();
        let inv = parse_in(&s(&[&compiler.to_string_lossy(), "-c", "a.cc"]), &dir, &env).unwrap();
        let prints: Vec<String> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4).map(|_| scope.spawn(|| driver.fingerprint(&inv, &dir, &env))).collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        });
        assert!(prints.iter().all(|p| p.contains("clang version 99")), "{prints:?}");
        assert_eq!(std::fs::read_to_string(dir.join("runs.txt")).unwrap().lines().count(), 1);
    }

    /// Clang's configuration files beside it and those `--config` names
    /// are part of profiles' keys: their contents change what it
    /// predefines.
    #[test]
    fn config_files_in_profile_keys() {
        let dir = test_dirs::TestDir::new("cpp_deps_config_keys");
        let compiler = fake_clang(&dir);
        std::fs::write(dir.join("clang++.cfg"), "-DX=1\n").unwrap();
        std::fs::create_dir_all(dir.join("cfgs")).unwrap();
        std::fs::write(dir.join("cfgs/extra.cfg"), "-DY=1\n").unwrap();
        let driver = Driver::new(Cache::new(vec![], false));
        let env = Environment { vars: Some(Arc::new(vec![])), profile_dir: None };
        let key = |flags: &[&str]| {
            let argv: Vec<String> =
                [&compiler.to_string_lossy()].into_iter().map(|c| c.to_string()).chain(flags.iter().map(|f| f.to_string())).chain(s(&["-c", "a.cc"])).collect();
            driver.profile_key(&parse_in(&argv, &dir, &env).unwrap(), &dir, &env)
        };
        let (plain, extra) = (key(&[]), key(&["--config", "cfgs/extra.cfg"]));
        assert_ne!(plain, extra);
        std::fs::write(dir.join("clang++.cfg"), "-DX=2\n").unwrap();
        // (Read once per session.)
        assert_eq!(key(&[]), plain);
        driver.cache.new_session();
        let plain2 = key(&[]);
        assert_ne!(plain2, plain);
        let extra2 = key(&["--config", "cfgs/extra.cfg"]);
        std::fs::write(dir.join("cfgs/extra.cfg"), "-DY=2\n").unwrap();
        driver.cache.new_session();
        assert_eq!(key(&[]), plain2);
        assert_ne!(key(&["--config", "cfgs/extra.cfg"]), extra2);
    }

    /// A compiler that can't answer queries isn't asked again by every scan
    /// (each could wait for `probe::TIMEOUT`): they stay inexact.
    #[test]
    fn asking_gives_up() {
        let dir = test_dirs::TestDir::new("cpp_deps_asking");
        std::fs::write(dir.join("a.cc"), "#if __has_builtin(__builtin_x)\n#endif\n").unwrap();
        let argv = s(&[if cfg!(windows) { "C:/no/such/clang++.exe" } else { "/no/such/clang++" }, "-c", "a.cc"]);
        let driver = Driver::new(Cache::new(vec![], false));
        let env = Environment { vars: Some(Arc::new(vec![])), profile_dir: Some(dir.join("profiles")) };
        let key = driver.profile_key(&parse_in(&argv, &dir, &env).unwrap(), &dir, &env);
        let operators = vec!["__has_include".into(), "__has_builtin".into()];
        Profile::new("#define __clang__ 1\n".into(), operators, crate::expr::Dialect::default(), false, vec![], vec![])
            .save(&dir.join("profiles"), &key)
            .unwrap();
        assert!(driver.scan_in(&argv, &dir, &env).is_err());
        let out = driver.scan_in(&argv, &dir, &env).unwrap();
        assert_eq!(out.unknown, vec![("__has_builtin".to_string(), "__builtin_x".to_string())]);
    }

    /// The driver forgets what it keeps for compilers and files no session
    /// used since (the daemon's lives for days), and profiles probed too
    /// long ago.
    #[test]
    fn driver_forgets_unused() {
        let dir = test_dirs::TestDir::new("cpp_deps_prune");
        std::fs::write(dir.join("a.cc"), "").unwrap();
        let argv = s(&[if cfg!(windows) { "C:/no/such/clang++.exe" } else { "/no/such/clang++" }, "-c", "a.cc"]);
        let (driver, env) = saved_profile(&argv, &dir, "#define __clang__ 1\n", vec![]);
        driver.scan_in(&argv, &dir, &env).unwrap();
        let mut old = Profile::new("#define OLD 1\n".into(), vec![], crate::expr::Dialect::default(), false, vec![], vec![]);
        old.probed -= PROFILE_LIFETIME.as_secs() + 60;
        let cell = Loaded::default();
        let _ = cell.set((driver.cache.session(), Ok(Arc::new(old))));
        driver.profiles.lock().unwrap().insert("old".into(), cell);
        driver.real_dirs.write().unwrap().insert(dir.join("gone"), (driver.cache.session(), dir.join("gone"), PathKind::Missing));
        assert_eq!((driver.profiles.lock().unwrap().len(), driver.fingerprints.lock().unwrap().len()), (2, 1));
        driver.cache.new_session();
        driver.scan_in(&argv, &dir, &env).unwrap();
        assert_eq!(driver.profiles.lock().unwrap().len(), 1);
        assert!(!driver.real_dirs.read().unwrap().contains_key(&dir.join("gone")));
    }

    /// Files flags make the compile read that no scan finds: GCC's profiles
    /// beside the object, its AutoFDO profile, basic block section lists,
    /// cl's modules.
    #[test]
    fn more_files_flags_read() {
        let dir = test_dirs::TestDir::new("cpp_deps_read_paths_more");
        for f in ["out/a.gcda", "a.gcda", "fbdata.afdo", "bbs.txt", "m.ifc", "ifcs/x.ifc"] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "").unwrap();
        }
        let read = |argv: &[&str]| {
            let (files, dirs) = read_paths(&s(argv), &dir);
            let rel = |v: Vec<PathBuf>| v.iter().map(|f| f.strip_prefix(&*dir).unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>();
            (rel(files), rel(dirs))
        };
        assert_eq!(read(&["g++", "-fprofile-use", "-c", "a.cc", "-o", "out/a.o"]).0, ["out/a.gcda"]);
        assert_eq!(read(&["g++", "-fbranch-probabilities", "-c", "a.cc"]).0, ["a.gcda"]);
        assert_eq!(read(&["g++", "-fauto-profile", "-fbasic-block-sections=list=bbs.txt", "-c", "a.cc"]).0, ["fbdata.afdo", "bbs.txt"]);
        assert_eq!(read(&["cl.exe", "/reference", "m=m.ifc", "/ifcSearchDir", "ifcs", "/c", "a.cc"]), (vec!["m.ifc".to_string()], vec!["ifcs".to_string()]));
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
        let dir = test_dirs::base().join(format!("cpp_deps_driver_lookups_{}", std::process::id()));
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
        // clang-cl merges as Clang does: an /I directory that's a built-in
        // one (INCLUDE's) is searched there, after the others; cl searches
        // it first.
        std::fs::write(dir.join("cl.cc"), "#include <x.h>\n").unwrap();
        std::fs::write(dir.join("other/x.h"), "").unwrap();
        let cl_scanned = |compiler: &str, predefines: &str| {
            let compiler = if cfg!(windows) { format!("C:/no/such/{compiler}.exe") } else { format!("/no/such/{compiler}") };
            let argv = s(&[&compiler, "/Isys", "/Iother", "/c", "cl.cc"]);
            let (driver, env) = saved_profile(&argv, &dir, predefines, vec![Dir::Path(dir.join("sys"))]);
            let out = driver.scan_in(&argv, &dir, &env).unwrap();
            out.files.iter().map(|f| f.strip_prefix(&dir).unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>()
        };
        assert_eq!(cl_scanned("clang-cl", clang_msvc), vec!["cl.cc", "other/x.h"]);
        assert_eq!(cl_scanned("cl", "#define _MSC_VER 1944\n"), vec!["cl.cc", "sys/x.h"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_dirs_once_per_session() {
        let dir = test_dirs::base().join(format!("cpp_deps_real_dirs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        let driver = Driver::new(Cache::new(vec![], false));
        let real_a = std::fs::canonicalize(&a).unwrap();
        assert_eq!(driver.real_dir(&a), (real_a.clone(), PathKind::Dir));
        // Taken once per session (scans don't canonicalize every pair).
        std::fs::remove_dir(&a).unwrap();
        assert_eq!(driver.real_dir(&a), (real_a, PathKind::Dir));
        driver.cache.new_session();
        assert_eq!(driver.real_dir(&a), (a, PathKind::Missing));
        // A directory that doesn't exist is looked at again.
        assert_eq!(driver.real_dir(&b), (b.clone(), PathKind::Missing));
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(driver.real_dir(&b), (std::fs::canonicalize(&b).unwrap(), PathKind::Dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A differential case: its files, and the arguments after the
    /// compiler (`{d}` is its directory).
    type Case = (&'static str, &'static [(&'static str, &'static str)], &'static [&'static str]);

    /// A differential case with its parts owned.
    type OwnedCase = (String, Vec<(String, String)>, Vec<String>);

    /// Scans each case and checks it against the compiler's own list of the
    /// files it reads: a scan that says it's exact names the same files
    /// (but those `__has_include` found, which GCC doesn't list). Returns
    /// the cases scanned exactly.
    fn differential(compiler: &str, cases: &[Case]) -> Vec<&'static str> {
        let owned: Vec<OwnedCase> = cases
            .iter()
            .map(|(name, files, args)| {
                (name.to_string(), files.iter().map(|(f, t)| (f.to_string(), t.to_string())).collect(), args.iter().map(|a| a.to_string()).collect())
            })
            .collect();
        let exact = differential_cases(compiler, &owned);
        cases.iter().map(|c| c.0).filter(|name| exact.iter().any(|e| e == name)).collect()
    }

    fn differential_cases(compiler: &str, cases: &[OwnedCase]) -> Vec<String> {
        use std::collections::BTreeSet;
        let root = test_dirs::TestDir::new(&format!("cpp_deps_differential_{}", cases[0].0));
        let driver = Driver::new(Cache::new(vec![], cfg!(windows)));
        let key = |p: &PathBuf| {
            let s = scan::normalize(p).to_string_lossy().into_owned();
            if cfg!(windows) { s.replace('\\', "/").to_lowercase() } else { s }
        };
        let mut exact = Vec::new();
        for (name, files, args) in cases {
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
            exact.push(name.clone());
        }
        exact
    }

    /// Real libraries' headers, whose configuration macros run pragmas
    /// through macros (libc++'s diagnostics, Boost's messages, GLib's
    /// deprecations), scanned exactly and as the compiler reads them: with
    /// `CPP_DEPS_TEST_CXX`, libc++ (`CPP_DEPS_TEST_LIBCXX`, its include
    /// directory), Boost (`CPP_DEPS_TEST_BOOST`, the directory holding
    /// `boost/`) and GLib (`CPP_DEPS_TEST_GLIB`, its flags), each when set.
    #[test]
    #[cfg_attr(
        not(all(cpp_deps_test_cxx, any(cpp_deps_test_libcxx, cpp_deps_test_boost, cpp_deps_test_glib))),
        ignore = "needs CPP_DEPS_TEST_CXX, and CPP_DEPS_TEST_LIBCXX, CPP_DEPS_TEST_BOOST or CPP_DEPS_TEST_GLIB"
    )]
    fn real_library_headers() {
        let cxx = crate::test_compiler("CPP_DEPS_TEST_CXX");
        let var = |v: &str| std::env::var(v).ok().filter(|v| !v.is_empty());
        let case = |name: &str, source: &str, text: &str, flags: Vec<String>| -> OwnedCase {
            let args = flags.into_iter().chain(["-c".to_string(), source.to_string()]).collect();
            (name.to_string(), vec![(source.to_string(), text.to_string())], args)
        };
        let mut cases = Vec::new();
        match var("CPP_DEPS_TEST_LIBCXX") {
            Some(dir) => cases.push(case(
                "libcxx",
                "main.cc",
                "#include <algorithm>\n#include <functional>\n#include <map>\n#include <memory>\n#include <optional>\n#include <string>\n#include <variant>\n#include <vector>\n",
                vec!["-std=c++20".into(), "-nostdinc++".into(), "-isystem".into(), dir],
            )),
            None => println!("NOT TESTED: libc++ (CPP_DEPS_TEST_LIBCXX names no include directory)"),
        }
        match var("CPP_DEPS_TEST_BOOST") {
            Some(dir) => cases.push(case(
                "boost",
                "main.cc",
                "#include <boost/config.hpp>\n#include <boost/config/pragma_message.hpp>\n#include <boost/static_assert.hpp>\n#include <boost/type_traits.hpp>\n",
                vec!["-std=c++17".into(), "-isystem".into(), dir],
            )),
            None => println!("NOT TESTED: Boost (CPP_DEPS_TEST_BOOST names no directory)"),
        }
        match var("CPP_DEPS_TEST_GLIB") {
            Some(flags) => cases.push(case("glib", "main.c", "#include <glib.h>\n", flags.split_whitespace().map(String::from).collect())),
            None => println!("NOT TESTED: GLib (CPP_DEPS_TEST_GLIB has no flags)"),
        }
        let exact = differential_cases(&cxx.to_string_lossy(), &cases);
        for (name, _, _) in &cases {
            assert!(exact.contains(name), "{name} isn't exact");
        }
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
        // The third review's.
        (
            "next_relative_inherits",
            &[("main.c", "#include <a.h>\n"), ("inc1/a.h", "#include \"b.h\"\n"), ("inc1/b.h", "#pragma once\n#include_next <b.h>\n"), ("inc2/b.h", "")],
            &["-I", "inc1", "-I", "inc2", "-c", "main.c"],
        ),
        (
            "next_relative_inherits_quote",
            &[
                ("main.c", "#include \"a.h\"\n"),
                ("q/a.h", "#include \"b.h\"\n"),
                ("q/b.h", "#ifndef B\n#define B\n#include_next <b.h>\n#endif\n"),
                ("inc/b.h", ""),
                ("q2/b.h", ""),
            ],
            &["-iquote", "q", "-iquote", "q2", "-I", "inc", "-c", "main.c"],
        ),
        (
            "wp_undef_define",
            &[("main.c", "#if _FORTIFY_SOURCE == 3\n#include \"f3.h\"\n#else\n#include \"fother.h\"\n#endif\n"), ("f3.h", ""), ("fother.h", "")],
            &["-D_FORTIFY_SOURCE=2", "-Wp,-U_FORTIFY_SOURCE,-D_FORTIFY_SOURCE=3", "-Wp,-Iwp", "-c", "main.c"],
        ),
        ("param_joined", &[("main.c", "#include \"a.h\"\n"), ("a.h", "")], &["--param=ssp-buffer-size=4", "-c", "main.c"]),
        (
            "iquote_dup_join",
            &[
                ("main.c", "#include \"x.h\"\n"),
                ("a/x.h", "#ifdef SECOND\n#include \"second.h\"\n#endif\n#define SECOND\n#include_next <x.h>\n"),
                ("a/second.h", ""),
                ("sys/x.h", ""),
            ],
            &["-iquote", "a", "-iquote", "a", "-I", "a", "-isystem", "sys", "-c", "main.c"],
        ),
        (
            "nonexistent_join",
            &[
                ("main.c", "#include \"x.h\"\n"),
                ("a/x.h", "#ifdef SECOND\n#include \"second.h\"\n#endif\n#define SECOND\n#include_next <x.h>\n"),
                ("a/second.h", ""),
                ("sys/x.h", ""),
            ],
            &["-iquote", "a", "-I", "missing", "-I", "a", "-isystem", "sys", "-c", "main.c"],
        ),
        ("pedantic_cxx", &[("main.cc", "#ifdef __cpp_runtime_arrays\n#include \"vla.h\"\n#endif\n"), ("vla.h", "")], &["-pedantic", "-c", "main.cc"]),
        ("nostdlib_ssp", &[("main.c", "#ifdef __SSP_STRONG__\n#include \"ssp.h\"\n#endif\n"), ("ssp.h", "")], &["-nostdlib", "-c", "main.c"]),
        (
            "include_file_macro_relative",
            &[("sub/main.c", "#ifndef AGAIN\n#define AGAIN\n#include __FILE__\n#endif\n"), ("sub/sub/main.c", "")],
            &["-c", "sub/main.c"],
        ),
        (
            "include_file_macro_absolute",
            &[("sub/main.c", "#ifndef AGAIN\n#define AGAIN\n#include __FILE__\n#endif\n"), ("sub/sub/main.c", "")],
            &["-c", "{d}/sub/main.c"],
        ),
        (
            "pragma_once_macro_operand",
            &[
                ("main.c", "#include \"h.h\"\n#define FEATURE\n#include \"h.h\"\n#ifdef FEATURE\n#include \"f.h\"\n#endif\n"),
                ("h.h", "#define ONCE_STR \"once\"\n_Pragma(ONCE_STR)\n#undef FEATURE\n"),
                ("f.h", ""),
            ],
            &["-c", "main.c"],
        ),
        // The fourth review's: pragmas libc++'s and Boost's macros run.
        (
            "libcxx_diag_pragma",
            &[("main.cc", "#define TOSTR(x) #x\n#define IGNORED(str) _Pragma(TOSTR(clang diagnostic ignored str))\n#include \"a.h\"\n"), ("a.h", "")],
            &["-c", "main.cc"],
        ),
        (
            "boost_pragma_message",
            &[
                (
                    "main.cc",
                    "#define BOOST_DO_STRINGIZE(X) #X\n#define BOOST_STRINGIZE(X) BOOST_DO_STRINGIZE(X)\n\
                     #define BOOST_PRAGMA_MESSAGE(x) _Pragma(BOOST_STRINGIZE(message(x)))\n#include \"a.h\"\n",
                ),
                ("a.h", ""),
            ],
            &["-c", "main.cc"],
        ),
        (
            "sysroot_eq_I",
            &[("main.c", "#include <x.h>\n"), ("sr/inc/x.h", "/*sysroot*/\n"), ("other/x.h", "/*other*/\n")],
            &["--sysroot=sr", "-I=/inc", "-I", "other", "-c", "main.c"],
        ),
        (
            "designated_include_field",
            &[("main.c", "#include \"a.h\"\nstruct o { int include; } x = {\n    .include = 1,\n};\n"), ("a.h", "")],
            &["-c", "main.c"],
        ),
    ];

    /// Clang's (`-Xclang`, its profiles, MSVC compatibility).
    const CLANG_CASES: &[Case] = &[
        ("xclang_dependent_lib", &[("main.c", "#include \"a.h\"\n"), ("a.h", "")], &["-Xclang", "--dependent-lib=msvcrt", "-c", "main.c"]),
        ("xclang_debug_dir", &[("main.c", "#include \"a.h\"\n"), ("a.h", "")], &["-Xclang", "-fdebug-compilation-dir", "-Xclang", ".", "-c", "main.c"]),
        (
            "profile_generate",
            &[("main.c", "#ifdef __LLVM_INSTR_PROFILE_GENERATE\n#include \"pg.h\"\n#endif\n"), ("pg.h", "")],
            &["-fprofile-instr-generate=x.profraw", "-c", "main.c"],
        ),
        (
            "static_darwin",
            &[("main.c", "#ifdef __STATIC__\n#include \"static.h\"\n#else\n#include \"dynamic.h\"\n#endif\n"), ("static.h", ""), ("dynamic.h", "")],
            &["--target=arm64-apple-macos14", "-static", "-c", "main.c"],
        ),
        (
            "next_from_includer_stack",
            &[
                ("src/main.cc", "#include <sub/a.h>\n"),
                ("inc/sub/a.h", "#include \"x.h\"\n"),
                ("src/x.h", "#include_next <x.h>\n"),
                ("inc/x.h", ""),
                ("other/x.h", ""),
            ],
            &["--target=x86_64-pc-windows-msvc", "-Iinc", "-Iother", "-c", "src/main.cc"],
        ),
        (
            "ms_int_suffix",
            &[("main.cc", "#if 0xffui64 == 255 && 1i64\n#include \"a.h\"\n#endif\n"), ("a.h", "")],
            &["--target=x86_64-pc-windows-msvc", "-c", "main.cc"],
        ),
        // Inexact: where Clang goes on depends on -Wmicrosoft-include.
        (
            "msvc_found_elsewhere_next",
            &[
                ("src/main.cc", "#include <sub/a.h>\n"),
                ("other/sub/a.h", "#include \"x.h\"\n"),
                ("src/x.h", "#include_next <x.h>\n"),
                ("inc/x.h", "/*inc*/\n"),
                ("other/x.h", "/*other*/\n"),
                ("third/x.h", "/*third*/\n"),
            ],
            &["--target=x86_64-pc-windows-msvc", "-Iinc", "-Iother", "-Ithird", "-c", "src/main.cc"],
        ),
        (
            "forced_source_dir_next",
            &[("src/main.cc", ""), ("src/pre.h", "#include_next <pre.h>\n"), ("inc/pre.h", "/*inc*/\n"), ("other/pre.h", "/*other*/\n")],
            &["--target=x86_64-pc-windows-msvc", "-Iinc", "-Iother", "-include", "pre.h", "-c", "src/main.cc"],
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
        (
            "warning_level_cl",
            &[("main.cc", "#if defined(_MSVC_WARNING_LEVEL) && _MSVC_WARNING_LEVEL >= 4\n#include \"w4.h\"\n#endif\n"), ("w4.h", "")],
            &["/nologo", "/W4", "/c", "main.cc"],
        ),
        ("clang_wp_define_cl", &[("main.cc", "#ifdef FOO\n#include \"foo.h\"\n#endif\n"), ("foo.h", "")], &["/nologo", "/clang:-Wp,-DFOO", "/c", "main.cc"]),
        ("o_separate_cl", &[("main.cc", "#include \"a.h\"\n"), ("a.h", "")], &["/nologo", "-o", "x.obj", "/c", "main.cc"]),
        (
            "libcxx_diag_pragma_cl",
            &[("main.cc", "#define TOSTR(x) #x\n#define IGNORED(str) _Pragma(TOSTR(clang diagnostic ignored str))\n#include \"a.h\"\n"), ("a.h", "")],
            &["/nologo", "/c", "main.cc"],
        ),
        ("int_suffix_cl", &[("main.cc", "#if 0xffui64 == 255 && 1i64\n#include \"a.h\"\n#endif\n"), ("a.h", "")], &["/nologo", "/c", "main.cc"]),
    ];

    /// The cases against GCC or Clang: `CPP_DEPS_TEST_CXX`.
    #[test]
    #[cfg_attr(not(cpp_deps_test_cxx), ignore = "needs CPP_DEPS_TEST_CXX, a GCC or Clang")]
    fn real_differential() {
        let cxx = crate::test_compiler("CPP_DEPS_TEST_CXX");
        let cxx = cxx.to_string_lossy();
        let exact = differential(&cxx, GCC_CASES);
        for name in [
            "plain",
            "cr_only_line_endings",
            "include_next_from_iquote",
            "imacros_after_include_order",
            "per_object_flags",
            "next_relative_inherits",
            "next_relative_inherits_quote",
            "wp_undef_define",
            "param_joined",
            "iquote_dup_join",
            "nonexistent_join",
            "pedantic_cxx",
            "nostdlib_ssp",
            "include_file_macro_absolute",
            "libcxx_diag_pragma",
            "boost_pragma_message",
            "sysroot_eq_I",
            "designated_include_field",
        ] {
            assert!(exact.contains(&name), "{name} isn't exact: {exact:?}");
        }
        let cwd = test_dirs::TestDir::new("cpp_deps_differential_fingerprint");
        if probe::fingerprint(&probe::Compiler {
            path: Path::new(&*cxx),
            args: &[],
            style: Style::Gcc,
            lang: "c",
            cwd: &cwd,
            env: None,
            timeout: probe::TIMEOUT,
        })
        .contains("clang")
        {
            let exact = differential(&cxx, CLANG_CASES);
            for &(name, _, _) in CLANG_CASES {
                let inexact = ["msvc_found_elsewhere_next", "forced_source_dir_next"].contains(&name);
                assert_eq!(exact.contains(&name), !inexact, "{name}: {exact:?}");
            }
        }
    }

    /// The cl-style cases against the compiler `var` names.
    fn cl_differential(var: &str) {
        let exact = differential(&crate::test_compiler(var).to_string_lossy(), CL_CASES);
        for name in [
            "plain_cl",
            "external_i_after_i",
            "forced_include_includer_stack",
            "warning_level_cl",
            "clang_wp_define_cl",
            "o_separate_cl",
            "libcxx_diag_pragma_cl",
            "int_suffix_cl",
        ] {
            assert!(exact.contains(&name), "{var}: {name} isn't exact: {exact:?}");
        }
    }

    /// The cl-style cases against `CPP_DEPS_TEST_CL` (cl.exe, with INCLUDE
    /// set).
    #[test]
    #[cfg_attr(not(cpp_deps_test_cl), ignore = "needs CPP_DEPS_TEST_CL, a cl.exe with INCLUDE set")]
    fn real_cl_differential() {
        cl_differential("CPP_DEPS_TEST_CL");
    }

    /// The cl-style cases against `CPP_DEPS_TEST_CLANG_CL`.
    #[test]
    #[cfg_attr(not(cpp_deps_test_clang_cl), ignore = "needs CPP_DEPS_TEST_CLANG_CL, a clang-cl")]
    fn real_clang_cl_differential() {
        cl_differential("CPP_DEPS_TEST_CLANG_CL");
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
