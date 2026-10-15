//! File digests and directory trees, kept until the watcher reports a
//! change (or, for what it doesn't watch, checked once per build).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::SystemTime;

use cpp_deps::scan::Watch;
use ninja_os::{Digest, FileStamp};

use crate::proto::{FileInfo, TreeInfo};
use crate::watch::{Change, Listener, PathMap, Watcher, key, strip_verbatim, walk_skips};

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

/// A digest being computed, or why it couldn't be.
type Hashing = Arc<OnceLock<Result<Digest, (io::ErrorKind, String)>>>;

pub struct Files {
    watcher: Arc<Watcher>,
    case_insensitive: bool,
    session: AtomicU64,
    /// As the watcher's maps: what's under a directory goes without going
    /// through the rest.
    files: RwLock<PathMap<Arc<FileEntry>>>,
    trees: RwLock<PathMap<Arc<TreeEntry>>>,
    /// Digests by real path (keys): links to a file (LLVM's tools) are
    /// hashed once, and digests saved by builds reused.
    hashes: Mutex<PathMap<Kept>>,
    /// Digests being computed, by real path (key) and stamp: concurrent
    /// requests for a file (every action's compiler) wait for the one
    /// hashing it.
    hashing: Mutex<HashMap<(PathBuf, FileStamp), Hashing>>,
    /// Trees being listed (keys): concurrent requests (nested builds', or a
    /// build's after one that outlived its client's patience) wait for the
    /// walk, and take what it found when it's for them.
    walking: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Changes the watcher reported (see `Kept::Changed`).
    changes: AtomicU64,
}

/// `e`, saying what it was doing with `p`.
fn context(what: &str, p: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{what} {}: {e}", p.display()))
}

/// Whether a tree's failure lasts the session (or, watched, until a change
/// under it): the walk would fail the same way (a dangling link, a file
/// gone or unreadable), unlike one out of file descriptors, or a file
/// locked for a moment (an installer, a virus scanner).
fn lasting(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(e.kind(), NotFound | PermissionDenied | NotADirectory | IsADirectory | InvalidInput)
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
            hashing: Mutex::default(),
            walking: Mutex::default(),
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
            let k = key(&p, self.case_insensitive);
            if !hashes.contains_key(&k) {
                hashes.insert(k, Kept::Saved(stamp, Digest { hash, size: stamp.len }));
            }
        }
    }

    fn current(&self, session: u64) -> bool {
        session == WATCHED || session == self.session.load(Ordering::Relaxed)
    }

    /// The session to record for a check that started with `token` in
    /// `session` (a check that spans a new session's start is the old
    /// one's).
    fn checked(&self, token: Option<u64>, session: u64) -> u64 {
        match token {
            Some(t) if self.watcher.unchanged_since(t) => WATCHED,
            _ => session,
        }
    }

    /// A file's digest (symlinks followed).
    pub fn get(&self, p: &Path) -> Result<FileInfo, String> {
        self.check(p).map(|(info, _)| info).map_err(|e| e.to_string())
    }

    /// A file's digest, and whether the watcher vouches for it.
    fn check(&self, p: &Path) -> io::Result<(FileInfo, bool)> {
        let k = key(p, self.case_insensitive);
        let known = self.files.read().unwrap().get(&k).cloned();
        // A file the build may write is checked every time: a generator
        // can rewrite it between two actions of a session.
        let volatile = self.watcher.volatile_file(&k);
        if let Some(e) = &known
            && !volatile
        {
            let session = e.session.load(Ordering::Relaxed);
            if self.current(session) {
                return Ok((e.info.clone(), session == WATCHED));
            }
        }
        let session = self.session.load(Ordering::Relaxed);
        let token = if volatile { None } else { self.watcher.token(p) };
        let (meta, stamp) = ninja_os::stat(p).map_err(|e| context("stat", p, e))?;
        if !meta.is_file() {
            let kind = if meta.is_dir() { io::ErrorKind::IsADirectory } else { io::ErrorKind::InvalidInput };
            return Err(io::Error::new(kind, format!("{} is not a file", p.display())));
        }
        if let Some(e) = known
            && e.stamp == stamp
            && !e.racy
        {
            let checked = self.checked(token, session);
            e.session.store(checked, Ordering::Relaxed);
            return Ok((e.info.clone(), checked == WATCHED));
        }
        let d = self.digest(p, stamp, || ninja_os::hash_file(p))?;
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
        let checked = self.checked(token, session);
        files.insert(k, Arc::new(FileEntry { info: info.clone(), stamp, racy, session: AtomicU64::new(checked) }));
        Ok((info, checked == WATCHED))
    }

    /// `p`'s digest: the one kept for its version, or `hash`ed (once for
    /// concurrent requests of that version).
    fn digest(&self, p: &Path, stamp: FileStamp, hash: impl FnOnce() -> io::Result<Digest>) -> io::Result<Digest> {
        // Links to a file share its entry.
        let is_link = std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
        let real = if is_link { std::fs::canonicalize(p).map(strip_verbatim).unwrap_or_else(|_| p.to_path_buf()) } else { p.to_path_buf() };
        let k = key(&real, self.case_insensitive);
        let seen = {
            let mut hashes = self.hashes.lock().unwrap();
            match hashes.get(&k) {
                Some(Kept::Hashed(s, d) | Kept::Saved(s, d)) if *s == stamp => return Ok(d.clone()),
                Some(Kept::Changed(n)) => Some(*n),
                Some(_) => None,
                // A change while it's hashed has an entry to mark.
                None => {
                    hashes.insert(k.clone(), Kept::Changed(0));
                    Some(0)
                }
            }
        };
        let version = (k.clone(), stamp);
        let cell = self.hashing.lock().unwrap().entry(version.clone()).or_default().clone();
        let mut hashed = false;
        let d = cell
            .get_or_init(|| {
                hashed = true;
                hash().map_err(|e| (e.kind(), e.to_string()))
            })
            .clone();
        if !hashed {
            return d.map_err(|(kind, e)| io::Error::new(kind, e));
        }
        self.hashing.lock().unwrap().remove(&version);
        let d = d.map_err(|(kind, e)| io::Error::new(kind, e))?;
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

    /// The tree of `k`, if one is kept for this session.
    fn kept_tree(&self, k: &Path) -> Option<Result<Arc<TreeInfo>, String>> {
        self.trees.read().unwrap().get(k).filter(|e| self.current(e.session.load(Ordering::Relaxed))).map(|e| e.info.clone())
    }

    /// The files under `dir` (symlinks followed) with their digests, as
    /// `InputTree::add_dir` sees them. Kept until a change under it when the
    /// watcher vouches for its files, but for their other names when the
    /// walk sees those all in the tree (hard links, symbolic links within
    /// it): a change through any is reported under it. One that can't be
    /// listed for a lasting reason (see `lasting`) fails for the session,
    /// or when watched, until a change under it: actions don't each walk it
    /// again. Other failures are tried again.
    pub fn tree(&self, dir: &Path) -> Result<Arc<TreeInfo>, String> {
        let k = key(dir, self.case_insensitive);
        if let Some(t) = self.kept_tree(&k) {
            return t;
        }
        let lock = self.walking.lock().unwrap().entry(k.clone()).or_default().clone();
        let walked = {
            let _walk = lock.lock().unwrap();
            match self.kept_tree(&k) {
                Some(t) => t,
                None => self.walk_tree(dir, &k),
            }
        };
        // The last one done with it.
        let mut walking = self.walking.lock().unwrap();
        if walking.get(&k).is_some_and(|l| Arc::ptr_eq(l, &lock) && Arc::strong_count(l) == 2) {
            walking.remove(&k);
        }
        walked
    }

    fn walk_tree(&self, dir: &Path, k: &Path) -> Result<Arc<TreeInfo>, String> {
        let session = self.session.load(Ordering::Relaxed);
        // Watched whole from now on (not in a build directory).
        self.watcher.watch_tree(dir);
        let token = if self.watcher.volatile_dir(k) { None } else { self.watcher.tree_token(dir) };
        let mut walk = Walk::default();
        let walked = std::fs::canonicalize(dir).map_err(|e| context("listing", dir, e)).and_then(|real| self.walk(dir, "", &real, &mut walk));
        // Every name of a file with others is in the tree: a change through
        // any is reported under it.
        walk.unwatched |= walk.names.values().any(|(names, seen)| seen != names);
        let kept = walked.as_ref().map_or_else(lasting, |_| true);
        let info = walked.map(|()| Arc::new(walk.info)).map_err(|e| e.to_string());
        if kept {
            let mut trees = self.trees.write().unwrap();
            // Changes outside, or to a file with other names, aren't seen.
            let session = if walk.unwatched { session } else { self.checked(token, session) };
            trees.insert(k.to_path_buf(), Arc::new(TreeEntry { info: info.clone(), session: AtomicU64::new(session) }));
        }
        info
    }

    fn walk(&self, dir: &Path, rel: &str, real: &Path, w: &mut Walk) -> io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir).and_then(|rd| rd.collect::<io::Result<_>>()).map_err(|e| context("reading dir", dir, e))?;
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
            // A link leading out of the tree (or where walks don't go): what
            // it leads to may change unseen.
            let link = e.file_type().is_ok_and(|t| t.is_symlink());
            if link {
                w.unwatched |= !std::fs::canonicalize(&path).is_ok_and(|t| t.strip_prefix(real).is_ok_and(|t| !t.iter().any(walk_skips)));
            }
            let meta = std::fs::metadata(&path).map_err(|e| context("stat", &path, e))?;
            if meta.is_dir() {
                w.info.dirs.push(child.clone());
                self.walk(&path, &child, real, w)?;
            } else {
                let (info, vouched) = self.check(&path)?;
                // Not alone, for its other names: the tree is, if the walk
                // sees them all.
                if !vouched {
                    match self.watcher.walked_token(&path) {
                        Some(_) if link => {}
                        Some((_, names)) if names > 1 => w.names.entry(identity(&meta, &info)).or_insert((names, 0)).1 += 1,
                        _ => w.unwatched = true,
                    }
                }
                w.info.files.push((child, info));
            }
        }
        Ok(())
    }

    /// Forgets what's kept under `dir`, digests included (nothing is asked
    /// for there again: see `Listener::forget`).
    fn forget_under(&self, dir: &Path) {
        let k = key(dir, self.case_insensitive);
        let n = self.hashes.lock().unwrap().remove_under(&k);
        self.watcher.sweep(n + 1);
    }
}

/// What a tree's walk found.
#[derive(Default)]
struct Walk {
    info: TreeInfo,
    /// A file the watcher doesn't vouch for (a link leading out of the
    /// tree, a file with names outside it, a change seen during the walk):
    /// the tree holds for the session only.
    unwatched: bool,
    /// Files with other names (hard links: GCC's `gcc` and `<target>-gcc`,
    /// Git's commands), by identity: how many names each has, and how many
    /// the walk saw.
    names: HashMap<(u64, u128), (u64, u64)>,
}

/// A file's identity: its device or volume, and its inode or file ID.
#[cfg(unix)]
fn identity(meta: &std::fs::Metadata, _info: &FileInfo) -> (u64, u128) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), u128::from(meta.ino()))
}

#[cfg(not(unix))]
fn identity(_meta: &std::fs::Metadata, info: &FileInfo) -> (u64, u128) {
    (info.volume, info.id)
}

impl Listener for Files {
    fn changed(&self, path: &Path, change: Change) {
        let k = key(path, self.case_insensitive);
        let mut swept = 1;
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
                swept += self.files.write().unwrap().remove_under(&k);
            }
        }
        // A tree changes with anything under it.
        let hit = {
            let trees = self.trees.read().unwrap();
            !trees.is_empty() && (k.ancestors().any(|a| trees.contains_key(a)) || trees.has_under(&k))
        };
        if hit {
            let mut trees = self.trees.write().unwrap();
            for a in k.ancestors().skip(1) {
                swept += usize::from(trees.remove(a).is_some());
            }
            swept += trees.remove_under(&k);
        }
        self.watcher.sweep(swept);
    }

    fn forget(&self, dir: &Path) {
        self.changed(dir, Change::Tree);
        self.forget_under(dir);
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
    use test_dirs::TestDir;

    #[test]
    fn digests_and_trees() {
        let dir = TestDir::new("ninja_daemon_files");
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
    }

    /// A file a watcher saw change is hashed again, whatever its stamp:
    /// neither the digest kept nor one a build saved is used for it.
    #[test]
    fn events_drop_digests() {
        let dir = TestDir::new("ninja_daemon_events");
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
    }

    /// A change reported while a file is hashed: the digest isn't kept for
    /// its stamp (it may be of the old contents, or of a mix). Nor is one of
    /// a file changed within its stamp's granularity: changed again, it
    /// could keep that stamp.
    #[test]
    fn changed_while_hashed() {
        let dir = TestDir::new("ninja_daemon_inflight");
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
        // Modified within its time's granularity of now (here, later).
        std::fs::write(&c, "ccc").unwrap();
        std::fs::File::options().write(true).open(&c).unwrap().set_modified(SystemTime::now() + Duration::from_secs(3600)).unwrap();
        files.get(&c).unwrap();
        assert!(kept(&c).is_none_or(|h| h == "changed"), "{:?}", kept(&c));
    }

    /// Requests for a file being hashed (every action's compiler, at the
    /// start of a build) wait for that digest rather than hash it again.
    #[test]
    fn hashed_once() {
        let dir = TestDir::new("ninja_daemon_hashed_once");
        let f = dir.join("cc");
        std::fs::write(&f, "cc").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Arc::new(Files::new(w.clone(), cfg!(windows)));
        let stamp = ninja_os::stat(&f).unwrap().1;
        let version = (key(&f, cfg!(windows)), stamp);
        let (started, go) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
        let first = {
            let (files, f, started, go) = (files.clone(), f.clone(), started.0, go.1);
            std::thread::spawn(move || {
                files.digest(&f, stamp, || {
                    started.send(()).unwrap();
                    go.recv().unwrap();
                    Ok(Digest { hash: "the one".into(), size: 2 })
                })
            })
        };
        started.1.recv().unwrap();
        let second = {
            let (files, f) = (files.clone(), f.clone());
            std::thread::spawn(move || files.digest(&f, stamp, || panic!("hashed twice")))
        };
        // The second waits for the first.
        let waiting = || files.hashing.lock().unwrap().get(&version).is_some_and(|c| Arc::strong_count(c) == 3);
        let deadline = Instant::now() + Duration::from_secs(60);
        while !waiting() {
            assert!(Instant::now() < deadline, "the second request doesn't wait");
            std::thread::sleep(Duration::from_millis(1));
        }
        go.0.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap().hash, "the one");
        assert_eq!(second.join().unwrap().unwrap().hash, "the one");
        assert!(files.hashing.lock().unwrap().is_empty());
    }

    /// Concurrent requests for a tree walk it once: the others wait, and
    /// take what it found.
    #[test]
    fn trees_walked_once() {
        let dir = TestDir::new("ninja_daemon_walked_once");
        std::fs::create_dir_all(dir.join("tc/bin")).unwrap();
        std::fs::write(dir.join("tc/bin/cc"), "cc").unwrap();
        let tc = dir.join("tc");
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Arc::new(Files::new(w.clone(), cfg!(windows)));
        let k = key(&tc, cfg!(windows));
        // A walk in progress.
        let walk = files.walking.lock().unwrap().entry(k.clone()).or_default().clone();
        let held = walk.lock().unwrap();
        let requests: Vec<_> = (0..2)
            .map(|_| {
                let (files, tc) = (files.clone(), tc.clone());
                std::thread::spawn(move || files.tree(&tc).unwrap())
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Arc::strong_count(&walk) < 4 {
            assert!(Instant::now() < deadline, "the requests don't wait");
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(held);
        let trees: Vec<Arc<TreeInfo>> = requests.into_iter().map(|r| r.join().unwrap()).collect();
        assert!(Arc::ptr_eq(&trees[0], &trees[1]));
    }

    /// Windows: a file of a tree locked for a moment (an installer, a virus
    /// scanner): the tree fails, and is walked again once it's free, in
    /// the same session (the failure used to be kept until a change under
    /// it, perhaps for the daemon's life). A tree missing fails for the
    /// session.
    #[cfg(windows)]
    #[test]
    fn transient_tree_failures() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = TestDir::new("ninja_daemon_tree_locked");
        std::fs::create_dir_all(dir.join("tc/bin")).unwrap();
        let cc = dir.join("tc/bin/cc");
        std::fs::write(&cc, "cc").unwrap();
        let w = Watcher::new(true, Box::new(|_| {}));
        let files = Files::new(w.clone(), true);
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&cc).unwrap();
        assert!(files.tree(&dir.join("tc")).is_err());
        drop(lock);
        assert_eq!(files.tree(&dir.join("tc")).unwrap().files.len(), 1);
    }

    /// Windows: a file rewritten in place with its modification time
    /// restored, or created again under its name (NTFS gives it its old
    /// creation time back), where no watch sees it: hashed again.
    #[cfg(windows)]
    #[test]
    fn windows_stamps() {
        let dir = TestDir::new("ninja_daemon_stamps");
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
    }

    /// The watcher's cookies (on Windows, at the top of a toolchain root
    /// for a moment) aren't part of trees.
    #[test]
    fn trees_skip_cookies() {
        let dir = TestDir::new("ninja_daemon_skip");
        std::fs::create_dir_all(dir.join("tc")).unwrap();
        std::fs::write(dir.join("tc/cc"), "cc").unwrap();
        std::fs::write(dir.join("tc/.ninja-rs-cookie-1-2"), "").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        let t = files.tree(&dir.join("tc")).unwrap();
        assert_eq!(t.files.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), vec!["cc"]);
    }

    /// Repositories' own directories aren't part of trees (a toolchain that
    /// is a checkout): their changes aren't followed.
    #[test]
    fn trees_skip_repositories() {
        let dir = TestDir::new("ninja_daemon_skip_vcs");
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
    }

    /// A tree that can't be listed (missing, a dangling link) fails once a
    /// session (or, watched, until a change under it): actions don't each
    /// walk it again.
    #[test]
    fn tree_failures_kept() {
        let dir = TestDir::new("ninja_daemon_tree_err");
        let tc = dir.join("tc");
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let files = Files::new(w.clone(), cfg!(windows));
        assert!(files.tree(&tc).is_err());
        std::fs::create_dir_all(&tc).unwrap();
        std::fs::write(tc.join("cc"), "cc").unwrap();
        assert!(files.tree(&tc).is_err());
        files.new_session();
        assert_eq!(files.tree(&tc).unwrap().files.len(), 1);
    }

    /// Lookups don't cost more with the build directories seen (CMake's
    /// `try_compile` names hundreds a day): a lookup per ancestor, as with
    /// one (in a checkout, which the test's directory may be in, a few
    /// more). Those no build named for a while are forgotten, with what was
    /// kept for their files.
    #[test]
    fn many_build_dirs() {
        let dir = TestDir::new("ninja_daemon_many");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let f = dir.join("src/x.h");
        std::fs::write(&f, "x").unwrap();
        let scratch = |i: usize| dir.join(format!("build/CMakeFiles/CMakeScratch/TryCompile-{i:06x}"));
        // What 1,000 lookups cost with `n` build directories, once the
        // root the file is in (if any) is set up.
        let lookups = |n: usize| {
            let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
            let files = Arc::new(Files::new(w.clone(), cfg!(windows)));
            w.listen(files.clone());
            for i in 0..n {
                w.ignore(&scratch(i), true);
            }
            files.get(&f).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while w.roots().iter().any(|(_, s)| *s == crate::watch::RootState::Starting) {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            let before = Watcher::probes();
            for _ in 0..1_000 {
                files.get(&f).unwrap();
                w.token(&f);
            }
            (Watcher::probes() - before, w, files)
        };
        let (one, ..) = lookups(1);
        let (many, w, files) = lookups(10_000);
        let depth = f.ancestors().count() as u64;
        assert!(many <= one + 1_000 * depth, "{many} probes, {one} with one build directory");
        let k = |p: &Path| key(p, cfg!(windows));
        assert!(w.volatile_file(&k(&scratch(42).join("a.o"))));
        // An object built in one, then not named for a while.
        std::fs::create_dir_all(scratch(42)).unwrap();
        std::fs::write(scratch(42).join("a.o"), "o").unwrap();
        files.get(&scratch(42).join("a.o")).unwrap();
        assert_eq!(files.len().0, 2);
        std::thread::sleep(Duration::from_millis(10));
        let before = Instant::now();
        w.ignore(&scratch(7), true);
        w.expire(before);
        assert!(w.volatile_file(&k(&scratch(7).join("a.o"))));
        assert!(!w.volatile_file(&k(&scratch(42).join("a.o"))));
        assert_eq!(files.len().0, 1);
        assert!(!files.hashes.lock().unwrap().keys().any(|p| p.starts_with(k(&scratch(42)))));
    }

    /// A tree whose files with other names have them all in it (GCC's
    /// `gcc` and `<target>-gcc`, hard links; LLVM's `clang++`, a symbolic
    /// link to `clang`) is vouched for: its walk sees every name, and a
    /// change through any is reported under it. Such trees were walked
    /// again every build. One with a name outside isn't.
    #[test]
    fn linked_trees() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_linked_trees");
        for d in ["tc/bin", "tc2/bin", "elsewhere"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = strip_verbatim(std::fs::canonicalize(&*base).unwrap());
        let (tc, tc2) = (dir.join("tc"), dir.join("tc2"));
        let gcc = tc.join("bin/gcc");
        std::fs::write(&gcc, "gcc").unwrap();
        std::fs::hard_link(&gcc, tc.join("bin/x86_64-linux-gnu-gcc")).unwrap();
        std::fs::write(tc.join("bin/clang"), "clang").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("clang", tc.join("bin/clang++")).unwrap();
        // A privilege not every user has.
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_file("clang", tc.join("bin/clang++"));
        std::fs::write(tc2.join("bin/cc"), "cc").unwrap();
        std::fs::hard_link(tc2.join("bin/cc"), dir.join("elsewhere/cc")).unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.set_cookie_dir(&dir.join("cookies"));
        let files = Arc::new(Files::new(w.clone(), cfg!(windows)));
        w.listen(files.clone());
        let watched = |t: &Path| files.trees.read().unwrap().get(&key(t, cfg!(windows))).is_some_and(|e| e.session.load(Ordering::Relaxed) == WATCHED);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !watched(&tc) {
            assert!(Instant::now() < deadline, "{:?}", w.roots());
            w.sync();
            files.new_session();
            files.tree(&tc).unwrap();
            files.tree(&tc2).unwrap();
        }
        assert!(!watched(&tc2));
        // Changed through its other name, before the next build: walked
        // again.
        let before = files.tree(&tc).unwrap();
        std::fs::write(tc.join("bin/x86_64-linux-gnu-gcc"), "gcc 2").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let after = loop {
            files.new_session();
            let t = files.tree(&tc).unwrap();
            if !Arc::ptr_eq(&t, &before) {
                break t;
            }
            assert!(Instant::now() < deadline, "the change wasn't seen");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(after.files.iter().find(|(f, _)| f == "bin/gcc").map(|(_, i)| i.size), Some(5));
    }

    /// A root gone (a checkout removed) takes what was kept for it along,
    /// digests included: those of the files at its top, under no directory
    /// known, stayed for the daemon's life.
    #[test]
    fn roots_gone_forgotten() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_gone_digests");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/src")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&*base).unwrap());
        let repo = dir.join("repo");
        let (a, b) = (repo.join("a.h"), repo.join("src/b.h"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.set_cookie_dir(&dir.join("cookies"));
        let files = Arc::new(Files::new(w.clone(), cfg!(windows)));
        w.listen(files.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        while w.token(&a).is_none() {
            assert!(Instant::now() < deadline, "{:?}", w.roots());
            files.get(&a).unwrap();
            files.get(&b).unwrap();
            w.sync();
        }
        test_dirs::remove_all(&repo).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !w.roots().is_empty() {
            assert!(Instant::now() < deadline, "{:?}", w.roots());
            w.sync();
            std::thread::sleep(Duration::from_millis(10));
        }
        let k = key(&repo, cfg!(windows));
        assert!(!files.hashes.lock().unwrap().keys().any(|p| p.starts_with(&k)), "{:?}", files.hashes.lock().unwrap().keys().collect::<Vec<_>>());
    }

    #[test]
    fn digest_files() {
        let dir = TestDir::new("ninja_daemon_digests");
        let f = dir.join("digests");
        std::fs::write(&f, format!("{DIGESTS_SIGNATURE}\n/a/b\t3\t1000\t7\t8\t5\t{}\tabc\nbad line\n", u128::MAX)).unwrap();
        let d = read_digests(&f);
        let stamp = FileStamp { len: 3, mtime_ns: 1000, change: 7, created: 8, volume: 5, id: u128::MAX };
        assert_eq!(d, vec![(PathBuf::from("/a/b"), stamp, "abc".to_string())]);
    }
}
