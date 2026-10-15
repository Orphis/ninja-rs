//! The environment variables tools read that change what they write: a
//! command run locally inherits them, a remote action only has its own.
//! Those the action's tools read go into its environment, with paths
//! rewritten as in its arguments (relative to the working directory when
//! under the exec root, else left as the host names them, for a worker of
//! the same OS), so that its key describes them and remote runs get them.
//! Variables of other tools are left out: they change nothing. A variable
//! that can't be carried (one making a compiler write a file the action
//! doesn't return) keeps a local result from being stored.
//!
//! `--action_env` sets variables in every action, as Bazel's does, without
//! rewriting; a variable it names is carried, never refused.

use std::collections::BTreeMap;
use std::path::Path;

use crate::cmdline::{join_rsp, split_rsp};
use crate::paths::PathMapper;

/// The kinds of tools whose variables are known, from their name and
/// command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
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

/// cl.exe and clang-cl: include directories (clang also reads them for
/// its MSVC targets), options added to every command line.
const CL_COMPILE: &[(&str, Value)] = &[("INCLUDE", Value::MsvcPaths), ("EXTERNAL_INCLUDE", Value::MsvcPaths), ("CL", Value::Options), ("_CL_", Value::Options)];

/// Where clang finds Visual Studio and the Windows SDK, when the include
/// and library variables aren't set.
const VS_LOCATIONS: &[(&str, Value)] = &[
    ("VCToolsInstallDir", Value::Path),
    ("VCINSTALLDIR", Value::Path),
    ("WindowsSdkDir", Value::Path),
    ("WindowsSDKVersion", Value::Plain),
    ("UniversalCRTSdkDir", Value::Path),
    ("UCRTVersion", Value::Plain),
];

/// GCC and Clang drivers compiling: include directories, where the driver
/// finds its programs, reproducible timestamps, Apple SDKs.
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
    ("MACOSX_DEPLOYMENT_TARGET", Value::Plain),
    ("IPHONEOS_DEPLOYMENT_TARGET", Value::Plain),
];

/// GCC's dependency output: files the action doesn't return.
const GNU_COMPILE_REFUSED: &[&str] = &["DEPENDENCIES_OUTPUT", "SUNPRO_DEPENDENCIES"];

/// MSVC linkers: library directories, options added to every command line.
const MSVC_LINK: &[(&str, Value)] = &[("LIB", Value::MsvcPaths), ("LIBPATH", Value::MsvcPaths), ("LINK", Value::Options), ("_LINK_", Value::Options)];

/// GNU-style links: library directories, where the driver finds its
/// programs, Apple SDKs and timestamps.
const GNU_LINK: &[(&str, Value)] = &[
    ("LIBRARY_PATH", Value::Paths),
    ("GCC_EXEC_PREFIX", Value::Path),
    ("COMPILER_PATH", Value::Paths),
    ("CCC_OVERRIDE_OPTIONS", Value::Plain),
    ("SDKROOT", Value::Path),
    ("DEVELOPER_DIR", Value::Path),
    ("MACOSX_DEPLOYMENT_TARGET", Value::Plain),
    ("IPHONEOS_DEPLOYMENT_TARGET", Value::Plain),
    ("ZERO_AR_DATE", Value::Plain),
];

/// Archivers: Apple's timestamps; lib.exe's library directories.
const AR: &[(&str, Value)] = &[("ZERO_AR_DATE", Value::Plain)];
const LIB: &[(&str, Value)] = &[("LIB", Value::MsvcPaths)];

impl Tool {
    /// The kind of `exe` running `argv` (response files expanded):
    /// compiling (`style`, the scanner's), linking, archiving.
    pub fn of(exe: &Path, argv: &[String], style: Option<crate::scanner::Style>) -> Tool {
        let name = exe.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let clang = name.contains("clang");
        let cl = style == Some(crate::scanner::Style::Msvc);
        match (style, crate::link::linker_flavor(&exe.to_string_lossy())) {
            (Some(_), _) if crate::link::driver_compiles(argv) => {
                if cl {
                    Tool::ClCompile { clang }
                } else {
                    Tool::GnuCompile { clang }
                }
            }
            (Some(_), _) if cl => Tool::MsvcLink { clang, driver: true },
            (Some(_), _) => Tool::GnuLink { clang },
            (None, Some(crate::link::Flavor::Msvc)) => Tool::MsvcLink { clang: name == "lld-link", driver: false },
            (None, Some(crate::link::Flavor::Gnu)) => Tool::GnuLink { clang: false },
            (None, None) if name == "lib" || name == "llvm-lib" => Tool::Lib,
            (None, None) if name.ends_with("ar") || name == "libtool" => Tool::Ar,
            _ => Tool::Other,
        }
    }

    /// The variables it reads, and how they're carried. `windows`: clang
    /// on a Windows host targets MSVC by default, reading MSVC's too.
    fn variables(self, windows: bool) -> Vec<(&'static str, Value)> {
        let mut vars = Vec::new();
        match self {
            Tool::ClCompile { clang } => {
                vars.extend_from_slice(CL_COMPILE);
                if clang {
                    vars.extend_from_slice(VS_LOCATIONS);
                }
            }
            Tool::GnuCompile { clang } => {
                vars.extend_from_slice(GNU_COMPILE);
                if clang && windows {
                    vars.extend_from_slice(&CL_COMPILE[..2]);
                    vars.extend_from_slice(VS_LOCATIONS);
                }
            }
            Tool::MsvcLink { clang, driver } => {
                vars.extend_from_slice(MSVC_LINK);
                if driver {
                    vars.extend_from_slice(&CL_COMPILE[2..]);
                }
                if clang {
                    vars.extend_from_slice(VS_LOCATIONS);
                }
            }
            Tool::GnuLink { clang } => {
                vars.extend_from_slice(GNU_LINK);
                if clang && windows {
                    vars.extend_from_slice(MSVC_LINK);
                    vars.extend_from_slice(VS_LOCATIONS);
                }
            }
            Tool::Ar => vars.extend_from_slice(AR),
            Tool::Lib => vars.extend_from_slice(LIB),
            Tool::Other => {}
        }
        vars
    }

    fn refused(self) -> &'static [&'static str] {
        match self {
            Tool::GnuCompile { .. } => GNU_COMPILE_REFUSED,
            _ => &[],
        }
    }
}

/// An action's environment: the variables its tools read, from `vars`
/// (the build's environment), then `action_env`. And the first variable
/// set in `vars` that a tool reads and the action can't carry, if any.
pub fn action_env(
    tools: &[Tool],
    mapper: &mut PathMapper,
    vars: &dyn Fn(&str) -> Option<String>,
    action_env: &[(String, String)],
) -> (BTreeMap<String, String>, Option<String>) {
    let windows = mapper.windows;
    let mut env = BTreeMap::new();
    let mut refused = None;
    for tool in tools {
        for (name, kind) in tool.variables(windows) {
            if env.contains_key(name) {
                continue;
            }
            if let Some(value) = vars(name) {
                env.insert(name.to_string(), carried(&value, kind, mapper));
            }
        }
        if refused.is_none() {
            refused = tool.refused().iter().find(|n| vars(n).is_some()).map(|n| n.to_string());
        }
    }
    let same = |a: &str, b: &str| if windows { a.eq_ignore_ascii_case(b) } else { a == b };
    for (name, value) in action_env {
        env.retain(|n, _| !same(n, name));
        env.insert(name.clone(), value.clone());
    }
    let refused = refused.filter(|r| !env.keys().any(|n| same(n, r)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ExecRoot;
    use crate::scanner::Style;

    fn argv(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn tools() {
        let of = |exe: &str, cmd: &str, style| Tool::of(Path::new(exe), &argv(cmd), style);
        assert_eq!(of("/usr/bin/gcc", "gcc -c a.c", Some(Style::Gcc)), Tool::GnuCompile { clang: false });
        assert_eq!(of("/usr/bin/clang++", "clang++ a.o -o app", Some(Style::Gcc)), Tool::GnuLink { clang: true });
        assert_eq!(of("C:/llvm/bin/clang-cl.exe", "clang-cl /c a.c", Some(Style::Msvc)), Tool::ClCompile { clang: true });
        assert_eq!(of("C:/vs/cl.exe", "cl a.obj /link /out:a.exe", Some(Style::Msvc)), Tool::MsvcLink { clang: false, driver: true });
        assert_eq!(of("C:/llvm/bin/lld-link.exe", "lld-link a.obj", None), Tool::MsvcLink { clang: true, driver: false });
        assert_eq!(of("/usr/bin/ld.lld", "ld.lld a.o", None), Tool::GnuLink { clang: false });
        assert_eq!(of("/usr/bin/llvm-ar", "llvm-ar qc a.a a.o", None), Tool::Ar);
        assert_eq!(of("C:/llvm/bin/llvm-lib.exe", "llvm-lib /out:a.lib a.obj", None), Tool::Lib);
        assert_eq!(of("/usr/bin/python3", "python3 gen.py", None), Tool::Other);
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
        ]
        .into_iter()
        .collect();
        let vars = |n: &str| host.get(n).map(|v| v.to_string());
        let (env, refused) = action_env(&[Tool::GnuCompile { clang: true }], &mut m, &vars, &[]);
        assert_eq!(
            env.into_iter().collect::<Vec<_>>(),
            [("CPATH".into(), "../include::/opt/inc".into()), ("SDKROOT".into(), "../sdk".into()), ("SOURCE_DATE_EPOCH".into(), "0".into())]
        );
        assert_eq!(refused.as_deref(), Some("DEPENDENCIES_OUTPUT"));
        let (env, refused) = action_env(&[Tool::GnuLink { clang: true }], &mut m, &vars, &[]);
        assert_eq!(env.keys().collect::<Vec<_>>(), ["LIBRARY_PATH", "SDKROOT"]);
        assert_eq!(env["LIBRARY_PATH"], "../lib");
        assert_eq!(refused, None);
        let given = [("DEPENDENCIES_OUTPUT".to_string(), "deps.txt".to_string()), ("SDKROOT".into(), "/sdk".into()), ("FOO".into(), "bar".into())];
        let (env, refused) = action_env(&[Tool::GnuCompile { clang: false }, Tool::Other], &mut m, &vars, &given);
        assert_eq!((env["SDKROOT"].as_str(), env["FOO"].as_str(), env["DEPENDENCIES_OUTPUT"].as_str()), ("/sdk", "bar", "deps.txt"));
        assert_eq!(refused, None);
        assert!(action_env(&[Tool::Other], &mut m, &vars, &[]).0.is_empty());
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
            ("LINK", r"/DEF:C:\Users\x\proj\src\a.def"),
            ("VCToolsInstallDir", r"C:\VS\"),
        ]
        .into_iter()
        .collect();
        let vars = |n: &str| host.get(n).map(|v| v.to_string());
        let (env, _) = action_env(&[Tool::ClCompile { clang: false }], &mut m, &vars, &[]);
        assert_eq!(env.keys().collect::<Vec<_>>(), ["CL", "INCLUDE"]);
        assert_eq!(env["INCLUDE"], r"..\inc;C:\VS\include");
        // Split and joined again as cl does: the definition keeps its path.
        assert_eq!(env["CL"], r"/DDATA=C:\Users\x\proj\data /I..\extra");
        let (env, _) = action_env(&[Tool::MsvcLink { clang: true, driver: false }], &mut m, &vars, &[]);
        assert_eq!(env.keys().collect::<Vec<_>>(), ["LIB", "LINK", "VCToolsInstallDir"]);
        assert_eq!(env["LINK"], r"/DEF:..\src\a.def");
        // clang on a Windows host reads MSVC's include directories too.
        let (env, _) = action_env(&[Tool::GnuCompile { clang: true }], &mut m, &vars, &[]);
        assert_eq!(env.keys().collect::<Vec<_>>(), ["INCLUDE", "VCToolsInstallDir"]);
    }
}
