//! The environment variables tools read that change what they write: a
//! command run locally inherits them, a remote action only has its own.
//! Those the action's tools read go into its environment, with paths
//! rewritten as in its arguments (relative to the working directory when
//! under the exec root, else left as the host names them, for a worker of
//! the same OS), so that its key describes them and remote runs get them.
//! Variables of other tools are left out: they change nothing. So are those
//! a tool's command line makes it ignore (a toolchain named with
//! `/vctoolsdir`, standard include directories left out with `/X`, lld-link's
//! `/lldignoreenv`): a developer prompt's values would otherwise change the
//! keys of a hermetic toolchain's actions. A variable that can't be carried
//! (one making a compiler write a file the action doesn't return, or that
//! isn't UTF-8) keeps a local result from being stored.
//!
//! `--action_env` sets variables in every action, as Bazel's does, without
//! rewriting; a variable it names is carried, never refused.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use crate::cmdline::{join_rsp, split_rsp};
use crate::paths::PathMapper;

/// The kinds of tools whose variables are known, from their name and
/// command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// cl.exe, or clang-cl (`clang`), compiling.
    ClCompile {
        clang: bool,
    },
    /// A GCC-style driver compiling.
    GnuCompile {
        clang: bool,
    },
    /// link.exe or lld-link, or a cl-style driver linking (`driver`).
    MsvcLink {
        clang: bool,
        driver: bool,
    },
    /// A GNU-style linker, or a GCC-style driver linking.
    GnuLink {
        clang: bool,
    },
    /// `ar` and the like.
    Ar,
    /// lib.exe or llvm-lib.
    Lib,
    Other,
}

/// A tool an action runs: its kind, and the variables it reads given its
/// command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub kind: Kind,
    /// The variables it reads, and how they're carried.
    vars: Vec<(String, Value)>,
    /// Those it reads that the action can't have.
    refused: Vec<&'static str>,
}

/// How a variable's value is carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Value {
    /// As it is.
    Plain,
    /// A path.
    Path,
    /// Paths, separated as on the host (`;` on Windows, `:` elsewhere).
    Paths,
    /// Paths separated by `;` (MSVC's).
    MsvcPaths,
    /// Command-line options (cl's `CL`, link's `LINK`).
    Options,
}

/// cl.exe and clang-cl: options added to every command line, whatever it
/// says.
const CL_OPTIONS: &[(&str, Value)] = &[("CL", Value::Options), ("_CL_", Value::Options)];

/// cl.exe and clang-cl (and Clang for MSVC targets): include directories,
/// unless the command line leaves out the standard ones, or (Clang) names
/// the toolchain.
const MSVC_INCLUDE: &[(&str, Value)] = &[("INCLUDE", Value::MsvcPaths), ("EXTERNAL_INCLUDE", Value::MsvcPaths)];

/// Where Clang and lld-link find Visual Studio when the command line
/// doesn't name it (`/vctoolsdir`, `/winsysroot`). They find the Windows
/// SDK from the command line or the registry, never from the environment
/// (`WindowsSdkDir`, `UCRTVersion`).
const VS_LOCATIONS: &[(&str, Value)] = &[("VCToolsInstallDir", Value::Path), ("VCINSTALLDIR", Value::Path)];

/// Apple's deployment targets, which Clang reads as `-m*-version-min`.
const DEPLOYMENT_TARGETS: &[(&str, Value)] = &[
    ("MACOSX_DEPLOYMENT_TARGET", Value::Plain),
    ("IPHONEOS_DEPLOYMENT_TARGET", Value::Plain),
    ("TVOS_DEPLOYMENT_TARGET", Value::Plain),
    ("WATCHOS_DEPLOYMENT_TARGET", Value::Plain),
    ("XROS_DEPLOYMENT_TARGET", Value::Plain),
    ("DRIVERKIT_DEPLOYMENT_TARGET", Value::Plain),
];

/// GCC and Clang drivers compiling: include directories, where the driver
/// finds its programs, reproducible timestamps, Apple SDKs, the command
/// line Apple's builds record in debug information.
const GNU_COMPILE: &[(&str, Value)] = &[
    ("CPATH", Value::Paths),
    ("C_INCLUDE_PATH", Value::Paths),
    ("CPLUS_INCLUDE_PATH", Value::Paths),
    ("OBJC_INCLUDE_PATH", Value::Paths),
    ("OBJCPLUS_INCLUDE_PATH", Value::Paths),
    ("GCC_EXEC_PREFIX", Value::Path),
    ("COMPILER_PATH", Value::Paths),
    ("CCC_OVERRIDE_OPTIONS", Value::Plain),
    ("SOURCE_DATE_EPOCH", Value::Plain),
    ("SDKROOT", Value::Path),
    ("DEVELOPER_DIR", Value::Path),
    ("RC_DEBUG_OPTIONS", Value::Plain),
];

/// GCC's dependency output: files the action doesn't return.
const GNU_COMPILE_REFUSED: &[&str] = &["DEPENDENCIES_OUTPUT", "SUNPRO_DEPENDENCIES"];

/// Clang drivers: whether to read configuration files, and what it prints
/// on its standard error (the options of each job, the headers read,
/// diagnostics, statistics)...
const CLANG: &[(&str, Value)] = &[
    ("CLANG_NO_DEFAULT_CONFIG", Value::Plain),
    ("CC_PRINT_OPTIONS", Value::Plain),
    ("CC_PRINT_HEADERS", Value::Plain),
    ("CC_PRINT_HEADERS_FORMAT", Value::Plain),
    ("CC_PRINT_HEADERS_FILTERING", Value::Plain),
    ("CC_LOG_DIAGNOSTICS", Value::Plain),
    ("CC_PRINT_PROC_STAT", Value::Plain),
];

/// ...or into files the action doesn't return.
const CLANG_REFUSED: &[&str] = &["CC_PRINT_OPTIONS_FILE", "CC_PRINT_HEADERS_FILE", "CC_LOG_DIAGNOSTICS_FILE", "CC_PRINT_PROC_STAT_FILE"];

/// MSVC linkers: library directories, and options added to every command
/// line (unless lld-link ignores the environment). No linker reads
/// `LIBPATH` (cl's `/clr` does).
const MSVC_LIB: &[(&str, Value)] = &[("LIB", Value::MsvcPaths)];
const LINK_OPTIONS: &[(&str, Value)] = &[("LINK", Value::Options), ("_LINK_", Value::Options)];

/// GNU-style links: library directories, where the driver finds its
/// programs, Apple SDKs and timestamps, and the run-time library path the
/// program keeps (as `-rpath`).
const GNU_LINK: &[(&str, Value)] = &[
    ("LIBRARY_PATH", Value::Paths),
    ("GCC_EXEC_PREFIX", Value::Path),
    ("COMPILER_PATH", Value::Paths),
    ("CCC_OVERRIDE_OPTIONS", Value::Plain),
    ("SDKROOT", Value::Path),
    ("DEVELOPER_DIR", Value::Path),
    ("ZERO_AR_DATE", Value::Plain),
    ("LD_RUN_PATH", Value::Plain),
];

/// Archivers: Apple's timestamps. (lib.exe's library directories are
/// `MSVC_LIB`, see `lib_searches_env`.)
const AR: &[(&str, Value)] = &[("ZERO_AR_DATE", Value::Plain)];

/// Whether a cl-style command line has an option (`/name` or `-name`),
/// alone or with a value joined (`/vctoolsdirC:\vc`): `names` are
/// prefixes, compared as cl does (case-sensitively).
fn cl_option(args: &[String], names: &[&str]) -> bool {
    args.iter().any(|a| a.strip_prefix(['/', '-']).is_some_and(|o| names.iter().any(|n| o.starts_with(n))))
}

/// Whether a linker's command line has an option, as MSVC linkers compare
/// them (case-insensitively, `/name` or `-name`, with or without a value).
fn link_option(args: &[&str], name: &str) -> bool {
    args.iter().any(|a| a.strip_prefix(['/', '-']).map(|o| o.split_once(':').map_or(o, |(n, _)| n)).is_some_and(|o| o.eq_ignore_ascii_case(name)))
}

/// The arguments a driver passes to its linker: after cl's `/link`, or in
/// `-Wl,` and `-Xlinker`.
fn linker_args(args: &[String], cl: bool) -> Vec<&str> {
    if cl {
        return args.iter().skip_while(|a| !(a.eq_ignore_ascii_case("/link") || a.eq_ignore_ascii_case("-link"))).skip(1).map(String::as_str).collect();
    }
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(list) = a.strip_prefix("-Wl,") {
            out.extend(list.split(','));
        } else if a == "-Xlinker" {
            out.extend(it.next().map(String::as_str));
        }
    }
    out
}

/// Whether lib.exe or llvm-lib, given `args`, looks for an input in `LIB`'s
/// directories: when it isn't found from the working directory, nor in a
/// `/LIBPATH:` directory (`present` tells, for a path relative to the
/// working directory or absolute).
fn lib_searches_env(args: &[String], present: &dyn Fn(&Path) -> bool) -> bool {
    let libpaths: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix(['/', '-'])?.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("libpath"))
        .map(|(_, dir)| dir.trim_matches('"'))
        .collect();
    // An absolute POSIX path is an input, not an option.
    let mut inputs = args.iter().filter(|a| !(a.starts_with('-') || (a.starts_with('/') && !present(Path::new(a.as_str())))));
    inputs.any(|i| !present(Path::new(i)) && !libpaths.iter().any(|d| present(&Path::new(d).join(i))))
}

impl Tool {
    /// The tool `exe` running `argv` (response files expanded, with the
    /// options `CL` or `LINK` add): compiling (`style`, the scanner's),
    /// linking, archiving. `windows`: on a Windows host, where Clang
    /// targets MSVC unless told otherwise. As `of_step`, for a command line
    /// without response files whose files are found from this process's
    /// directory.
    pub fn of(exe: &Path, argv: &[String], style: Option<crate::scanner::Style>, windows: bool) -> Tool {
        Self::of_step(exe, argv, argv, style, windows, &|p| p.is_file())
    }

    /// The tool of a step: `exe` running `argv` (as for `of`), whose
    /// command line is `written` (response files unexpanded), and for
    /// which `present` tells which files are there (absolute, or relative
    /// to its working directory).
    pub fn of_step(
        exe: &Path,
        written: &[String],
        argv: &[String],
        style: Option<crate::scanner::Style>,
        windows: bool,
        present: &dyn Fn(&Path) -> bool,
    ) -> Tool {
        let name = exe.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let clang = name.contains("clang");
        let cl = style == Some(crate::scanner::Style::Msvc);
        let kind = match (style, crate::link::linker_flavor(&exe.to_string_lossy())) {
            (Some(_), _) if crate::link::driver_compiles(argv) => {
                if cl {
                    Kind::ClCompile { clang }
                } else {
                    Kind::GnuCompile { clang }
                }
            }
            (Some(_), _) if cl => Kind::MsvcLink { clang, driver: true },
            (Some(_), _) => Kind::GnuLink { clang },
            (None, Some(crate::link::Flavor::Msvc)) => Kind::MsvcLink { clang: name == "lld-link", driver: false },
            (None, Some(crate::link::Flavor::Gnu)) => Kind::GnuLink { clang: false },
            (None, None) if name == "lib" || name == "llvm-lib" => Kind::Lib,
            (None, None) if is_archiver(&name) => Kind::Ar,
            _ => Kind::Other,
        };
        let mut tool = Tool { kind, vars: Vec::new(), refused: Vec::new() };
        let args = argv.get(1..).unwrap_or_default();
        // Clang for MSVC targets names its toolchain with these, which it
        // then reads instead of the environment.
        let gnu_toolchain = |names: &[&str]| args.iter().any(|a| names.iter().any(|n| a.starts_with(n)));
        let no_std_includes = |cl: bool| {
            args.iter().any(|a| matches!(a.as_str(), "-nostdinc" | "-nostdlibinc" | "/clang:-nostdinc" | "/clang:-nostdlibinc"))
                || (cl && args.iter().any(|a| a == "/X" || a == "-X"))
        };
        match kind {
            Kind::ClCompile { clang } => {
                tool.add(CL_OPTIONS);
                // Clang reads neither once the toolchain is named.
                let named = clang && cl_option(args, &["vctoolsdir", "winsysroot"]);
                if !no_std_includes(true) && !named {
                    tool.add(MSVC_INCLUDE);
                }
                // `/external:env:NAME`: include directories from NAME.
                for a in args {
                    if let Some(var) = a.strip_prefix(['/', '-']).and_then(|o| o.strip_prefix("external:env:")) {
                        tool.vars.push((var.to_string(), Value::MsvcPaths));
                    }
                }
                // cl's `/clr`: assemblies for `#using`.
                if !clang && cl_option(args, &["clr"]) {
                    tool.vars.push(("LIBPATH".into(), Value::MsvcPaths));
                }
                if clang {
                    if !named {
                        tool.add(VS_LOCATIONS);
                    }
                    tool.add_clang();
                }
            }
            Kind::GnuCompile { clang } => {
                tool.add(GNU_COMPILE);
                tool.add(DEPLOYMENT_TARGETS);
                tool.refused.extend_from_slice(GNU_COMPILE_REFUSED);
                if clang {
                    if targets_msvc(argv, windows) {
                        let named = gnu_toolchain(&["-Xmicrosoft-visualc-tools-root", "-Xmicrosoft-windows-sys-root"]);
                        if !no_std_includes(false) && !named {
                            tool.add(MSVC_INCLUDE);
                        }
                        if !named {
                            tool.add(VS_LOCATIONS);
                        }
                    }
                    tool.add_clang();
                }
            }
            Kind::MsvcLink { clang, driver } => {
                let linker = if driver { linker_args(args, true) } else { args.iter().map(String::as_str).collect() };
                // lld-link's `/lldignoreenv`: it then reads neither `LIB` nor
                // where Visual Studio is (nor `LIB` under `/winsysroot`).
                // Nor `LINK` and `_LINK_`, which it adds before reading
                // response files: when its own command line has it, not a
                // response file (a driver passes it on the linker's).
                let ignores = clang && link_option(&linker, "lldignoreenv");
                let written_args: Vec<&str> = written.get(1..).unwrap_or_default().iter().map(String::as_str).collect();
                let ignores_options = if driver { ignores } else { clang && link_option(&written_args, "lldignoreenv") };
                let sysroot = clang && !driver && link_option(&linker, "winsysroot");
                if !ignores && !sysroot {
                    tool.add(MSVC_LIB);
                }
                if !ignores_options {
                    tool.add(LINK_OPTIONS);
                }
                // The driver's own (and its toolchain's, which it reads
                // instead of the environment's once named; lld-link alone
                // doesn't look for it under `/lldignoreenv` either).
                let named = if driver {
                    cl_option(args, &["vctoolsdir", "winsysroot"])
                } else {
                    link_option(&linker, "vctoolsdir") || link_option(&linker, "winsysroot")
                };
                if driver {
                    tool.add(CL_OPTIONS);
                }
                if clang {
                    if !named && (driver || !ignores) {
                        tool.add(VS_LOCATIONS);
                    }
                    if driver {
                        tool.add_clang();
                    }
                }
            }
            Kind::GnuLink { clang } => {
                tool.add(GNU_LINK);
                tool.add(DEPLOYMENT_TARGETS);
                if clang {
                    // Linking for MSVC: the driver runs an MSVC linker.
                    if targets_msvc(argv, windows) {
                        if !link_option(&linker_args(args, false), "lldignoreenv") {
                            tool.add(MSVC_LIB);
                            tool.add(LINK_OPTIONS);
                        }
                        if !gnu_toolchain(&["-Xmicrosoft-visualc-tools-root", "-Xmicrosoft-windows-sys-root"]) {
                            tool.add(VS_LOCATIONS);
                        }
                    }
                    tool.add_clang();
                }
            }
            Kind::Ar => tool.add(AR),
            Kind::Lib => {
                if lib_searches_env(args, present) {
                    tool.add(MSVC_LIB);
                }
            }
            Kind::Other => {}
        }
        tool
    }

    /// The variables naming include directories it searches, which it
    /// reads (cl's and clang-cl's `INCLUDE`, `EXTERNAL_INCLUDE` and
    /// `/external:env:`'s).
    pub fn include_dir_vars(&self) -> impl Iterator<Item = &str> {
        let compile = matches!(self.kind, Kind::ClCompile { .. });
        self.vars.iter().filter(move |(n, v)| compile && *v == Value::MsvcPaths && n != "LIBPATH").map(|(n, _)| n.as_str())
    }

    fn add(&mut self, vars: &[(&str, Value)]) {
        self.vars.extend(vars.iter().map(|(n, v)| (n.to_string(), *v)));
    }

    /// Clang drivers' own.
    fn add_clang(&mut self) {
        self.add(CLANG);
        self.refused.extend_from_slice(CLANG_REFUSED);
    }
}

/// Whether a GCC-style Clang command targets MSVC, whose toolchain reads
/// MSVC's variables: the target it's given (as the scanner decides its
/// lookup: Windows, not MinGW, Cygwin nor Itanium), else the host's
/// (Windows hosts' Clang targets MSVC).
fn targets_msvc(argv: &[String], windows: bool) -> bool {
    let mut target = None;
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--target" || a == "-target" {
            target = it.next().map(String::as_str);
        } else if let Some(t) = a.strip_prefix("--target=").or_else(|| a.strip_prefix("-target=")) {
            target = Some(t);
        }
    }
    match target.map(str::to_ascii_lowercase) {
        Some(t) => (t.contains("windows") || t.contains("win32")) && !["gnu", "cygnus", "cygwin", "mingw", "itanium"].iter().any(|e| t.contains(e)),
        None => windows,
    }
}

/// Whether a program is an archiver (`ar`, `llvm-ar`, `gcc-ar`, a cross
/// toolchain's `x86_64-linux-gnu-ar`, Apple's `libtool` and `ranlib`), from
/// its name: not `tar` nor `jar`.
fn is_archiver(name: &str) -> bool {
    // A version after it (`llvm-ar-18`).
    let name = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let name = name.strip_suffix('-').unwrap_or(name);
    matches!(name, "ar" | "llvm-ar" | "libtool" | "ranlib" | "llvm-ranlib") || name.ends_with("-ar") || name.ends_with("-ranlib")
}

/// An action's environment: the variables its tools read, from `vars`
/// (the build's environment), then `action_env`. And why a local result
/// can't be the action's, if it can't: a variable set in `vars` that a tool
/// reads and the action can't carry.
pub fn action_env(
    tools: &[Tool],
    mapper: &mut PathMapper,
    vars: &dyn Fn(&str) -> Option<OsString>,
    action_env: &[(String, String)],
) -> (BTreeMap<String, String>, Option<String>) {
    let windows = mapper.windows;
    let same = |a: &str, b: &str| if windows { a.eq_ignore_ascii_case(b) } else { a == b };
    let given = |name: &str| action_env.iter().any(|(n, _)| same(n, name));
    let mut env = BTreeMap::new();
    let mut refused = None;
    for tool in tools {
        for (name, kind) in &tool.vars {
            if env.contains_key(name) || given(name) {
                continue;
            }
            match vars(name).map(OsString::into_string) {
                Some(Ok(value)) => {
                    env.insert(name.clone(), carried(&value, *kind, mapper));
                }
                // A command can't carry it (nor `--action_env`).
                Some(Err(_)) => {
                    refused.get_or_insert_with(|| format!("{name} isn't UTF-8, which their actions can't have"));
                }
                None => {}
            }
        }
        if let Some(name) = tool.refused.iter().find(|n| !given(n) && vars(n).is_some()) {
            refused.get_or_insert_with(|| format!("{name} is set, which their actions can't have (--action_env={name} gives it them)"));
        }
    }
    for (name, value) in action_env {
        env.retain(|n, _| !same(n, name));
        env.insert(name.clone(), value.clone());
    }
    (env, refused)
}

/// A variable's value as the action gets it.
fn carried(value: &str, kind: Value, mapper: &mut PathMapper) -> String {
    let windows = mapper.windows;
    match kind {
        Value::Plain => value.to_string(),
        Value::Path => mapper.rewrite_arg(value),
        Value::Paths => {
            let sep = if windows { ';' } else { ':' };
            value.split(sep).map(|p| mapper.rewrite_arg(p)).collect::<Vec<_>>().join(&sep.to_string())
        }
        // Empty entries dropped, as cl does.
        Value::MsvcPaths => value.split(';').filter(|s| !s.is_empty()).map(|p| mapper.rewrite_arg(p)).collect::<Vec<_>>().join(";"),
        Value::Options => join_rsp(&mapper.rewrite_args(&split_rsp(value, true), true), true),
    }
}

/// The options a tool takes from the environment, as `vars` gives them:
/// cl's and clang-cl's `CL` (before the command's) and `_CL_` (after), the
/// MSVC linkers' `LINK` and `_LINK_` (unless lld-link ignores the
/// environment). The include scanner, the link's analysis and the check of
/// definitions see the command line as the tool does.
pub fn env_options(tool: &Tool, vars: &dyn Fn(&str) -> Option<OsString>) -> (Vec<String>, Vec<String>) {
    let (before, after) = match tool.kind {
        Kind::ClCompile { .. } => ("CL", "_CL_"),
        Kind::MsvcLink { driver: false, .. } => ("LINK", "_LINK_"),
        _ => return Default::default(),
    };
    let read = |name: &str| -> Vec<String> {
        if !tool.vars.iter().any(|(n, _)| n == name) {
            return Vec::new();
        }
        vars(name).and_then(|v| v.into_string().ok()).map(|v| split_rsp(&v, true)).unwrap_or_default()
    };
    (read(before), read(after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ExecRoot;
    use crate::scanner::Style;

    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    fn names(env: &BTreeMap<String, String>) -> Vec<&str> {
        env.keys().map(String::as_str).collect()
    }

    #[test]
    fn tools() {
        let of = |exe: &str, cmd: &str, style| Tool::of(Path::new(exe), &argv(cmd), style, false).kind;
        assert_eq!(of("/usr/bin/gcc", "gcc -c a.c", Some(Style::Gcc)), Kind::GnuCompile { clang: false });
        assert_eq!(of("/usr/bin/clang++", "clang++ a.o -o app", Some(Style::Gcc)), Kind::GnuLink { clang: true });
        assert_eq!(of("C:/llvm/bin/clang-cl.exe", "clang-cl /c a.c", Some(Style::Msvc)), Kind::ClCompile { clang: true });
        assert_eq!(of("C:/vs/cl.exe", "cl a.obj /link /out:a.exe", Some(Style::Msvc)), Kind::MsvcLink { clang: false, driver: true });
        assert_eq!(of("C:/llvm/bin/lld-link.exe", "lld-link a.obj", None), Kind::MsvcLink { clang: true, driver: false });
        assert_eq!(of("/usr/bin/ld.lld", "ld.lld a.o", None), Kind::GnuLink { clang: false });
        assert_eq!(of("/usr/bin/llvm-ar", "llvm-ar qc a.a a.o", None), Kind::Ar);
        assert_eq!(of("C:/llvm/bin/llvm-lib.exe", "llvm-lib /out:a.lib a.obj", None), Kind::Lib);
        assert_eq!(of("/usr/bin/python3", "python3 gen.py", None), Kind::Other);
        // Archivers by name, not every name ending in "ar".
        for ar in ["ar", "llvm-ar", "llvm-ar-18", "gcc-ar", "x86_64-linux-gnu-ar", "libtool", "ranlib", "x86_64-w64-mingw32-ranlib"] {
            assert_eq!(of(&format!("/usr/bin/{ar}"), &format!("{ar} qc a.a a.o"), None), Kind::Ar, "{ar}");
        }
        for other in ["tar", "jar", "rar", "star"] {
            assert_eq!(of(&format!("/usr/bin/{other}"), &format!("{other} cf a.tar a.o"), None), Kind::Other, "{other}");
        }
    }

    /// Each tool gets the variables it reads, paths rewritten; a link
    /// variable doesn't change a compile; a refused one is reported unless
    /// `--action_env` sets it; `--action_env` values go as given.
    #[test]
    fn environments() {
        let root = ExecRoot::common(&["/home/u/proj/build".into(), "/home/u/proj/src/a.c".into()], false);
        let mut m = PathMapper::new(root, "/home/u/proj/build", false).unwrap();
        m.tree_path("/home/u/proj/src/a.c").unwrap();
        let host: BTreeMap<&str, &str> = [
            ("CPATH", "/home/u/proj/include::/opt/inc"),
            ("LIB", "/home/u/proj/lib"),
            ("LIBRARY_PATH", "/home/u/proj/lib"),
            ("SOURCE_DATE_EPOCH", "0"),
            ("DEPENDENCIES_OUTPUT", "deps.txt"),
            ("SDKROOT", "/home/u/proj/sdk"),
            ("LD_RUN_PATH", "/home/u/proj/build/lib"),
            ("TVOS_DEPLOYMENT_TARGET", "17.0"),
        ]
        .into_iter()
        .collect();
        let vars = |n: &str| host.get(n).map(OsString::from);
        let gcc = |cmd: &str| Tool::of(Path::new("/usr/bin/gcc"), &argv(cmd), Some(Style::Gcc), false);
        let clang = |cmd: &str| Tool::of(Path::new("/usr/bin/clang"), &argv(cmd), Some(Style::Gcc), false);
        let (env, refused) = action_env(&[clang("clang -c a.c")], &mut m, &vars, &[]);
        assert_eq!(
            env.into_iter().collect::<Vec<_>>(),
            [
                ("CPATH".into(), "../include::/opt/inc".into()),
                ("SDKROOT".into(), "../sdk".into()),
                ("SOURCE_DATE_EPOCH".into(), "0".into()),
                ("TVOS_DEPLOYMENT_TARGET".into(), "17.0".into())
            ]
        );
        assert_eq!(refused.as_deref(), Some("DEPENDENCIES_OUTPUT is set, which their actions can't have (--action_env=DEPENDENCIES_OUTPUT gives it them)"));
        let (env, refused) = action_env(&[clang("clang a.o -o app")], &mut m, &vars, &[]);
        assert_eq!(names(&env), ["LD_RUN_PATH", "LIBRARY_PATH", "SDKROOT", "TVOS_DEPLOYMENT_TARGET"]);
        assert_eq!(env["LIBRARY_PATH"], "../lib");
        // A path the program keeps, as it is.
        assert_eq!(env["LD_RUN_PATH"], "/home/u/proj/build/lib");
        assert_eq!(refused, None);
        let given = [("DEPENDENCIES_OUTPUT".to_string(), "deps.txt".to_string()), ("SDKROOT".into(), "/sdk".into()), ("FOO".into(), "bar".into())];
        let python = Tool::of(Path::new("/usr/bin/python3"), &argv("python3 gen.py"), None, false);
        let (env, refused) = action_env(&[gcc("gcc -c a.c"), python.clone()], &mut m, &vars, &given);
        assert_eq!((env["SDKROOT"].as_str(), env["FOO"].as_str(), env["DEPENDENCIES_OUTPUT"].as_str()), ("/sdk", "bar", "deps.txt"));
        assert_eq!(refused, None);
        assert!(action_env(&[python], &mut m, &vars, &[]).0.is_empty());
    }

    /// Variables Clang writes files from are refused; those changing what
    /// it prints are carried. GCC reads neither.
    #[test]
    fn clang_output_variables() {
        let root = ExecRoot::common(&["/home/u/proj/build".into()], false);
        let mut m = PathMapper::new(root, "/home/u/proj/build", false).unwrap();
        let host: BTreeMap<&str, &str> =
            [("CC_PRINT_HEADERS", "1"), ("CC_PRINT_HEADERS_FILE", "headers.txt"), ("CLANG_NO_DEFAULT_CONFIG", "1")].into_iter().collect();
        let vars = |n: &str| host.get(n).map(OsString::from);
        let clang = Tool::of(Path::new("/usr/bin/clang"), &argv("clang -c a.c"), Some(Style::Gcc), false);
        let (env, refused) = action_env(&[clang], &mut m, &vars, &[]);
        assert_eq!(names(&env), ["CC_PRINT_HEADERS", "CLANG_NO_DEFAULT_CONFIG"]);
        assert!(refused.is_some_and(|r| r.starts_with("CC_PRINT_HEADERS_FILE is set")));
        let gcc = Tool::of(Path::new("/usr/bin/gcc"), &argv("gcc -c a.c"), Some(Style::Gcc), false);
        assert_eq!(action_env(&[gcc], &mut m, &vars, &[]), (BTreeMap::new(), None));
    }

    /// A value that isn't UTF-8 can't be carried: the local run has it, the
    /// action wouldn't.
    #[test]
    fn values_not_utf8() {
        #[cfg(unix)]
        let bad: OsString = std::os::unix::ffi::OsStringExt::from_vec(vec![b'x', 0xff]);
        #[cfg(windows)]
        let bad: OsString = std::os::windows::ffi::OsStringExt::from_wide(&[u16::from(b'x'), 0xd800]);
        let root = ExecRoot::common(&["/home/u/proj/build".into()], false);
        let mut m = PathMapper::new(root, "/home/u/proj/build", false).unwrap();
        let vars = |n: &str| (n == "CPATH").then(|| bad.clone());
        let gcc = Tool::of(Path::new("/usr/bin/gcc"), &argv("gcc -c a.c"), Some(Style::Gcc), false);
        let (env, refused) = action_env(&[gcc], &mut m, &vars, &[]);
        assert!(env.is_empty());
        assert_eq!(refused.as_deref(), Some("CPATH isn't UTF-8, which their actions can't have"));
    }

    #[test]
    fn msvc_environments() {
        let root = ExecRoot::common(&["C:/Users/x/proj/build".into(), "C:/Users/x/proj/src/a.cpp".into()], true);
        let mut m = PathMapper::new(root, "C:\\Users\\x\\proj\\build", true).unwrap();
        m.tree_path("C:/Users/x/proj/src/a.cpp").unwrap();
        let host: BTreeMap<&str, &str> = [
            ("INCLUDE", r"C:\Users\x\proj\inc;;C:\VS\include;"),
            ("CL", r#"/DDATA="C:\Users\x\proj\data" /IC:\Users\x\proj\extra"#),
            ("LIB", r"C:\Users\x\proj\lib;C:\VS\lib"),
            ("LIBPATH", r"C:\VS\lib"),
            ("LINK", r"/DEF:C:\Users\x\proj\src\a.def"),
            ("VCToolsInstallDir", r"C:\VS\"),
            ("WindowsSdkDir", r"C:\Kits\10\"),
            ("MY_INCLUDES", r"C:\Users\x\proj\ext"),
        ]
        .into_iter()
        .collect();
        let vars = |n: &str| host.get(n).map(OsString::from);
        let env_of = |m: &mut PathMapper, exe: &str, cmd: &str, style| action_env(&[Tool::of(Path::new(exe), &argv(cmd), style, true)], m, &vars, &[]).0;
        let env = env_of(&mut m, "C:/vs/cl.exe", "cl /c a.cpp", Some(Style::Msvc));
        assert_eq!(names(&env), ["CL", "INCLUDE"]);
        assert_eq!(env["INCLUDE"], r"..\inc;C:\VS\include");
        // Split and joined again as cl does: the definition keeps its path.
        assert_eq!(env["CL"], r"/DDATA=C:\Users\x\proj\data /I..\extra");
        // Directories named by `/external:env:`; `/clr`'s assemblies.
        let env = env_of(&mut m, "C:/vs/cl.exe", "cl /c /clr /external:env:MY_INCLUDES a.cpp", Some(Style::Msvc));
        assert_eq!(names(&env), ["CL", "INCLUDE", "LIBPATH", "MY_INCLUDES"]);
        assert_eq!(env["MY_INCLUDES"], r"..\ext");
        let env = env_of(&mut m, "C:/llvm/bin/lld-link.exe", "lld-link a.obj", None);
        assert_eq!(names(&env), ["LIB", "LINK", "VCToolsInstallDir"]);
        assert_eq!(env["LINK"], r"/DEF:..\src\a.def");
        // Clang on a Windows host targets MSVC, reading MSVC's include
        // directories too; not when it targets something else.
        let env = env_of(&mut m, "C:/llvm/bin/clang++.exe", "clang++ -c a.cpp", Some(Style::Gcc));
        assert_eq!(names(&env), ["INCLUDE", "VCToolsInstallDir"]);
        for target in ["--target=x86_64-linux-gnu", "--target=x86_64-w64-windows-gnu"] {
            let env = env_of(&mut m, "C:/llvm/bin/clang++.exe", &format!("clang++ {target} -c a.cpp"), Some(Style::Gcc));
            assert!(env.is_empty(), "{target}: {env:?}");
        }
        let env = env_of(&mut m, "/usr/bin/clang++", "clang++ --target=x86_64-pc-windows-msvc -c a.cpp", Some(Style::Gcc));
        assert_eq!(names(&env), ["INCLUDE", "VCToolsInstallDir"]);
        let env = env_of(&mut m, "C:/llvm/bin/clang++.exe", "clang++ a.obj -o a.exe", Some(Style::Gcc));
        assert_eq!(names(&env), ["LIB", "LINK", "VCToolsInstallDir"]);
    }

    /// Options making a tool ignore variables leave them out: the standard
    /// include directories left out, the toolchain named (as
    /// hermetic-llvm-cmake's clang-cl and lld-link flags do), the
    /// environment ignored. A developer prompt and a plain shell then give
    /// the same keys.
    #[test]
    fn ignored_variables() {
        let root = ExecRoot::common(&["C:/Users/x/proj/build".into(), "C:/Users/x/proj/src/a.cpp".into()], true);
        let mut m = PathMapper::new(root, r"C:\Users\x\proj\build", true).unwrap();
        let prompt: BTreeMap<&str, &str> = [
            ("INCLUDE", r"C:\Program Files\VS\VC\include;C:\Program Files (x86)\Windows Kits\10\include\ucrt"),
            ("EXTERNAL_INCLUDE", r"C:\Program Files\VS\VC\include"),
            ("VCToolsInstallDir", r"C:\Program Files\VS\VC\Tools\MSVC\14.44.35207\"),
            ("VCINSTALLDIR", r"C:\Program Files\VS\VC\"),
            ("WindowsSdkDir", r"C:\Program Files (x86)\Windows Kits\10\"),
            ("UCRTVersion", "10.0.22621.0"),
            ("LIB", r"C:\Program Files\VS\VC\lib\x64"),
            ("LIBPATH", r"C:\Program Files\VS\VC\lib\x64"),
        ]
        .into_iter()
        .collect();
        let in_prompt = |n: &str| prompt.get(n).map(OsString::from);
        let plain = |_: &str| None;
        let hermetic = [
            ("C:/llvm/bin/clang-cl.exe", "clang-cl /c /vctoolsdirC:/tc/msvc /winsdkdirC:/tc/sdk /winsdkversion10.0.22621.0 a.cpp", Some(Style::Msvc)),
            ("C:/llvm/bin/clang-cl.exe", "clang-cl /c /winsysroot C:/tc a.cpp", Some(Style::Msvc)),
            ("C:/llvm/bin/lld-link.exe", "lld-link /LIBPATH:C:/tc/lib /lldignoreenv /vctoolsdir:C:/tc/msvc a.obj", None),
            ("C:/llvm/bin/lld-link.exe", "lld-link -LLDIGNOREENV -winsysroot:C:/tc a.obj", None),
            ("C:/llvm/bin/clang-cl.exe", "clang-cl /vctoolsdirC:/tc/msvc a.obj /link /lldignoreenv", Some(Style::Msvc)),
            ("C:/vs/cl.exe", "cl /c /X a.cpp", Some(Style::Msvc)),
            ("C:/llvm/bin/clang-cl.exe", "clang-cl /c /X /vctoolsdir C:/tc/msvc a.cpp", Some(Style::Msvc)),
            ("C:/llvm/bin/clang++.exe", "clang++ -nostdinc -Xmicrosoft-visualc-tools-root C:/tc/msvc -c a.cpp", Some(Style::Gcc)),
            ("C:/llvm/bin/clang++.exe", "clang++ -Xmicrosoft-windows-sys-root C:/tc a.obj -o a.exe -Wl,/lldignoreenv", Some(Style::Gcc)),
            ("C:/vs/link.exe", "link a.obj /out:a.exe", None),
        ];
        for (exe, cmd, style) in hermetic {
            let tool = Tool::of(Path::new(exe), &argv(cmd), style, true);
            let (env, refused) = action_env(std::slice::from_ref(&tool), &mut m, &in_prompt, &[]);
            let (bare, _) = action_env(&[tool], &mut m, &plain, &[]);
            // link.exe has no way to ignore LIB.
            let expected: &[&str] = if exe.ends_with("link.exe") && !exe.contains("lld") { &["LIB"] } else { &[] };
            assert_eq!((names(&env), refused), (expected.to_vec(), None), "{cmd}");
            assert!(bare.is_empty(), "{cmd}");
        }
        // Without them, the prompt's variables are the action's.
        let tool = Tool::of(Path::new("C:/llvm/bin/clang-cl.exe"), &argv("clang-cl /c a.cpp"), Some(Style::Msvc), true);
        assert_eq!(names(&action_env(&[tool], &mut m, &in_prompt, &[]).0), ["EXTERNAL_INCLUDE", "INCLUDE", "VCINSTALLDIR", "VCToolsInstallDir"]);
        // Only `/vctoolsdir` makes Clang ignore INCLUDE: `/winsdkdir` alone
        // names the SDK, which no variable gives anyway.
        let tool = Tool::of(Path::new("C:/llvm/bin/clang-cl.exe"), &argv("clang-cl /c /winsdkdirC:/tc/sdk a.cpp"), Some(Style::Msvc), true);
        assert_eq!(names(&action_env(&[tool], &mut m, &in_prompt, &[]).0), ["EXTERNAL_INCLUDE", "INCLUDE", "VCINSTALLDIR", "VCToolsInstallDir"]);
    }

    /// lld-link's `/lldignoreenv` in a response file: it then ignores `LIB`
    /// and where Visual Studio is, but still reads `LINK` and `_LINK_`,
    /// which it adds before reading response files. On its command line:
    /// none of them. A driver passes it on its linker's command line.
    #[test]
    fn lld_link_ignoring_the_environment() {
        let root = ExecRoot::common(&["C:/Users/x/proj/build".into()], true);
        let mut m = PathMapper::new(root, r"C:\Users\x\proj\build", true).unwrap();
        let prompt: BTreeMap<&str, &str> =
            [("LIB", r"C:\VS\lib"), ("LINK", "/DEBUG"), ("_LINK_", "/OPT:REF"), ("VCToolsInstallDir", r"C:\VS\")].into_iter().collect();
        let vars = |n: &str| prompt.get(n).map(OsString::from);
        let lld = Path::new("C:/llvm/bin/lld-link.exe");
        let names_of = |m: &mut PathMapper, written: &str, read: &str, style| {
            let tool = Tool::of_step(lld, &argv(written), &argv(read), style, true, &|_| true);
            names(&action_env(&[tool], m, &vars, &[]).0).iter().map(|s| s.to_string()).collect::<Vec<_>>()
        };
        assert_eq!(names_of(&mut m, "lld-link @link.rsp", "lld-link /lldignoreenv a.obj", None), ["LINK", "_LINK_"]);
        let tool = Tool::of_step(lld, &argv("lld-link @link.rsp"), &argv("lld-link /lldignoreenv a.obj"), None, true, &|_| true);
        assert_eq!(env_options(&tool, &vars), (vec!["/DEBUG".to_string()], vec!["/OPT:REF".to_string()]));
        // `/winsysroot` names Visual Studio, and leaves `LIB` out too.
        assert_eq!(names_of(&mut m, "lld-link /winsysroot:C:/tc a.obj", "lld-link /winsysroot:C:/tc a.obj", None), ["LINK", "_LINK_"]);
        assert!(names_of(&mut m, "lld-link /lldignoreenv @link.rsp", "lld-link /lldignoreenv a.obj", None).is_empty());
        assert_eq!(names_of(&mut m, "lld-link a.obj", "lld-link a.obj", None), ["LIB", "LINK", "VCToolsInstallDir", "_LINK_"]);
        // A driver: its linker gets it on its command line; the driver
        // looks for Visual Studio itself.
        let line = "clang-cl @link.rsp";
        let tool =
            Tool::of_step(Path::new("C:/llvm/bin/clang-cl.exe"), &argv(line), &argv("clang-cl a.obj /link /lldignoreenv"), Some(Style::Msvc), true, &|_| true);
        assert_eq!(names(&action_env(&[tool], &mut m, &vars, &[]).0), ["VCToolsInstallDir"]);
    }

    /// lib.exe and llvm-lib read `LIB` to find an input that isn't found
    /// from the working directory or a `/LIBPATH:`, else not: an archive's
    /// key is the same in a developer prompt.
    #[test]
    fn librarian_lib() {
        let root = ExecRoot::common(&["C:/Users/x/proj/build".into()], true);
        let mut m = PathMapper::new(root, r"C:\Users\x\proj\build", true).unwrap();
        let vars = |n: &str| (n == "LIB").then(|| OsString::from(r"C:\VS\lib"));
        let here = ["a.obj", "objs/b.obj", "C:/Users/x/proj/c.obj"];
        let present = |p: &Path| here.iter().any(|h| Path::new(h) == p);
        let env_of = |m: &mut PathMapper, exe: &str, cmd: &str| {
            let tool = Tool::of_step(Path::new(exe), &argv(cmd), &argv(cmd), None, true, &present);
            names(&action_env(&[tool], m, &vars, &[]).0).iter().map(|s| s.to_string()).collect::<Vec<_>>()
        };
        for (exe, name) in [("C:/llvm/bin/llvm-lib.exe", "llvm-lib"), ("C:/VS/bin/lib.exe", "lib")] {
            assert!(env_of(&mut m, exe, &format!("{name} /nologo /out:x.lib a.obj C:/Users/x/proj/c.obj")).is_empty(), "{name}");
            assert!(env_of(&mut m, exe, &format!("{name} /out:x.lib /LIBPATH:objs b.obj")).is_empty(), "{name}");
            assert_eq!(env_of(&mut m, exe, &format!("{name} /out:x.lib a.obj kernel32.lib")), ["LIB"], "{name}");
        }
    }

    /// The options `CL` and `LINK` add, for the tools that read them.
    #[test]
    fn options_from_the_environment() {
        let host: BTreeMap<&str, &str> =
            [("CL", "/Iextra /DX"), ("_CL_", "/FIpre.h"), ("LINK", "/LIBPATH:lib"), ("_LINK_", "/DEF:a.def")].into_iter().collect();
        let vars = |n: &str| host.get(n).map(OsString::from);
        let of = |exe: &str, cmd: &str, style| Tool::of(Path::new(exe), &argv(cmd), style, true);
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(env_options(&of("C:/vs/cl.exe", "cl /c a.cpp", Some(Style::Msvc)), &vars), (s(&["/Iextra", "/DX"]), s(&["/FIpre.h"])));
        assert_eq!(env_options(&of("C:/vs/link.exe", "link a.obj", None), &vars), (s(&["/LIBPATH:lib"]), s(&["/DEF:a.def"])));
        assert_eq!(env_options(&of("C:/llvm/bin/lld-link.exe", "lld-link /lldignoreenv a.obj", None), &vars), Default::default());
        assert_eq!(env_options(&of("C:/llvm/bin/clang.exe", "clang -c a.c", Some(Style::Gcc)), &vars), Default::default());
    }
}
