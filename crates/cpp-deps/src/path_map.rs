//! Maps keyed by path whose lookups are hashed, and whose entries under a
//! directory go together: what's under it is a range of an ordered index,
//! so dropping it costs what it held, not what the map holds (a branch
//! switch removing thousands of directories). Paths compare as
//! `Path::starts_with` does, by components.
//!
//! A key is kept once, encoded by its components (`PathKey`): the hash map
//! and the ordered index share it, and it's hashed as it's compared (std's
//! `Path` hashes some spellings equal paths have apart, `\\?\C:\x` and
//! `\\?\C:/x`). A path looked up several times (an event, in every map it
//! concerns, and its ancestors) is encoded once.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::ops::Bound;
use std::path::{Component, MAIN_SEPARATOR_STR, Path, PathBuf, Prefix};
use std::sync::Arc;

/// A map by path (see the module's doc).
pub struct PathMap<V> {
    map: HashMap<Arc<[u8]>, V>,
    /// The same keys, ordered: what's under a key follows it.
    order: BTreeSet<Arc<[u8]>>,
}

/// A set of paths whose members under a directory go together.
pub type PathSet = PathMap<()>;

impl<V> Default for PathMap<V> {
    fn default() -> Self {
        PathMap { map: HashMap::new(), order: BTreeSet::new() }
    }
}

/// Tags of the components (see `encode`).
mod tag {
    pub const VERBATIM: u8 = b'v';
    pub const VERBATIM_UNC: u8 = b'w';
    pub const VERBATIM_DISK: u8 = b'x';
    pub const DEVICE_NS: u8 = b'e';
    pub const UNC: u8 = b'u';
    pub const DISK: u8 = b'd';
    pub const ROOT: u8 = b'/';
    pub const CUR: u8 = b'.';
    pub const PARENT: u8 = b'^';
    pub const NORMAL: u8 = b'n';
    /// Between a prefix's parts (a server and its share).
    pub const PART: u8 = 1;
}

/// Appends `p`'s components to `out`, each a tag and its bytes ended by a
/// NUL (which no component has): a path's descendants are those whose
/// bytes start with its own, and sort right after them. Prefixes as `Path`
/// compares them (`C:` is `c:`, `\\?\C:` isn't `C:`).
fn encode(p: &Path, out: &mut Vec<u8>) {
    for c in p.components() {
        match c {
            Component::Prefix(prefix) => {
                let mut parts = |t: u8, parts: &[&[u8]]| {
                    out.push(t);
                    for (i, part) in parts.iter().enumerate() {
                        if i > 0 {
                            out.push(tag::PART);
                        }
                        out.extend_from_slice(part);
                    }
                };
                match prefix.kind() {
                    Prefix::Verbatim(s) => parts(tag::VERBATIM, &[s.as_encoded_bytes()]),
                    Prefix::VerbatimUNC(server, share) => parts(tag::VERBATIM_UNC, &[server.as_encoded_bytes(), share.as_encoded_bytes()]),
                    Prefix::VerbatimDisk(d) => parts(tag::VERBATIM_DISK, &[&[d]]),
                    Prefix::DeviceNS(s) => parts(tag::DEVICE_NS, &[s.as_encoded_bytes()]),
                    Prefix::UNC(server, share) => parts(tag::UNC, &[server.as_encoded_bytes(), share.as_encoded_bytes()]),
                    Prefix::Disk(d) => parts(tag::DISK, &[&[d]]),
                }
            }
            Component::RootDir => out.push(tag::ROOT),
            Component::CurDir => out.push(tag::CUR),
            Component::ParentDir => out.push(tag::PARENT),
            Component::Normal(name) => {
                out.push(tag::NORMAL);
                out.extend_from_slice(name.as_encoded_bytes());
            }
        }
        out.push(0);
    }
}

/// The path a key was made from (as `Path` compares it: separators may
/// differ), for showing it.
fn decode(key: &[u8]) -> PathBuf {
    let mut out: Vec<u8> = Vec::with_capacity(key.len());
    let mut separate = false;
    for c in key.split(|&b| b == 0).filter(|c| !c.is_empty()) {
        let (t, rest) = (c[0], &c[1..]);
        let mut parts = rest.splitn(2, |&b| b == tag::PART);
        let (first, second) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let name: &[u8] = match t {
            tag::ROOT => {
                out.extend_from_slice(MAIN_SEPARATOR_STR.as_bytes());
                separate = false;
                continue;
            }
            tag::CUR => b".",
            tag::PARENT => b"..",
            tag::NORMAL => rest,
            _ => {
                let pieces: [&[u8]; 4] = match t {
                    tag::VERBATIM => [br"\\?\", first, b"", b""],
                    tag::VERBATIM_UNC => [br"\\?\UNC\", first, br"\", second],
                    tag::VERBATIM_DISK => [br"\\?\", first, b":", b""],
                    tag::DEVICE_NS => [br"\\.\", first, b"", b""],
                    tag::UNC => [br"\\", first, br"\", second],
                    _ => [first, b":", b"", b""],
                };
                pieces.iter().for_each(|p| out.extend_from_slice(p));
                separate = false;
                continue;
            }
        };
        if separate {
            out.extend_from_slice(MAIN_SEPARATOR_STR.as_bytes());
        }
        out.extend_from_slice(name);
        separate = true;
    }
    // SAFETY: whole components of `OsStr`s (`as_encoded_bytes`) and ASCII,
    // concatenated.
    PathBuf::from(unsafe { OsString::from_encoded_bytes_unchecked(out) })
}

/// A path encoded as `PathMap`s key it (see `encode`): made once for
/// several lookups, of it and of its ancestors.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PathKey(Box<[u8]>);

/// A `PathKey`, or one of its ancestors.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyRef<'a>(&'a [u8]);

impl PathKey {
    pub fn new(p: &Path) -> PathKey {
        let mut out = Vec::with_capacity(p.as_os_str().len() + 16);
        encode(p, &mut out);
        PathKey(out.into())
    }

    pub fn as_ref(&self) -> KeyRef<'_> {
        KeyRef(&self.0)
    }

    /// It and the paths above it, as `Path::ancestors` gives them.
    pub fn ancestors(&self) -> impl Iterator<Item = KeyRef<'_>> {
        self.as_ref().ancestors()
    }

    pub fn parent(&self) -> Option<KeyRef<'_>> {
        self.ancestors().nth(1)
    }

    pub fn to_path(&self) -> PathBuf {
        decode(&self.0)
    }
}

impl std::fmt::Debug for PathKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.to_path().fmt(f)
    }
}

impl<'a> KeyRef<'a> {
    /// It and the paths above it, as `Path::ancestors` gives them: a prefix
    /// isn't one without its root (`C:` of `C:\x`); a relative path's
    /// last one is the empty path.
    pub fn ancestors(self) -> impl Iterator<Item = KeyRef<'a>> {
        let bytes = self.0;
        let tag_at = |i: usize| bytes.get(i).copied();
        let relative = !matches!(tag_at(0), Some(t) if is_prefix(t) || t == tag::ROOT);
        // Where each component ends, from the last.
        let ends = bytes.iter().enumerate().filter(|(_, b)| **b == 0).map(|(i, _)| i + 1);
        let mut ends: Vec<usize> = ends.filter(|&end| !(is_prefix(bytes[0]) && end < bytes.len() && tag_at(end) == Some(tag::ROOT))).collect();
        if relative {
            ends.insert(0, 0);
        }
        ends.into_iter().rev().map(move |end| KeyRef(&bytes[..end]))
    }

    pub fn parent(self) -> Option<KeyRef<'a>> {
        self.ancestors().nth(1)
    }

    /// Whether it's `other` or under it, as `Path::starts_with` says.
    pub fn starts_with(self, other: KeyRef) -> bool {
        self.0.starts_with(other.0)
    }

    pub fn to_owned(self) -> PathKey {
        PathKey(self.0.into())
    }

    pub fn to_path(self) -> PathBuf {
        decode(self.0)
    }
}

impl std::fmt::Debug for KeyRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.to_path().fmt(f)
    }
}

fn is_prefix(t: u8) -> bool {
    matches!(t, tag::VERBATIM | tag::VERBATIM_UNC | tag::VERBATIM_DISK | tag::DEVICE_NS | tag::UNC | tag::DISK)
}

thread_local! {
    /// Where paths looked up are encoded (see `Lookup for Path`).
    static ENCODED: Cell<Vec<u8>> = const { Cell::new(Vec::new()) };
}

/// What a `PathMap` is looked up by: a path, or a key encoded already.
pub trait Lookup {
    /// Calls `f` with the key's bytes.
    fn with_key<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R;
}

impl Lookup for Path {
    /// Encoded in a buffer kept for the next one (a lookup in it calling
    /// another gets one of its own).
    fn with_key<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        let mut buf = ENCODED.take();
        buf.clear();
        encode(self, &mut buf);
        let r = f(&buf);
        ENCODED.set(buf);
        r
    }
}

impl Lookup for PathBuf {
    fn with_key<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        self.as_path().with_key(f)
    }
}

impl Lookup for PathKey {
    fn with_key<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(&self.0)
    }
}

impl Lookup for KeyRef<'_> {
    fn with_key<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        f(self.0)
    }
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

    pub fn get<K: Lookup + ?Sized>(&self, key: &K) -> Option<&V> {
        key.with_key(|k| self.map.get(k))
    }

    pub fn get_mut<K: Lookup + ?Sized>(&mut self, key: &K) -> Option<&mut V> {
        key.with_key(|k| self.map.get_mut(k))
    }

    pub fn contains_key<K: Lookup + ?Sized>(&self, key: &K) -> bool {
        key.with_key(|k| self.map.contains_key(k))
    }

    /// Sets `key`'s value: the one before, if any.
    pub fn insert<K: Lookup + ?Sized>(&mut self, key: &K, value: V) -> Option<V> {
        key.with_key(|k| {
            if let Some(v) = self.map.get_mut(k) {
                return Some(std::mem::replace(v, value));
            }
            let k: Arc<[u8]> = Arc::from(k);
            self.order.insert(k.clone());
            self.map.insert(k, value);
            None
        })
    }

    pub fn remove<K: Lookup + ?Sized>(&mut self, key: &K) -> Option<V> {
        key.with_key(|k| {
            let (k, v) = self.map.remove_entry(k)?;
            self.order.remove(&k);
            Some(v)
        })
    }

    /// The keys under the one encoded as `start` (it included).
    fn under<'a>(&'a self, start: &'a [u8]) -> impl Iterator<Item = &'a Arc<[u8]>> {
        self.order.range::<[u8], _>((Bound::Included(start), Bound::Unbounded)).take_while(move |k| k.starts_with(start))
    }

    /// Whether it has `key` or entries under it.
    pub fn has_under<K: Lookup + ?Sized>(&self, key: &K) -> bool {
        key.with_key(|k| self.under(k).next().is_some())
    }

    /// Removes `key` and the entries under it, looking at those only:
    /// how many there were.
    pub fn remove_under<K: Lookup + ?Sized>(&mut self, key: &K) -> usize {
        self.take_under(key, |_| {})
    }

    /// Removes `key` and the entries under it, giving each value to
    /// `taken`: how many there were.
    pub fn take_under<K: Lookup + ?Sized>(&mut self, key: &K, mut taken: impl FnMut(V)) -> usize {
        let under: Vec<Arc<[u8]>> = key.with_key(|k| self.under(k).cloned().collect());
        for k in &under {
            self.order.remove(k);
            if let Some(v) = self.map.remove(k) {
                taken(v);
            }
        }
        under.len()
    }

    /// Keeps the entries whose value `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&mut V) -> bool) {
        let order = &mut self.order;
        self.map.retain(|k, v| {
            let kept = keep(v);
            if !kept {
                order.remove(k);
            }
            kept
        });
    }

    /// The paths of the keys, as `Path` compares them (separators may
    /// differ from those inserted).
    pub fn keys(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.order.iter().map(|k| decode(k))
    }

    pub fn iter(&self) -> impl Iterator<Item = (PathBuf, &V)> {
        self.order.iter().map(|k| (decode(k), &self.map[k]))
    }
}

impl PathSet {
    /// Adds `key`: whether it's new.
    pub fn add<K: Lookup + ?Sized>(&mut self, key: &K) -> bool {
        self.insert(key, ()).is_none()
    }

    pub fn contains<K: Lookup + ?Sized>(&self, key: &K) -> bool {
        self.contains_key(key)
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for PathMap<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
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
            m.insert(Path::new(p), ());
        }
        assert!(m.has_under(Path::new("a/b")) && m.has_under(Path::new("a/bb")) && !m.has_under(Path::new("a/c")));
        assert_eq!(m.remove_under(Path::new("a/b")), 3);
        let mut left: Vec<String> = m.keys().map(|k| k.to_str().unwrap().replace('\\', "/")).collect();
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
                m.insert(Path::new(k), *k);
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

    /// Random operations agree with a list compared as `Path` compares,
    /// over spellings equal paths have (`\\?\C:\x` and `\\?\C:/x`, whose
    /// `Path` hashes differ: the map kept both, its index one) and those of
    /// different ones (`\\.\COM1` and `COM1`, whose index keys were one).
    #[test]
    fn as_paths_compare() {
        let spellings: Vec<String> = if cfg!(windows) {
            let mut v = Vec::new();
            for pre in [r"C:", r"c:", r"\\?\C:", r"\\?\c:", r"\\srv\share", r"\\?\UNC\srv\share", r"\\.\COM1", ""] {
                for mid in [r"\x", "/x", r"\x\", r"\.\x", r"\\x", r"\COM1", "COM1"] {
                    for leaf in ["", r"\a", r"\a\b.h", "/a/c.h", r"\a-b\d.h", r"\a.h", r"\a\..\f.h"] {
                        v.push(format!("{pre}{mid}{leaf}"));
                    }
                }
            }
            v
        } else {
            let mut v = Vec::new();
            for pre in ["", "/", "//", "./", "/./"] {
                for mid in ["x", "x/", "X", "x/.", "x//"] {
                    for leaf in ["", "/a", "/a/b.h", "/a-b/d.h", "/a.h", "/a/../f.h", "/a\u{1}/h.h", "/a\u{1}"] {
                        v.push(format!("{pre}{mid}{leaf}"));
                    }
                }
            }
            v
        };
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = |n: usize| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s % n as u64) as usize
        };
        for round in 0..50 {
            let mut m: PathMap<usize> = PathMap::new();
            let mut model: Vec<(PathBuf, usize)> = Vec::new();
            for op in 0..300 {
                let p = PathBuf::from(&spellings[rnd(spellings.len())]);
                let at = format!("round {round} op {op}: {}", p.display());
                match rnd(6) {
                    0..=2 => {
                        let before = model.iter_mut().find(|(k, _)| *k == p).map(|e| std::mem::replace(&mut e.1, op));
                        if before.is_none() {
                            model.push((p.clone(), op));
                        }
                        assert_eq!(m.insert(&p, op), before, "{at}");
                    }
                    3 => {
                        let i = model.iter().position(|(k, _)| *k == p);
                        assert_eq!(m.remove(&p), i.map(|i| model.swap_remove(i).1), "{at}");
                    }
                    4 => {
                        let before = model.len();
                        model.retain(|(k, _)| !k.starts_with(&p));
                        assert_eq!(m.remove_under(&p), before - model.len(), "{at}");
                    }
                    _ => assert_eq!(m.has_under(&p), model.iter().any(|(k, _)| k.starts_with(&p)), "{at}"),
                }
                assert_eq!((m.len(), m.order.len()), (model.len(), model.len()), "{at}");
            }
            m.retain(|v| *v % 2 == 0);
            model.retain(|(_, v)| *v % 2 == 0);
            assert_eq!(m.remove_under(Path::new("")), model.len());
            assert!(m.is_empty());
        }
    }

    /// A key's ancestors are its path's, and its path is the one encoded
    /// (as `Path` compares it).
    #[test]
    fn keys_and_ancestors() {
        let paths: &[&str] = if cfg!(windows) {
            &[r"C:\x\y", r"C:x\y", "a/b", r"\\?\C:\x", r"\\srv\sh\x", r"C:\", r"\\?\C:/x\a", r"\\.\COM1\x", r"..\a", r".\a"]
        } else {
            &["/x/y", "a/b", "/", "../a", "./a", "/a\u{1}b/c"]
        };
        for p in paths {
            let key = PathKey::new(Path::new(p));
            assert_eq!(key.to_path(), Path::new(p), "{p}");
            let ancestors: Vec<PathBuf> = key.ancestors().map(KeyRef::to_path).collect();
            let expected: Vec<&Path> = Path::new(p).ancestors().collect();
            assert_eq!(ancestors, expected, "{p}");
            for (a, e) in key.ancestors().zip(&expected) {
                assert_eq!(a.to_owned(), PathKey::new(e), "{p}: {}", e.display());
            }
        }
    }
}
