//! `CMakeFiles/rbe_toolchains.json` (see docs/cmake-interface.md).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Toolchain {
    #[serde(default)]
    pub language: String,
    pub compiler: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub platform: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ToolchainsFile {
    pub version: u32,
    #[serde(default)]
    pub toolchains: Vec<Toolchain>,
}

#[derive(Debug, Clone, Default)]
pub struct Toolchains {
    pub toolchains: Vec<Toolchain>,
    windows: bool,
}

/// Normalizes a path for comparison: forward slashes, lexically canonical,
/// lowercase on Windows.
pub fn compare_key(p: &str, windows: bool) -> String {
    let s = if windows { p.replace('\\', "/") } else { p.to_string() };
    let (c, _) = ninja_core::canon::canonicalize_path_with(&s, false);
    if windows { c.to_ascii_lowercase() } else { c }
}

impl Toolchains {
    pub fn parse(json: &str, windows: bool) -> Result<Self, String> {
        let f: ToolchainsFile = serde_json::from_str(json).map_err(|e| format!("invalid toolchains file: {e}"))?;
        if f.version != 1 {
            return Err(format!("unsupported toolchains file version {}", f.version));
        }
        Ok(Self { toolchains: f.toolchains, windows })
    }

    pub fn load(path: &Path, windows: bool) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Self::parse(&text, windows)
    }

    /// Finds the toolchain whose `compiler` is `exe` (an absolute path).
    pub fn find(&self, exe: &Path) -> Option<&Toolchain> {
        let key = compare_key(&exe.to_string_lossy(), self.windows);
        self.toolchains.iter().find(|t| {
            let k = compare_key(&t.compiler, self.windows);
            k == key || (self.windows && (format!("{k}.exe") == key || k == format!("{key}.exe")))
        })
    }
}

/// On Windows, expands 8.3 short names (`C:\PROGRA~1\LLVM\bin\CLANG_~1.EXE`)
/// to the long path; elsewhere returns the path unchanged (resolving
/// symlinks there would break multi-call binaries like LLVM's).
pub fn long_path(p: &Path) -> PathBuf {
    if cfg!(windows)
        && let Ok(c) = std::fs::canonicalize(p)
    {
        let s = c.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
        return c;
    }
    p.to_path_buf()
}

/// Resolves the first token of a command to an absolute path: relative
/// paths against `cwd`, bare names through `PATH` (with `.exe` on Windows).
pub fn resolve_program(program: &str, cwd: &Path, windows: bool) -> Option<PathBuf> {
    let has_sep = program.contains('/') || (windows && program.contains('\\'));
    let p = Path::new(program);
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        let mut v = vec![base.clone()];
        if windows && base.extension().is_none() {
            v.push(base.with_extension("exe"));
        }
        v
    };
    if p.is_absolute() || has_sep {
        let base = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
        return candidates(base).into_iter().find(|c| c.is_file());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for c in candidates(dir.join(program)) {
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{
      "version": 1,
      "toolchains": [
        {
          "language": "CXX",
          "compiler": "C:/llvm/bin/clang-cl.exe",
          "inputs": ["C:/llvm/bin", "C:/llvm/lib/clang"],
          "platform": { "OSFamily": "Windows", "container-image": "docker://x" }
        },
        { "language": "C", "compiler": "/usr/bin/cc" }
      ]
    }"#;

    #[test]
    fn parse_and_find_windows() {
        let t = Toolchains::parse(JSON, true).unwrap();
        let tc = t.find(Path::new(r"c:\LLVM\bin\clang-cl.exe")).unwrap();
        assert_eq!(tc.language, "CXX");
        assert_eq!(tc.inputs.len(), 2);
        assert_eq!(tc.platform["OSFamily"], "Windows");
        assert!(t.find(Path::new(r"c:\llvm\bin\clang-cl")).is_some());
        assert!(t.find(Path::new(r"c:\other\clang-cl.exe")).is_none());
    }

    #[test]
    fn find_posix_is_case_sensitive() {
        let t = Toolchains::parse(JSON, false).unwrap();
        assert!(t.find(Path::new("/usr/bin/cc")).is_some());
        assert!(t.find(Path::new("/usr/bin/../bin/cc")).is_some());
        assert!(t.find(Path::new("/USR/bin/cc")).is_none());
        assert!(t.find(Path::new("/usr/bin/cc")).unwrap().inputs.is_empty());
    }

    #[test]
    fn bad_version() {
        assert!(Toolchains::parse(r#"{"version": 2, "toolchains": []}"#, false).is_err());
    }
}
