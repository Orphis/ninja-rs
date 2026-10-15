//! File digests and directory trees, kept until the watcher reports a
//! change (or, for what it doesn't watch, checked once per build).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use cpp_deps::scan::Watch;
use ninja_os::{Digest, FileStamp};

use crate::proto::{FileInfo, TreeInfo};
use crate::watch::{Change, Listener, Watcher, key, strip_verbatim, walk_skips};

/// The session of entries the watcher vouches for.
const WATCHED: u64 = u64::MAX;

struct FileEntry {
    info: FileInfo,
    stamp: FileStamp,
    /// Checked within its file system's time granularity of its last
    /// change: changed again since, it could have kept its stamp.
    racy: bool,
    session: AtomicU64,
}

struct TreeEntry {
    /// Or why it couldn't be listed.
    info: Result<Arc<TreeInfo>, String>,
    session: AtomicU64,
}

/// A digest kept for a file.
enum Kept {
    /// Hashed here, for that version of the file.
    Hashed(FileStamp, Digest),
    /// Saved by a build (`seed`), for that version of the file.
    Saved(FileStamp, Digest),
    /// The watcher saw the file change since (the `n`th change it
    /// reported; 0: none yet, the file being hashed for the first time):
    /// hashed again, whatever its stamp.
    Changed(u64),
}

pub struct Files {
    watcher: Arc<Watcher>,
    case_insensitive: bool,
    session: AtomicU64,
    files: RwLock<HashMap<PathBuf, Arc<FileEntry>>>,
    trees: RwLock<HashMap<PathBuf, Arc<TreeEntry>>>,
    /// Digests by real path (keys): links to a file (LLVM's tools) are
    /// hashed once, and digests saved by builds reused.
    hashes: Mutex<HashMap<PathBuf, Kept>>,
    /// Changes the watcher reported (see `Kept::Changed`).
    changes: AtomicU64,
}

impl Files {
    pub fn new(watcher: Arc<Watcher>, case_insensitive: bool) -> Files {
        Files {
            watcher,
            case_insensitive,
            session: AtomicU64::new(1),
            files: RwLock::default(),
            trees: RwLock::default(),
            hashes: Mutex::default(),
            changes: AtomicU64::new(0),
        }
    }

    pub fn new_session(&self) {
        self.session.fetch_add(1, Ordering::Relaxed);
    }

    /// The number of file digests and trees kept.
    pub fn len(&self) -> (usize, usize) {
        (self.files.read().unwrap().len(), self.trees.read().unwrap().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == (0, 0)
    }

    /// Digests a build saved: used for files whose stamp still matches,
    /// unless the daemon knows better (it hashed the file, or saw it
    /// change).
    pub fn seed(&self, entries: impl IntoIterator<Item = (PathBuf, FileStamp, String)>) {
        let mut hashes = self.hashes.lock().unwrap();
        for (p, stamp, hash) in entries {
            hashes.entry(key(&p, self.case_insensitive)).or_insert(Kept::Saved(stamp, Digest { hash, size: stamp.len }));
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

    /// In a build directory (or another one builds write to, see
    /// `Watcher::ignore`).
    fn is_volatile(&self, k: &Path) -> bool {
        self.watcher.is_ignored(k)
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
        let (meta, stamp) = ninja_os::stat(p).map_err(|e| format!("stat {}: {e}", p.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", p.display()));
        }
        if let Some(e) = known
            && e.stamp == stamp
            && !e.racy
        {
            e.session.store(self.checked(token), Ordering::Relaxed);
            return Ok(e.info.clone());
        }
        let d = self.digest(p, stamp, || ninja_os::hash_file(p).map_err(|e| e.to_string()))?;
        let racy = stamp.is_racy(SystemTime::now());
        let info = FileInfo {
            hash: d.hash,
            size: d.size,
            executable: ninja_os::is_executable(&meta),
            mtime_ns: stamp.mtime_ns,
            change: stamp.change,
            created: stamp.created,
            volume: stamp.volume,
            id: stamp.id,
        };
        // The token compared under the lock: an event counted after it
        // drops the entry after it's in.
        let mut files = self.files.write().unwrap();
        files.insert(k, Arc::new(FileEntry { info: info.clone(), stamp, racy, session: AtomicU64::new(self.checked(token)) }));
        Ok(info)
    }

    /// `p`'s digest: the one kept for its version, or `hash`ed.
    fn digest(&self, p: &Path, stamp: FileStamp, hash: impl FnOnce() -> Result<Digest, String>) -> Result<Digest, String> {
        // Links to a file share its entry.
        let is_link = std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
        let real = if is_link { std::fs::canonicalize(p).map(strip_verbatim).unwrap_or_else(|_| p.to_path_buf()) } else { p.to_path_buf() };
        let k = key(&real, self.case_insensitive);
        let seen = match self.hashes.lock().unwrap().entry(k.clone()) {
            Entry::Occupied(e) => match e.get() {
                Kept::Hashed(s, d) | Kept::Saved(s, d) if *s == stamp => return Ok(d.clone()),
                Kept::Changed(n) => Some(*n),
                _ => None,
            },
            // A change while it's hashed has an entry to mark.
            Entry::Vacant(e) => {
                e.insert(Kept::Changed(0));
                Some(0)
            }
        };
        let d = hash()?;
        let mut hashes = self.hashes.lock().unwrap();
        match hashes.get(&k) {
            // Changed while hashed: the digest may be of the old contents,
            // or of a mix.
            Some(Kept::Changed(n)) if Some(*n) != seen => {}
            // It may change again within the same tick, keeping its stamp.
            _ if stamp.is_racy(SystemTime::now()) => {}
            _ => {
                hashes.insert(k, Kept::Hashed(stamp, d.clone()));
            }
        }
        Ok(d)
    }

    /// The files under `dir` (symlinks followed) with their digests, as
    /// `InputTree::add_dir` sees them. One that can't be listed fails for
    /// the session, or when watched, until a change under it: actions don't
    /// each walk it again.
    pub fn tree(&self, dir: &Path) -> Result<Arc<TreeInfo>, String> {
        let k = key(dir, self.case_insensitive);
        if let Some(e) = self.trees.read().unwrap().get(&k)
            && self.current(e.session.load(Ordering::Relaxed))
        {
            return e.info.clone();
        }
        // Watched whole from now on (not in a build directory).
        self.watcher.watch_tree(dir);
        let token = if self.is_volatile(&k) { None } else { self.watcher.tree_token(dir) };
        let mut info = TreeInfo::default();
        let mut outside = false;
        let walked =
            std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display())).and_then(|real| self.walk(dir, "", &real, &mut info, &mut outside));
        let info = walked.map(|()| Arc::new(info));
        let mut trees = self.trees.write().unwrap();
        // Changes outside aren't seen.
        let session = if outside { self.session.load(Ordering::Relaxed) } else { self.checked(token) };
        trees.insert(k, Arc::new(TreeEntry { info: info.clone(), session: AtomicU64::new(session) }));
        info
    }

    /// `outside`: a symbolic link leads out of `real` (the tree's real path).
    fn walk(&self, dir: &Path, rel: &str, real: &Path, out: &mut TreeInfo, outside: &mut bool) -> Result<(), String> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("reading dir {}: {e}", dir.display()))?
            .collect::<std::io::Result<_>>()
            .map_err(|e| format!("reading dir {}: {e}", dir.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            // The watcher's cookies, there for a moment (on Windows, a
            // toolchain root's go at its top), and repositories' own
            // directories, whose changes it doesn't follow.
            if walk_skips(&e.file_name()) {
                continue;
            }
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
                // Its stamp may not tell: a change within a tick of the
                // file system's clock keeps the change time.
                if let Some(kept) = self.hashes.lock().unwrap().get_mut(&k) {
                    *kept = Kept::Changed(self.changes.fetch_add(1, Ordering::Relaxed) + 1);
                }
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
pub const DIGESTS_SIGNATURE: &str = "# ninja-rs digests v3";

/// The entries of a `.ninja_rs_digests` file
/// (`path\tsize\tmtime_ns\tchange\tcreated\tvolume\tid\thash` lines, see
/// `FileStamp`): (path, stamp, hash).
pub fn read_digests(file: &Path) -> Vec<(PathBuf, FileStamp, String)> {
    let Ok(text) = std::fs::read_to_string(file) else { return Vec::new() };
    let mut lines = text.lines();
    if lines.next() != Some(DIGESTS_SIGNATURE) {
        return Vec::new();
    }
    lines
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 8 {
                return None;
            }
            let (Ok(len), Ok(mtime_ns), Ok(change), Ok(created), Ok(volume), Ok(id)) =
                (f[1].parse(), f[2].parse(), f[3].parse(), f[4].parse(), f[5].parse(), f[6].parse())
            else {
                return None;
            };
            Some((PathBuf::from(f[0]), FileStamp { len, mtime_ns, change, created, volume, id }, f[7].to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn digests_and_trees() {
        let dir = test_dirs::base().join(format!("ninja_daemon_files_{}", std::process::id()));
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
        files.seed([(b.clone(), ninja_os::stat(&b).unwrap().1, "made-up".to_string())]);
        assert_eq!(files.get(&b).unwrap().hash, "made-up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file a watcher saw change is hashed again, whatever its stamp:
    /// neither the digest kept nor one a build saved is used for it.
    #[test]
    fn events_drop_digests() {
        let dir = test_dirs::base().join(format!("ninja_daemon_events_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let b = dir.join("b.txt");
        std::fs::write(&b, "bbb").unwrap();
        let stamp = ninja_os::stat(&b).unwrap().1;
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        files.seed([(b.clone(), stamp, "made-up".to_string())]);
        assert_eq!(files.get(&b).unwrap().hash, "made-up");
        files.changed(&b, Change::Content);
        // Another build's saved digests, read meanwhile.
        files.seed([(b.clone(), stamp, "made-up".to_string())]);
        files.new_session();
        assert_eq!(files.get(&b).unwrap().hash, "3e744b9dc39389baf0c5a0660589b8402f3dbb49b89b3e75f2c9355852a3c677");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A change reported while a file is hashed: the digest isn't kept for
    /// its stamp (it may be of the old contents, or of a mix). Nor is one of
    /// a file changed within its stamp's granularity: changed again, it
    /// could keep that stamp.
    #[test]
    fn changed_while_hashed() {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_inflight_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (b, c) = (dir.join("b.txt"), dir.join("c.txt"));
        std::fs::write(&b, "bbb").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options().write(true).open(&b).unwrap().set_modified(old).unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        let digest = |h: &str| Digest { hash: h.into(), size: 3 };
        let kept = |p: &Path| match files.hashes.lock().unwrap().get(&key(p, cfg!(windows))) {
            Some(Kept::Hashed(_, d)) => Some(d.hash.clone()),
            Some(Kept::Changed(_)) => Some("changed".into()),
            _ => None,
        };
        let stamp = ninja_os::stat(&b).unwrap().1;
        files.digest(&b, stamp, || Ok(digest("first"))).unwrap();
        assert_eq!(kept(&b).as_deref(), Some("first"));
        files.changed(&b, Change::Content);
        files
            .digest(&b, stamp, || {
                files.changed(&b, Change::Content);
                Ok(digest("mixed"))
            })
            .unwrap();
        assert_eq!(kept(&b).as_deref(), Some("changed"));
        files.digest(&b, stamp, || Ok(digest("second"))).unwrap();
        assert_eq!(kept(&b).as_deref(), Some("second"));
        // Hashed for the first time.
        let d = dir.join("d.txt");
        std::fs::write(&d, "ddd").unwrap();
        std::fs::File::options().write(true).open(&d).unwrap().set_modified(old).unwrap();
        files
            .digest(&d, ninja_os::stat(&d).unwrap().1, || {
                files.changed(&d, Change::Content);
                Ok(digest("mixed"))
            })
            .unwrap();
        assert_eq!(kept(&d).as_deref(), Some("changed"));
        std::fs::write(&c, "ccc").unwrap();
        files.get(&c).unwrap();
        assert!(kept(&c).is_none_or(|h| h == "changed"), "{:?}", kept(&c));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows: a file rewritten in place with its modification time
    /// restored, or created again under its name (NTFS gives it its old
    /// creation time back), where no watch sees it: hashed again.
    #[cfg(windows)]
    #[test]
    fn windows_stamps() {
        let dir = test_dirs::base().join(format!("ninja_daemon_stamps_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f.h");
        std::fs::write(&f, "version 1").unwrap();
        let mtime = std::fs::metadata(&f).unwrap().modified().unwrap();
        let w = Watcher::new(true, Box::new(|_| {}));
        let files = Files::new(w.clone(), true);
        for (i, recreate) in [false, true].into_iter().enumerate() {
            files.new_session();
            let before = files.get(&f).unwrap();
            if recreate {
                std::fs::remove_file(&f).unwrap();
            }
            std::fs::write(&f, format!("version {}", i + 2)).unwrap();
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(mtime).unwrap();
            let (after, was) = (ninja_os::stat(&f).unwrap().1, before.stamp());
            assert_eq!((after.len, after.mtime_ns, after.created), (was.len, was.mtime_ns, was.created), "the same size and times");
            files.new_session();
            assert_ne!(files.get(&f).unwrap().hash, before.hash, "recreated: {recreate}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The watcher's cookies (on Windows, at the top of a toolchain root
    /// for a moment) aren't part of trees.
    #[test]
    fn trees_skip_cookies() {
        let dir = test_dirs::base().join(format!("ninja_daemon_skip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tc")).unwrap();
        std::fs::write(dir.join("tc/cc"), "cc").unwrap();
        std::fs::write(dir.join("tc/.ninja-rs-cookie-1-2"), "").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        let t = files.tree(&dir.join("tc")).unwrap();
        assert_eq!(t.files.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), vec!["cc"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Repositories' own directories aren't part of trees (a toolchain that
    /// is a checkout): their changes aren't followed.
    #[test]
    fn trees_skip_repositories() {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_skip_vcs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tc/.git/refs")).unwrap();
        std::fs::create_dir_all(dir.join("tc/sub/.hg")).unwrap();
        std::fs::write(dir.join("tc/cc"), "cc").unwrap();
        std::fs::write(dir.join("tc/.git/refs/main"), "1").unwrap();
        std::fs::write(dir.join("tc/sub/.hg/x"), "x").unwrap();
        std::fs::write(dir.join("tc/sub/.git"), "gitdir: elsewhere").unwrap();
        std::fs::write(dir.join("tc/sub/h.h"), "h").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        let t = files.tree(&dir.join("tc")).unwrap();
        assert_eq!(t.files.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), vec!["cc", "sub/h.h"]);
        assert_eq!(t.dirs, vec!["sub"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tree that can't be listed fails once a session (or, watched, until
    /// a change under it): actions don't each walk it again.
    #[test]
    fn tree_failures_kept() {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_tree_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tc = dir.join("tc");
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        assert!(files.tree(&tc).is_err());
        std::fs::create_dir_all(&tc).unwrap();
        std::fs::write(tc.join("cc"), "cc").unwrap();
        assert!(files.tree(&tc).is_err());
        files.new_session();
        assert_eq!(files.tree(&tc).unwrap().files.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lookups don't slow down with the build directories seen (CMake's
    /// `try_compile` names hundreds a day); those no build named for a
    /// while are forgotten.
    #[test]
    fn many_build_dirs() {
        let dir = test_dirs::base().join(format!("ninja_daemon_many_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let f = dir.join("src/x.h");
        std::fs::write(&f, "x").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        let scratch = |i: usize| dir.join(format!("build/CMakeFiles/CMakeScratch/TryCompile-{i:06x}"));
        for i in 0..10_000 {
            w.ignore(&scratch(i));
        }
        files.get(&f).unwrap();
        let t = Instant::now();
        for _ in 0..5_000 {
            files.get(&f).unwrap();
            w.token(&f);
        }
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
        let k = |p: &Path| key(p, cfg!(windows));
        assert!(files.is_volatile(&k(&scratch(42).join("a.o"))));
        std::thread::sleep(Duration::from_millis(10));
        let before = Instant::now();
        w.ignore(&scratch(7));
        w.expire(before);
        assert!(files.is_volatile(&k(&scratch(7).join("a.o"))));
        assert!(!files.is_volatile(&k(&scratch(42).join("a.o"))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_files() {
        let f = test_dirs::base().join(format!("ninja_daemon_digests_{}", std::process::id()));
        std::fs::write(&f, format!("{DIGESTS_SIGNATURE}\n/a/b\t3\t1000\t7\t8\t5\t{}\tabc\nbad line\n", u128::MAX)).unwrap();
        let d = read_digests(&f);
        let stamp = FileStamp { len: 3, mtime_ns: 1000, change: 7, created: 8, volume: 5, id: u128::MAX };
        assert_eq!(d, vec![(PathBuf::from("/a/b"), stamp, "abc".to_string())]);
        let _ = std::fs::remove_file(&f);
    }
}
