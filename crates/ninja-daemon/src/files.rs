//! File digests and directory trees, kept until the watcher reports a
//! change (or, for what it doesn't watch, checked once per build).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use cpp_deps::scan::Watch;
use rbe_client::DigestCache;
use rbe_client::proto as re;
use rbe_client::tree::{FileStamp, MemDigestCache};

use crate::proto::{FileInfo, TreeInfo};
use crate::watch::{Change, Listener, Watcher, key};

/// The session of entries the watcher vouches for.
const WATCHED: u64 = u64::MAX;

struct FileEntry {
    info: FileInfo,
    session: AtomicU64,
}

struct TreeEntry {
    info: Arc<TreeInfo>,
    session: AtomicU64,
}

pub struct Files {
    watcher: Arc<Watcher>,
    case_insensitive: bool,
    session: AtomicU64,
    /// Build directories (keys): always checked.
    volatile: RwLock<Vec<PathBuf>>,
    files: RwLock<HashMap<PathBuf, Arc<FileEntry>>>,
    trees: RwLock<HashMap<PathBuf, Arc<TreeEntry>>>,
    /// Digests by real path, size and modification time: links to a file
    /// (LLVM's tools) are hashed once, and digests saved by builds reused.
    hashes: MemDigestCache,
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

impl Files {
    pub fn new(watcher: Arc<Watcher>, case_insensitive: bool) -> Files {
        Files {
            watcher,
            case_insensitive,
            session: AtomicU64::new(1),
            volatile: RwLock::default(),
            files: RwLock::default(),
            trees: RwLock::default(),
            hashes: MemDigestCache::default(),
        }
    }

    pub fn new_session(&self) {
        self.session.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_volatile(&self, dir: &Path) {
        let k = key(dir, self.case_insensitive);
        let mut v = self.volatile.write().unwrap();
        if !v.contains(&k) {
            v.push(k);
        }
    }

    /// The number of file digests and trees kept.
    pub fn len(&self) -> (usize, usize) {
        (self.files.read().unwrap().len(), self.trees.read().unwrap().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == (0, 0)
    }

    /// Digests a build saved: used for files whose `FileStamp` still
    /// matches.
    pub fn seed(&self, entries: impl IntoIterator<Item = (PathBuf, FileStamp, String)>) {
        for (p, stamp, hash) in entries {
            self.hashes.put(&p, stamp, re::Digest { hash, size_bytes: stamp.len as i64 });
        }
    }

    fn current(&self, session: u64) -> bool {
        session == WATCHED || session == self.session.load(Ordering::Relaxed)
    }

    /// The session to record for a check that started with `token`.
    fn checked(&self, token: Option<u64>) -> u64 {
        match token {
            Some(t) if self.watcher.unchanged_since(t) => WATCHED,
            _ => self.session.load(Ordering::Relaxed),
        }
    }

    fn is_volatile(&self, k: &Path) -> bool {
        self.volatile.read().unwrap().iter().any(|v| k.starts_with(v))
    }

    fn token(&self, k: &Path, p: &Path) -> Option<u64> {
        if self.is_volatile(k) { None } else { self.watcher.token(p) }
    }

    /// A file's digest (symlinks followed).
    pub fn get(&self, p: &Path) -> Result<FileInfo, String> {
        let k = key(p, self.case_insensitive);
        let known = self.files.read().unwrap().get(&k).cloned();
        // A file the build may write is checked every time: a generator
        // can rewrite it between two actions of a session.
        if let Some(e) = &known
            && self.current(e.session.load(Ordering::Relaxed))
            && !self.is_volatile(&k)
        {
            return Ok(e.info.clone());
        }
        let token = self.token(&k, p);
        let meta = std::fs::metadata(p).map_err(|e| format!("stat {}: {e}", p.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", p.display()));
        }
        let stamp = FileStamp::of(&meta);
        if let Some(e) = known
            && e.info.stamp() == stamp
        {
            e.session.store(self.checked(token), Ordering::Relaxed);
            return Ok(e.info.clone());
        }
        let d = rbe_client::tree::file_digest(p, Some(&self.hashes)).map_err(|e| format!("{e:#}"))?;
        let info = FileInfo {
            hash: d.hash,
            size: d.size_bytes as u64,
            executable: is_executable(&meta),
            mtime_ns: stamp.mtime_ns,
            change: stamp.change,
            inode: stamp.inode,
        };
        let entry = FileEntry { info: info.clone(), session: AtomicU64::new(self.checked(token)) };
        self.files.write().unwrap().insert(k, Arc::new(entry));
        Ok(info)
    }

    /// The files under `dir` (symlinks followed) with their digests, as
    /// `InputTree::add_dir` sees them.
    pub fn tree(&self, dir: &Path) -> Result<Arc<TreeInfo>, String> {
        let k = key(dir, self.case_insensitive);
        if let Some(e) = self.trees.read().unwrap().get(&k)
            && self.current(e.session.load(Ordering::Relaxed))
        {
            return Ok(e.info.clone());
        }
        // Watched whole from now on.
        self.watcher.watch_tree(dir);
        let token = if self.is_volatile(&k) { None } else { self.watcher.tree_token(dir) };
        let mut info = TreeInfo::default();
        let mut outside = false;
        let real = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        self.walk(dir, "", &real, &mut info, &mut outside)?;
        let info = Arc::new(info);
        // Changes outside aren't seen.
        let session = if outside { self.session.load(Ordering::Relaxed) } else { self.checked(token) };
        self.trees.write().unwrap().insert(k, Arc::new(TreeEntry { info: info.clone(), session: AtomicU64::new(session) }));
        Ok(info)
    }

    /// `outside`: a symbolic link leads out of `real` (the tree's real path).
    fn walk(&self, dir: &Path, rel: &str, real: &Path, out: &mut TreeInfo, outside: &mut bool) -> Result<(), String> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("reading dir {}: {e}", dir.display()))?
            .collect::<std::io::Result<_>>()
            .map_err(|e| format!("reading dir {}: {e}", dir.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let path = e.path();
            if e.file_type().is_ok_and(|t| t.is_symlink()) {
                *outside |= !std::fs::canonicalize(&path).is_ok_and(|t| t.starts_with(real));
            }
            let meta = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
            if meta.is_dir() {
                out.dirs.push(child.clone());
                self.walk(&path, &child, real, out, outside)?;
            } else {
                out.files.push((child, self.get(&path)?));
            }
        }
        Ok(())
    }
}

impl Listener for Files {
    fn changed(&self, path: &Path, change: Change) {
        let k = key(path, self.case_insensitive);
        match change {
            Change::Content => {
                self.files.write().unwrap().remove(&k);
            }
            Change::Names => {}
            Change::Tree => {
                self.files.write().unwrap().retain(|f, _| !f.starts_with(&k));
            }
        }
        // A tree changes with anything under it.
        self.trees.write().unwrap().retain(|t, _| !k.starts_with(t) && !t.starts_with(&k));
    }
}

/// The first line of `.ninja_rs_digests` files.
pub const DIGESTS_SIGNATURE: &str = "# ninja-rs digests v2";

/// The entries of a `.ninja_rs_digests` file
/// (`path\tsize\tmtime_ns\tchange\tinode\thash` lines): (path, stamp, hash).
pub fn read_digests(file: &Path) -> Vec<(PathBuf, FileStamp, String)> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    let mut lines = text.lines();
    if lines.next() != Some(DIGESTS_SIGNATURE) {
        return Vec::new();
    }
    lines
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 6 {
                return None;
            }
            let (Ok(len), Ok(mtime_ns), Ok(change), Ok(inode)) = (f[1].parse(), f[2].parse(), f[3].parse(), f[4].parse()) else { return None };
            Some((PathBuf::from(f[0]), FileStamp { len, mtime_ns, change, inode }, f[5].to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_and_trees() {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_files_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tc/bin")).unwrap();
        std::fs::create_dir_all(dir.join("tc/empty")).unwrap();
        std::fs::write(dir.join("tc/bin/cc"), "cc").unwrap();
        std::fs::write(dir.join("tc/a.h"), "a").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Arc::new(Files::new(w.clone(), cfg!(windows)));
        w.listen(files.clone());
        let a = files.get(&dir.join("tc/a.h")).unwrap();
        assert_eq!((a.size, a.hash.as_str()), (1, "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"));
        assert!(files.get(&dir.join("tc")).is_err());
        let t = files.tree(&dir.join("tc")).unwrap();
        assert_eq!(t.dirs, vec!["bin", "empty"]);
        assert_eq!(t.files.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), vec!["a.h", "bin/cc"]);
        // Within a session, kept; a change reported drops the tree.
        assert!(Arc::ptr_eq(&files.tree(&dir.join("tc")).unwrap(), &t));
        files.changed(&dir.join("tc/bin/cc"), Change::Content);
        assert!(!Arc::ptr_eq(&files.tree(&dir.join("tc")).unwrap(), &t));
        // Saved digests are reused for the same version of a file.
        let b = dir.join("b.txt");
        std::fs::write(&b, "bbb").unwrap();
        let m = std::fs::metadata(&b).unwrap();
        files.seed([(b.clone(), FileStamp::of(&m), "made-up".to_string())]);
        assert_eq!(files.get(&b).unwrap().hash, "made-up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_files() {
        let f = std::env::temp_dir().join(format!("ninja_daemon_digests_{}", std::process::id()));
        std::fs::write(&f, format!("{DIGESTS_SIGNATURE}\n/a/b\t3\t1000\t7\t9\tabc\nbad line\n")).unwrap();
        let d = read_digests(&f);
        assert_eq!(d, vec![(PathBuf::from("/a/b"), FileStamp { len: 3, mtime_ns: 1000, change: 7, inode: 9 }, "abc".to_string())]);
        let _ = std::fs::remove_file(&f);
    }
}
