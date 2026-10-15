//! Maps keyed by path whose lookups are hashed, and whose entries under a
//! directory go together: what's under it is a range of an ordered index,
//! so dropping it costs what it held, not what the map holds (a branch
//! switch removing thousands of directories). Paths compare as
//! `Path::starts_with` does, by components.

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;

/// A map by path (see the module's doc).
pub struct PathMap<V> {
    map: HashMap<Arc<Path>, V>,
    /// The keys, by their components (see `ordered`).
    order: BTreeMap<Box<[u8]>, Arc<Path>>,
}

/// A set of paths whose members under a directory go together.
pub type PathSet = PathMap<()>;

impl<V> Default for PathMap<V> {
    fn default() -> Self {
        PathMap { map: HashMap::new(), order: BTreeMap::new() }
    }
}

/// `p`'s components, each a tag and its bytes ended by a NUL (which no
/// component has): a path's descendants are those whose bytes start with
/// its own, and sort right after them. Prefixes as `Path` compares them
/// (`C:` is `c:`, `\\?\C:` isn't `C:`).
fn ordered(p: &Path) -> Box<[u8]> {
    let mut out = Vec::with_capacity(p.as_os_str().len() + 16);
    for c in p.components() {
        match c {
            Component::Prefix(prefix) => {
                let mut parts = |tag: u8, parts: &[&[u8]]| {
                    out.push(tag);
                    for (i, part) in parts.iter().enumerate() {
                        if i > 0 {
                            out.push(1);
                        }
                        out.extend_from_slice(part);
                    }
                };
                match prefix.kind() {
                    Prefix::Verbatim(s) => parts(b'v', &[s.as_encoded_bytes()]),
                    Prefix::VerbatimUNC(server, share) => parts(b'w', &[server.as_encoded_bytes(), share.as_encoded_bytes()]),
                    Prefix::VerbatimDisk(d) => parts(b'x', &[&[d]]),
                    Prefix::DeviceNS(s) => parts(b'n', &[s.as_encoded_bytes()]),
                    Prefix::UNC(server, share) => parts(b'u', &[server.as_encoded_bytes(), share.as_encoded_bytes()]),
                    Prefix::Disk(d) => parts(b'd', &[&[d]]),
                }
            }
            Component::RootDir => out.push(b'/'),
            Component::CurDir => out.push(b'.'),
            Component::ParentDir => out.push(b'^'),
            Component::Normal(name) => {
                out.push(b'n');
                out.extend_from_slice(name.as_encoded_bytes());
            }
        }
        out.push(0);
    }
    out.into()
}

impl<V> PathMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, key: &Path) -> Option<&V> {
        self.map.get(key)
    }

    pub fn get_mut(&mut self, key: &Path) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    pub fn contains_key(&self, key: &Path) -> bool {
        self.map.contains_key(key)
    }

    /// Sets `key`'s value: the one before, if any.
    pub fn insert(&mut self, key: impl AsRef<Path> + Into<PathBuf>, value: V) -> Option<V> {
        if let Some(v) = self.map.get_mut(key.as_ref()) {
            return Some(std::mem::replace(v, value));
        }
        let key: Arc<Path> = Arc::from(key.into());
        self.order.insert(ordered(&key), key.clone());
        self.map.insert(key, value);
        None
    }

    pub fn remove(&mut self, key: &Path) -> Option<V> {
        let (key, v) = self.map.remove_entry(key)?;
        self.order.remove(&ordered(&key));
        Some(v)
    }

    /// The keys under the one ordered as `start` (it included).
    fn under<'a>(&'a self, start: &'a [u8]) -> impl Iterator<Item = (&'a Box<[u8]>, &'a Arc<Path>)> {
        self.order.range::<[u8], _>((Bound::Included(start), Bound::Unbounded)).take_while(move |(k, _)| k.starts_with(start))
    }

    /// Whether it has `key` or entries under it.
    pub fn has_under(&self, key: &Path) -> bool {
        self.under(&ordered(key)).next().is_some()
    }

    /// Removes `key` and the entries under it, looking at those only:
    /// how many there were.
    pub fn remove_under(&mut self, key: &Path) -> usize {
        self.take_under(key, |_, _| {})
    }

    /// Removes `key` and the entries under it, giving each to `taken`: how
    /// many there were.
    pub fn take_under(&mut self, key: &Path, mut taken: impl FnMut(&Path, V)) -> usize {
        let start = ordered(key);
        let under: Vec<(Box<[u8]>, Arc<Path>)> = self.under(&start).map(|(k, p)| (k.clone(), p.clone())).collect();
        for (k, p) in &under {
            self.order.remove(k);
            if let Some(v) = self.map.remove(p) {
                taken(p, v);
            }
        }
        under.len()
    }

    /// Keeps the entries `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&Path, &mut V) -> bool) {
        let order = &mut self.order;
        self.map.retain(|k, v| {
            let kept = keep(k, v);
            if !kept {
                order.remove(&ordered(k));
            }
            kept
        });
    }

    pub fn keys(&self) -> impl Iterator<Item = &Path> {
        self.map.keys().map(|k| &**k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Path, &V)> {
        self.map.iter().map(|(k, v)| (&**k, v))
    }
}

impl PathSet {
    /// Adds `key`: whether it's new.
    pub fn add(&mut self, key: impl AsRef<Path> + Into<PathBuf>) -> bool {
        self.insert(key, ()).is_none()
    }

    pub fn contains(&self, key: &Path) -> bool {
        self.contains_key(key)
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for PathMap<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.order.values().map(|k| (k, &self.map[k]))).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What's under a directory goes, nothing beside it (`a/b-x`, `a/b.h`
    /// and `a/b0` sort among `a/b/...` as strings, not as paths).
    #[test]
    fn subtrees() {
        let paths = ["a", "a/b", "a/b/c", "a/b/c/d", "a/b-x", "a/b.h", "a/b0", "a/bb/c", "b"];
        let mut m: PathMap<()> = PathMap::new();
        for p in paths {
            m.insert(p, ());
        }
        assert!(m.has_under(Path::new("a/b")) && m.has_under(Path::new("a/bb")) && !m.has_under(Path::new("a/c")));
        assert_eq!(m.remove_under(Path::new("a/b")), 3);
        let mut left: Vec<&str> = m.keys().map(|k| k.to_str().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["a", "a/b-x", "a/b.h", "a/b0", "a/bb/c", "b"]);
        assert_eq!(m.remove_under(Path::new("a/c")), 0);
        assert_eq!(m.order.len(), m.map.len());
    }

    /// Entries removed under a path are those `starts_with` finds, however
    /// spelled: separators doubled or trailing, `.`, `..`, other volumes,
    /// prefixes (`\\?\`, UNC), siblings whose names extend the path's.
    #[test]
    fn as_starts_with() {
        let keys: Vec<&str> = if cfg!(windows) {
            vec![
                r"c:\x\a",
                r"c:\x\a\f.h",
                r"c:\x\a\b\g.h",
                r"c:\x\a-b\h.h",
                r"c:\x\a.c",
                r"c:\x\a b\i.h",
                r"C:\x\a\J.H",
                "c:/x/a/k.h",
                r"c:\x\ab",
                r"\\?\c:\x\a\v.h",
                r"d:\x\a\d.h",
                r"c:\x\a\\double.h",
                r"c:\x\a\.\dot.h",
                r"c:\x\a\..\up.h",
                r"\\srv\share\x\a\u.h",
                "//srv/share/x/a/w.h",
                r"\\?\UNC\srv\share\x",
                r"c:x\a\rel.h",
                r"x\a\rel.h",
            ]
        } else {
            vec![
                "/x/a",
                "/x/a/f.h",
                "/x/a/b/g.h",
                "/x/a-b/h.h",
                "/x/a.c",
                "/x/a b/i.h",
                "/x/A/J.H",
                "/x/ab",
                "/x/a//double.h",
                "/x/a/./dot.h",
                "/x/a/../up.h",
                "x/a/rel.h",
                "/x/a\u{1}b",
            ]
        };
        let targets: Vec<&str> = if cfg!(windows) {
            vec![r"C:\x\a", r"c:\x\a\", r"\\?\c:\x", r"\\srv\share\x", "c:x", r"c:\", r"d:\", ""]
        } else {
            vec!["/x/a", "/x/a/", "/x", "x", "/", ""]
        };
        for target in targets {
            let mut m: PathMap<&str> = PathMap::new();
            for k in &keys {
                m.insert(*k, *k);
            }
            let mut linear: Vec<&str> = keys.iter().filter(|k| Path::new(k).starts_with(target)).copied().collect();
            // Keys equal as paths are one entry.
            linear.dedup_by(|a, b| Path::new(a) == Path::new(b));
            assert_eq!(m.has_under(Path::new(target)), !linear.is_empty(), "{target}");
            let n = m.remove_under(Path::new(target));
            assert_eq!(n, linear.len(), "{target}: {linear:?}");
            assert!(m.keys().all(|k| !k.starts_with(target)), "{target}");
            assert_eq!(m.len(), m.order.len());
        }
    }
}
