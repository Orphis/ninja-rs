//! Clang header maps (`.hmap` files given as `-I` or `-iquote`
//! directories, as Xcode builds use): include names mapped to paths,
//! looked up case-insensitively.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::scan::normalize;

#[derive(Debug, Default)]
pub struct HeaderMap {
    pub path: PathBuf,
    /// Lowercased include name → path.
    entries: HashMap<String, PathBuf>,
}

const MAGIC: u32 = u32::from_le_bytes(*b"pamh");

impl HeaderMap {
    /// Whether a file is a header map (by its magic).
    pub fn is_header_map(path: &Path) -> bool {
        use std::io::Read;
        let mut magic = [0u8; 4];
        std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && (magic == *b"pamh" || magic == *b"hmap")
    }

    /// Reads a header map; relative results are against `cwd`.
    pub fn load(path: &Path, cwd: &Path) -> Result<HeaderMap, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        HeaderMap::parse(&data, path, cwd).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(data: &[u8], path: &Path, cwd: &Path) -> Result<HeaderMap, String> {
        if data.len() < 24 {
            return Err("truncated header map".into());
        }
        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let big = match magic {
            MAGIC => false,
            m if m.swap_bytes() == MAGIC => true,
            _ => return Err("not a header map".into()),
        };
        let u32_at = |o: usize| -> Option<u32> {
            let b: [u8; 4] = data.get(o..o + 4)?.try_into().ok()?;
            Some(if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
        };
        let strings = u32_at(8).ok_or("truncated")? as usize;
        let buckets = u32_at(16).ok_or("truncated")? as usize;
        let string = |o: u32| -> Option<String> {
            let start = strings.checked_add(o as usize)?;
            let rest = data.get(start..)?;
            let end = rest.iter().position(|&c| c == 0)?;
            Some(String::from_utf8_lossy(&rest[..end]).into_owned())
        };
        let mut entries = HashMap::new();
        for i in 0..buckets {
            let at = 24 + i * 12;
            let key = u32_at(at).ok_or("truncated bucket")?;
            if key == 0 {
                continue;
            }
            let (Some(k), Some(prefix), Some(suffix)) = (string(key), u32_at(at + 4).and_then(string), u32_at(at + 8).and_then(string)) else {
                return Err("bad string offset".into());
            };
            entries.insert(k.to_lowercase(), normalize(&cwd.join(format!("{prefix}{suffix}"))));
        }
        Ok(HeaderMap { path: path.to_path_buf(), entries })
    }

    pub fn lookup(&self, name: &str) -> Option<&PathBuf> {
        self.entries.get(&name.to_lowercase())
    }
}

#[cfg(test)]
pub(crate) fn build(entries: &[(&str, &str, &str)]) -> Vec<u8> {
    // Strings: a leading NUL so that offset 0 means "empty".
    let mut strings = vec![0u8];
    let mut add = |s: &str| {
        let o = strings.len() as u32;
        strings.extend_from_slice(s.as_bytes());
        strings.push(0);
        o
    };
    let n = entries.len().next_power_of_two().max(1) * 2;
    let mut buckets = vec![(0u32, 0u32, 0u32); n];
    for (i, (k, p, s)) in entries.iter().enumerate() {
        buckets[i] = (add(k), add(p), add(s));
    }
    let strings_offset = 24 + n * 12;
    let mut out = Vec::new();
    out.extend_from_slice(b"pamh");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(strings_offset as u32).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (k, p, s) in buckets {
        for v in [k, p, s] {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out.extend_from_slice(&strings);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_header_maps() {
        let data = build(&[("Foo/Bar.h", "/src/foo/", "bar.h"), ("rel.h", "gen/", "rel.h")]);
        let m = HeaderMap::parse(&data, Path::new("/b/x.hmap"), Path::new("/b")).unwrap();
        assert_eq!(m.lookup("foo/bar.h"), Some(&PathBuf::from("/src/foo/bar.h")));
        assert_eq!(m.lookup("rel.h"), Some(&PathBuf::from("/b/gen/rel.h")));
        assert_eq!(m.lookup("other.h"), None);
        assert!(HeaderMap::parse(b"nope", Path::new("x"), Path::new("/")).is_err());
    }
}
