//! What a link reads and writes beyond its declared inputs and outputs: the
//! linker a compiler driver runs, the library directories it searches (the
//! toolchain's are sent whole: which of their libraries a link pulls in
//! depends on directives inside objects and libraries), files named by
//! flags (overlays, linker scripts, `.def` files), and side outputs such as
//! the PDB and the import library.
//!
//! A compiler driver doesn't say which linker it runs or which directories
//! it adds: its dry run (`-###`) prints the linker's command line, which is
//! read instead of the driver's.

use std::path::{Path, PathBuf};

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
/// linker. `argv` are the arguments as the command has them (response
/// files included), run from `cwd`.
pub fn analyze(exe: &Path, argv: &[String], cwd: &Path, driver: bool) -> Result<LinkInfo, String> {
    let (linker, args) = if driver {
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
    let mut info = parse_linker_args(&args, flavor, cwd);
    info.linker = linker;
    Ok(info)
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
    let job = text.lines().filter(|l| l.starts_with(" \"")).next_back().ok_or_else(|| format!("{} -### printed no job", exe.display()))?;
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
                } else if value_of(&["-o", "-l", "-m", "-z", "-e", "-u", "--sysroot", "-soname", "-rpath", "-plugin", "-plugin-opt", "-dynamic-linker"], &mut i).is_some() {
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
        let dir = std::env::temp_dir().join(format!("ninja-rs-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.obj"), "").unwrap();
        std::fs::write(dir.join("over.yaml"), "").unwrap();
        let args: Vec<String> = ["-out:app.exe", "a.obj", "kernel32.lib", "/LIBPATH:lib/x64", "-pdb:app.pdb", "/implib:app.lib", "/vfsoverlay:over.yaml", "-defaultlib:libcmt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let info = parse_linker_args(&args, Flavor::Msvc, &dir);
        assert_eq!(info.files, vec![dir.join("a.obj"), dir.join("over.yaml")]);
        assert_eq!(info.lib_dirs, vec![dir.join("lib").join("x64")]);
        assert_eq!(info.outputs, vec!["app.pdb", "app.lib"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gnu_arguments() {
        let dir = std::env::temp_dir().join(format!("ninja-rs-link-gnu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
