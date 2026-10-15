//! What a link reads and writes beyond its declared inputs and outputs: the
//! linker a compiler driver runs, the library directories it searches (the
//! toolchain's are sent whole: which of their libraries a link pulls in
//! depends on directives inside objects and libraries), files named by
//! flags (overlays, linker scripts, `.def` files), and side outputs such as
//! the PDB and the import library.
//!
//! A compiler driver doesn't say which linker it runs or which directories
//! it adds: its dry run (`-###`) prints the linker's command line, which is
//! read instead of the driver's. An MSVC linker also takes options from
//! `LINK` and `_LINK_`, and searches `LIB`'s directories, unless lld-link
//! is told to ignore the environment: the files it reads there are sent
//! (see `env_files_read`), not the others, which a developer prompt fills
//! with Visual Studio's and the SDK's.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The linker command-line conventions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// `link.exe`, `lld-link`: `/libpath:`, `/out:`, `/pdb:`...
    Msvc,
    /// `ld`, `ld.lld`, `ld64.lld`, `wasm-ld`: `-L`, `-o`, `-T`...
    Gnu,
}

#[derive(Debug, Default, PartialEq)]
pub struct LinkInfo {
    /// The linker a driver runs, as it names it (absolute).
    pub linker: Option<PathBuf>,
    /// Files the link reads that may not be declared inputs (absolute).
    pub files: Vec<PathBuf>,
    /// Library search directories (absolute).
    pub lib_dirs: Vec<PathBuf>,
    /// Side outputs (PDB, import library, map), as written on the command
    /// line: relative to the working directory, or absolute.
    pub outputs: Vec<String>,
}

/// The flavor of a linker executable, from its name.
pub fn linker_flavor(exe: &str) -> Option<Flavor> {
    let base = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    match base {
        "lld-link" | "link" => Some(Flavor::Msvc),
        "ld" | "ld.lld" | "ld.bfd" | "ld.gold" | "ld.mold" | "ld64" | "ld64.lld" | "wasm-ld" | "mold" => Some(Flavor::Gnu),
        _ => None,
    }
}

/// Whether a compiler driver command line compiles (rather than links).
pub fn driver_compiles(argv: &[String]) -> bool {
    argv.iter().skip(1).any(|a| matches!(a.as_str(), "-c" | "/c" | "-S" | "/S" | "-E" | "/E" | "/P" | "-fsyntax-only" | "/Zs"))
}

/// Analyzes a link: `exe` (absolute, as the command names it) is either a
/// compiler driver, whose dry run gives the linker's command line, or a
/// linker. For a driver, `argv` are the arguments as the command has them
/// (it reads its response files itself); for a linker, as the linker reads
/// them (response files expanded, with what `LINK` and `_LINK_` add, see
/// `env::env_options`). Run from `cwd` with the variables `vars` gives;
/// `directives` gives those of objects and libraries (see
/// `env_files_read`).
pub fn analyze(
    exe: &Path,
    argv: &[String],
    cwd: &Path,
    driver: bool,
    vars: &dyn Fn(&str) -> Option<OsString>,
    directives: &dyn Fn(&Path) -> Option<Arc<Vec<String>>>,
) -> Result<LinkInfo, String> {
    let (linker, mut args) = if driver {
        let job = driver_link_job(exe, &argv[1..], cwd)?;
        let linker = abs(&job[0], cwd);
        let linker = if linker.is_file() || cfg!(not(windows)) { linker } else { linker.with_extension("exe") };
        (Some(linker), job[1..].to_vec())
    } else {
        (None, argv[1..].to_vec())
    };
    let name = linker.as_ref().map(|l| l.to_string_lossy().into_owned()).unwrap_or_else(|| exe.to_string_lossy().into_owned());
    let Some(flavor) = linker_flavor(&name) else {
        return Err(format!("unrecognized linker '{name}'"));
    };
    // What the environment adds: options before and after the command
    // line's (a driver's linker; a linker's are in `argv`), then
    // directories searched after `/libpath:`'s.
    let var = |n: &str| vars(n).map(|v| v.to_string_lossy().into_owned());
    if flavor == Flavor::Msvc && driver && !ignores_environment(&name, &args, true) {
        let mut with_env = var("LINK").map(|v| crate::cmdline::split_rsp(&v, true)).unwrap_or_default();
        with_env.append(&mut args);
        with_env.extend(var("_LINK_").map(|v| crate::cmdline::split_rsp(&v, true)).unwrap_or_default());
        args = with_env;
    }
    let mut info = parse_linker_args(&args, flavor, cwd);
    if flavor == Flavor::Msvc && !ignores_environment(&name, &args, false) {
        let env: Vec<PathBuf> = var("LIB").iter().flat_map(|v| v.split(';')).filter(|d| !d.is_empty()).map(|d| abs(d, cwd)).collect();
        if !env.is_empty() {
            match env_files_read(&args, cwd, &env, directives) {
                Some(files) => info.files.extend(files),
                None => info.lib_dirs.extend(env),
            }
        }
    }
    info.files.sort();
    info.files.dedup();
    info.lib_dirs.dedup();
    info.linker = linker;
    Ok(info)
}

/// Whether an MSVC linker ignores the environment: lld-link's `LIB` under
/// `/lldignoreenv` or `/winsysroot`, its `LINK` and `_LINK_` (`options`)
/// under `/lldignoreenv`. link.exe always reads them.
fn ignores_environment(linker: &str, args: &[String], options: bool) -> bool {
    let base = linker.rsplit(['/', '\\']).next().unwrap_or(linker).to_ascii_lowercase();
    let has = |name: &str| {
        args.iter().any(|a| a.strip_prefix(['/', '-']).map(|o| o.split_once(':').map_or(o, |(n, _)| n)).is_some_and(|o| o.eq_ignore_ascii_case(name)))
    };
    base.strip_suffix(".exe").unwrap_or(&base) == "lld-link" && (has("lldignoreenv") || (!options && has("winsysroot")))
}

/// A library's file name: `.lib` added when it has no extension, as the
/// linkers look for it.
fn lib_file(name: &str) -> String {
    if name.contains('.') { name.to_string() } else { format!("{name}.lib") }
}

/// The files of `LIB`'s directories (`env`, in order) an MSVC link reads,
/// given its arguments `args` (run from `cwd`): it looks for each input its
/// command line names, each library `/defaultlib:` names, and each its
/// inputs' and libraries' directives ask for (see `directives`; but those
/// `/nodefaultlib` leaves out), in the working directory, then in
/// `/libpath:`'s, then in `LIB`'s, as lld-link and link.exe do: those found
/// first in `LIB`'s are read, not the others there (a developer prompt's
/// `LIB` names hundreds of MB of libraries). None (all of them, whole) when
/// that can't be told: a file whose directives can't be read (LTO bitcode,
/// an output left in the remote CAS), or one not found. (Directives' own
/// `/nodefaultlib` is ignored: more is sent, never less.)
fn env_files_read(args: &[String], cwd: &Path, env: &[PathBuf], directives: &dyn Fn(&Path) -> Option<Arc<Vec<String>>>) -> Option<Vec<PathBuf>> {
    let mut dirs = vec![cwd.to_path_buf()];
    let mut no_default_all = false;
    let mut no_default = std::collections::HashSet::new();
    // (name, whether a library).
    let mut queue: Vec<(String, bool)> = Vec::new();
    for a in args {
        // An absolute POSIX path is a file, not an option.
        let Some(opt) = a.strip_prefix(['/', '-']).filter(|_| !(a.starts_with('/') && Path::new(a).is_file())) else {
            queue.push((a.clone(), false));
            continue;
        };
        let (name, value) = opt.split_once(':').map_or((opt, ""), |(n, v)| (n, v.trim_matches('"')));
        match (name.to_ascii_lowercase().as_str(), value.is_empty()) {
            ("libpath", false) => dirs.push(abs(value, cwd)),
            ("defaultlib", false) => queue.push((value.to_string(), true)),
            ("wholearchive", false) => queue.push((value.to_string(), false)),
            ("nodefaultlib", true) => no_default_all = true,
            ("nodefaultlib", false) => {
                no_default.insert(lib_file(value).to_ascii_lowercase());
            }
            _ => {}
        }
    }
    let mut read = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some((name, lib)) = queue.pop() {
        let name = if lib { lib_file(&name) } else { name };
        if lib && (no_default_all || no_default.contains(&name.to_ascii_lowercase())) {
            continue;
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            continue;
        }
        // An input named without an extension may be an object.
        let names = std::iter::once(name.clone()).chain((!lib && !name.contains('.')).then(|| format!("{name}.obj"))).collect::<Vec<_>>();
        let searched = dirs.iter().map(|d| (false, d)).chain(env.iter().map(|d| (true, d)));
        let (in_env, path) = searched.into_iter().find_map(|(in_env, d)| names.iter().map(|n| d.join(n)).find(|p| p.is_file()).map(|p| (in_env, p)))?;
        let options = directives(&path)?;
        if in_env {
            read.push(path);
        }
        queue.extend(crate::directives::default_libs(&options).map(|l| (l.to_string(), true)));
    }
    read.sort();
    Some(read)
}

/// Runs the driver's dry run and returns the last job it prints: the link.
fn driver_link_job(exe: &Path, args: &[String], cwd: &Path) -> Result<Vec<String>, String> {
    let out = std::process::Command::new(exe)
        // First: arguments after clang-cl's /link go to the linker.
        .arg("-###")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("running {} -###: {e}", exe.display()))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let job = text.lines().rfind(|l| l.starts_with(" \"")).ok_or_else(|| format!("{} -### printed no job", exe.display()))?;
    let tokens = split_quoted(job);
    if tokens.is_empty() {
        return Err(format!("{} -### printed an empty job", exe.display()));
    }
    Ok(tokens)
}

/// Splits a job line of `-###` output: arguments in double quotes, with
/// backslash escapes.
fn split_quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut arg = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        arg.push(n);
                    }
                }
                '"' => break,
                _ => arg.push(c),
            }
        }
        out.push(arg);
    }
    out
}

fn abs(p: &str, cwd: &Path) -> PathBuf {
    crate::scanner::normalize(&cwd.join(p))
}

fn parse_linker_args(args: &[String], flavor: Flavor, cwd: &Path) -> LinkInfo {
    let mut info = LinkInfo::default();
    let file = |info: &mut LinkInfo, p: &str| {
        let p = abs(p, cwd);
        if p.is_file() {
            info.files.push(p);
        }
    };
    match flavor {
        Flavor::Msvc => {
            for a in args {
                let Some(opt) = a.strip_prefix(['/', '-']) else {
                    file(&mut info, a);
                    continue;
                };
                // An absolute POSIX path is a file, not an option.
                if a.starts_with('/') && Path::new(a).is_file() {
                    file(&mut info, a);
                    continue;
                }
                let (name, value) = match opt.split_once(':') {
                    Some((n, v)) => (n.to_ascii_lowercase(), v),
                    None => (opt.to_ascii_lowercase(), ""),
                };
                let value = value.trim_matches('"');
                match name.as_str() {
                    "libpath" => info.lib_dirs.push(abs(value, cwd)),
                    "vfsoverlay" | "def" | "manifestinput" | "natvis" | "wholearchive" => file(&mut info, value),
                    "order" => file(&mut info, value.trim_start_matches('@')),
                    "pdb" | "implib" | "map" if !value.is_empty() => info.outputs.push(value.to_string()),
                    _ => {}
                }
            }
        }
        Flavor::Gnu => {
            let mut i = 0;
            while i < args.len() {
                let a = args[i].as_str();
                i += 1;
                let value_of = |flags: &[&str], i: &mut usize| -> Option<String> {
                    for f in flags {
                        if a == *f {
                            *i += 1;
                            return args.get(*i - 1).cloned();
                        }
                        if let Some(v) = a.strip_prefix(f).and_then(|v| if f.starts_with("--") || f.len() > 2 { v.strip_prefix('=') } else { Some(v) })
                            && !v.is_empty()
                        {
                            return Some(v.to_string());
                        }
                    }
                    None
                };
                if let Some(v) = value_of(&["-L", "--library-path"], &mut i) {
                    info.lib_dirs.push(abs(&v, cwd));
                } else if let Some(v) = value_of(&["-T", "--script", "--version-script", "--dynamic-list"], &mut i) {
                    file(&mut info, &v);
                } else if let Some(v) = value_of(&["-Map", "--Map", "--out-implib"], &mut i) {
                    info.outputs.push(v);
                } else if value_of(&["-o", "-l", "-m", "-z", "-e", "-u", "--sysroot", "-soname", "-rpath", "-plugin", "-plugin-opt", "-dynamic-linker"], &mut i)
                    .is_some()
                {
                    // Values that aren't input files.
                } else if !a.starts_with('-') {
                    file(&mut info, a);
                }
            }
        }
    }
    info.files.sort();
    info.files.dedup();
    info.lib_dirs.sort();
    info.lib_dirs.dedup();
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavors() {
        assert_eq!(linker_flavor("C:\\llvm\\bin\\lld-link.exe"), Some(Flavor::Msvc));
        assert_eq!(linker_flavor("/usr/bin/ld.lld"), Some(Flavor::Gnu));
        assert_eq!(linker_flavor("clang"), None);
        assert!(driver_compiles(&["clang".into(), "-c".into(), "a.c".into()]));
        assert!(!driver_compiles(&["clang".into(), "a.o".into(), "-o".into(), "app".into()]));
    }

    #[test]
    fn dry_run_lines() {
        let line = r#" "C:\\llvm\\bin\\lld-link" "-out:app.exe" "-libpath:C:\\sdk\\lib" "a \"b\".obj""#;
        assert_eq!(split_quoted(line), vec![r"C:\llvm\bin\lld-link", "-out:app.exe", r"-libpath:C:\sdk\lib", "a \"b\".obj"]);
    }

    #[test]
    fn msvc_arguments() {
        let dir = test_dirs::TestDir::new("ninja-rs-link");
        std::fs::write(dir.join("a.obj"), "").unwrap();
        std::fs::write(dir.join("over.yaml"), "").unwrap();
        let args: Vec<String> =
            ["-out:app.exe", "a.obj", "kernel32.lib", "/LIBPATH:lib/x64", "-pdb:app.pdb", "/implib:app.lib", "/vfsoverlay:over.yaml", "-defaultlib:libcmt"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let info = parse_linker_args(&args, Flavor::Msvc, &dir);
        assert_eq!(info.files, vec![dir.join("a.obj"), dir.join("over.yaml")]);
        assert_eq!(info.lib_dirs, vec![dir.join("lib").join("x64")]);
        assert_eq!(info.outputs, vec!["app.pdb", "app.lib"]);
    }

    fn read_directives(p: &Path) -> Option<Arc<Vec<String>>> {
        crate::directives::read(p).ok().flatten().map(Arc::new)
    }

    /// An MSVC linker's command line as it reads it (with the options of
    /// `LINK` and `_LINK_`, which `env` adds), and `LIB`'s directories: the
    /// files they name are the link's, and the libraries it may pull in
    /// (all of `LIB`'s, when its objects' directives can't be read). Not
    /// when lld-link ignores them.
    #[test]
    fn msvc_environment() {
        let dir = test_dirs::TestDir::new("ninja-rs-link-env");
        for f in ["a.obj", "a.def", "order.txt"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let vars = |n: &str| match n {
            "LINK" => Some(OsString::from("/DEF:a.def")),
            "LIB" => Some(OsString::from("sdk/um;;vc/lib")),
            _ => None,
        };
        let argv = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        for exe in ["link.exe", "lld-link.exe"] {
            let line = argv(&format!("{exe} /DEF:a.def a.obj /OUT:app.exe /LIBPATH:lib /ORDER:@order.txt /PDB:app.pdb"));
            let info = analyze(&dir.join(exe), &line, &dir, false, &vars, &read_directives).unwrap();
            assert_eq!(info.files, [dir.join("a.def"), dir.join("a.obj"), dir.join("order.txt")], "{exe}");
            assert_eq!(info.lib_dirs, [dir.join("lib"), dir.join("sdk").join("um"), dir.join("vc").join("lib")], "{exe}");
            assert_eq!(info.outputs, ["app.pdb"], "{exe}");
            // `LINK` is read into the command line before (see `env`).
            let info = analyze(&dir.join(exe), &argv(&format!("{exe} a.obj")), &dir, false, &vars, &read_directives).unwrap();
            assert_eq!(info.files, [dir.join("a.obj")], "{exe}");
        }
        for ignored in ["-LLDIGNOREENV", "/winsysroot:C:/tc"] {
            let info =
                analyze(&dir.join("lld-link.exe"), &argv(&format!("lld-link a.obj /OUT:app.exe {ignored}")), &dir, false, &vars, &read_directives).unwrap();
            assert_eq!((info.files, info.lib_dirs, info.outputs), (vec![dir.join("a.obj")], vec![], vec![]), "{ignored}");
        }
        // GNU linkers read none of them.
        let info = analyze(&dir.join("ld.lld"), &argv("ld.lld a.obj -o app"), &dir, false, &vars, &read_directives).unwrap();
        assert_eq!((info.files, info.lib_dirs), (vec![dir.join("a.obj")], vec![]));
    }

    /// Of the files of `LIB`'s directories, those a link finds first: a
    /// library its command line names, or the directives of its objects and
    /// libraries ask for (transitively, those of objects for any machine
    /// too, as `msvcrt.lib` holds), unless `/nodefaultlib` leaves it out;
    /// after the working directory and `/libpath:`. All the directories when
    /// an input's directives can't be read, or a library isn't found.
    #[test]
    fn libraries_read_from_lib() {
        use crate::directives::tests::{archive, coff, coff_for};
        let dir = test_dirs::TestDir::new("ninja-rs-link-lib");
        let put = |path: &str, data: &[u8]| {
            std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
            std::fs::write(dir.join(path), data).unwrap();
        };
        let main = coff(&[(".drectve", b" /DEFAULTLIB:\"msvcrt\" /DEFAULTLIB:\"oldnames\"")]);
        let import = [0u8, 0, 0xff, 0xff, 0, 0, 0x64, 0x86, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        put("main.obj", &main);
        put("lto.obj", b"BC\xc0\xde\x35\x14\x00\x00\x05\x00\x00\x00\x62\x0c\x30\x24");
        put("tc/lib/oldnames.lib", &archive(&[]));
        put("vc/dup/oldnames.lib", &archive(&[]));
        let crt = coff_for(0, &[(".drectve", b"-defaultlib:vcruntime.lib")]);
        put("vc/lib/msvcrt.lib", &archive(&[("msvcrt.dll/", &import), ("crt.obj/", &crt)]));
        put("vc/lib/unused.lib", &archive(&[]));
        put("vc/dup/msvcrt.lib", &archive(&[]));
        put("vc/rt/vcruntime.lib", &archive(&[]));
        put("sdk/um/kernel32.lib", &archive(&[("KERNEL32.dll/", &import)]));
        put("sdk/net/mscoree.lib", &archive(&[]));
        let env: Vec<PathBuf> = ["vc/dup2", "sdk/net", "vc/lib", "vc/rt", "sdk/um", "vc/dup"].iter().map(|d| dir.join(d)).collect();
        let rel = |p: &PathBuf| p.strip_prefix(&*dir).unwrap().to_string_lossy().replace('\\', "/");
        let read = |line: &str| {
            let args: Vec<String> = line.split(' ').map(str::to_string).collect();
            env_files_read(&args, &dir, &env, &read_directives).map(|files| files.iter().map(rel).collect::<Vec<_>>())
        };
        let files = |f: &[&str]| Some(f.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(read("main.obj kernel32.lib /LIBPATH:tc/lib /OUT:app.exe"), files(&["sdk/um/kernel32.lib", "vc/lib/msvcrt.lib", "vc/rt/vcruntime.lib"]));
        assert_eq!(read("main /LIBPATH:tc/lib kernel32.lib /NODEFAULTLIB:msvcrt"), files(&["sdk/um/kernel32.lib"]));
        assert_eq!(read("main.obj /NODEFAULTLIB /DEFAULTLIB:kernel32"), files(&[]));
        assert_eq!(read("main.obj /NODEFAULTLIB:MSVCRT.lib"), files(&["vc/dup/oldnames.lib"]));
        // What can't be told.
        assert_eq!(read("lto.obj /LIBPATH:tc/lib"), None);
        assert_eq!(read("main.obj missing.lib /LIBPATH:tc/lib"), None);
        // Through `analyze`: those files, after `/libpath:`'s directory; all
        // of `LIB`'s directories, whole, when it can't be told.
        let vars = |n: &str| (n == "LIB").then(|| OsString::from(env.iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>().join(";")));
        let line: Vec<String> = "link.exe main.obj kernel32.lib /LIBPATH:tc/lib".split(' ').map(str::to_string).collect();
        let info = analyze(&dir.join("link.exe"), &line, &dir, false, &vars, &read_directives).unwrap();
        assert_eq!(info.lib_dirs, [dir.join("tc").join("lib")]);
        assert_eq!(info.files.iter().map(rel).collect::<Vec<_>>(), ["main.obj", "sdk/um/kernel32.lib", "vc/lib/msvcrt.lib", "vc/rt/vcruntime.lib"]);
        let line: Vec<String> = "link.exe lto.obj kernel32.lib /LIBPATH:tc/lib".split(' ').map(str::to_string).collect();
        let info = analyze(&dir.join("link.exe"), &line, &dir, false, &vars, &read_directives).unwrap();
        assert_eq!(info.lib_dirs.len(), 1 + env.len());
        assert_eq!(info.files.iter().map(rel).collect::<Vec<_>>(), ["lto.obj"]);
    }

    #[test]
    fn gnu_arguments() {
        let dir = test_dirs::TestDir::new("ninja-rs-link-gnu");
        std::fs::write(dir.join("crt1.o"), "").unwrap();
        std::fs::write(dir.join("v.map"), "").unwrap();
        let args: Vec<String> = ["-o", "app", "crt1.o", "-L/sys/lib", "-L", "lib", "-lc", "--version-script=v.map", "-Map", "app.map", "-z", "now"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let info = parse_linker_args(&args, Flavor::Gnu, &dir);
        assert_eq!(info.files, vec![dir.join("crt1.o"), dir.join("v.map")]);
        assert_eq!(info.lib_dirs.len(), 2);
        assert_eq!(info.outputs, vec!["app.map"]);
    }
}
