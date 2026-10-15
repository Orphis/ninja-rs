//! Watching the files the daemon knows, so that what it checked stays
//! valid until they change.
//!
//! - **Roots**: the repository a checked file is in (the nearest
//!   directory with `.git`, `.hg`, `.jj`, `.sl` or `.svn`), and directories
//!   the daemon was asked for whole (toolchain inputs). Files elsewhere
//!   (system headers) aren't watched: they're checked once per build, as
//!   without the daemon. So are build directories and the others builds
//!   write to, whose files change during builds (forgotten once no build
//!   named them for a while): nothing in them is watched, nor is a root
//!   made there. A root found around others (a checkout whose toolchain
//!   was listed first) takes them over.
//! - **Lazily** (inotify, a watch per directory): a repository's root is
//!   watched alone at first, then the directories of the files checked in
//!   it (and those listed), with those above them (whose renaming moves
//!   the files), vouched for once a sync followed; directories asked for
//!   whole (a toolchain in the source tree) are watched with everything
//!   under them. A dotfiles repository in the home directory doesn't watch
//!   the home directory, nor other build trees in a checkout. Out of
//!   watches (ENOSPC), a root gives back all of its own (IDEs need them
//!   too) and is checked once per build for a while.
//!   `ReadDirectoryChangesW` watches a whole tree with one handle: roots
//!   are watched whole.
//! - **Trust.** A root's events are only trusted once it's in sync: before
//!   each build, the daemon creates a file (a cookie) for every root it can
//!   write to and waits for its event, which comes after every change made
//!   before (Watchman's technique). Cookies go in the repository's own
//!   directory (`.git` and the like) when there is one; else, with inotify,
//!   in a directory of the daemon's (one queue carries every watch's
//!   events, in order); else at the root's top (a Windows worktree or
//!   submodule, whose `.git` is a file, or a toolchain). Tree walks skip
//!   both. A root that can't be written to is trusted without a cookie
//!   (system directories change rarely); one whose cookie can't be made
//!   otherwise, or doesn't come back, is watched again. A root whose
//!   watcher lost events (a queue overflow) is out of sync again, and
//!   everything under it is checked again; with inotify, it's watched
//!   again from scratch (a directory replaced meanwhile took its watch
//!   along unseen), vouched for after the next sync. So is a root replaced
//!   (renamed, removed, mounted over) or one with file systems mounted or
//!   unmounted under it (Linux), and a directory whose watch went (its
//!   file system was unmounted).
//! - **Events** drop the entries they concern (`Listener`): a file's, the
//!   names of its directory when it's created, removed or renamed, and
//!   everything under a directory that was.
//! - **Paths.** Only paths without symbolic links are vouched for: events
//!   come for real paths. Nor are paths in a repository's own directory,
//!   whose events are ignored (git writes there all the time), nor, on
//!   Linux, on file systems under a root that may not report every change.
//!
//! Backends: inotify on Linux (the build directories aren't watched, so
//!   that compiles writing objects don't flood the queue),
//!   `ReadDirectoryChangesW` on Windows (whose events in build directories
//!   are dropped before anything else is done with them). Elsewhere
//!   nothing is watched.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

pub use cpp_deps::scan::Change;

/// Gets what changed.
pub trait Listener: Send + Sync {
    fn changed(&self, path: &Path, change: Change);
}

impl Listener for cpp_deps::Driver {
    fn changed(&self, path: &Path, change: Change) {
        self.cache.changed(path, change);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootState {
    /// Its watch is being set up.
    Starting,
    /// Watched, but events may be missing: nothing is trusted yet.
    Unsynced,
    Trusted,
    Failed(String),
}

struct Root {
    path: PathBuf,
    key: PathBuf,
    state: RootState,
    /// Where cookies go: its `.git` when it has one, else the root.
    cookie_dir: PathBuf,
    /// Watched whole, not lazily (a toolchain directory).
    whole: bool,
    /// The sync that last brought it in sync (see `Watcher::epoch`).
    synced: u64,
    /// Failed for want of watches: not started again before then.
    retry_after: Option<Instant>,
    /// What its directory was when its watch was set up: one replaced
    /// since (or mounted over) isn't the one watched.
    id: Option<DirId>,
    /// Linux: the file systems mounted in it then (keys), and whether each
    /// reports every change.
    mounts: Vec<(PathBuf, bool)>,
}

/// Directories marking a repository: its root is watched; nothing inside
/// them is vouched for, and their events are ignored.
pub const VCS_DIRS: &[&str] = &[".git", ".hg", ".jj", ".sl", ".svn"];

/// Names of the files the daemon creates for a moment to sync a root: tree
/// walks skip them, its own and the builds' alike.
pub const COOKIE_PREFIX: &str = ".ninja-rs-cookie-";

/// Whether tree walks skip an entry named `name`, the daemon's and the
/// builds' alike: the watcher's cookies, and repositories' own directories
/// (or a worktree's `.git` file), whose changes it doesn't follow.
pub fn walk_skips(name: &std::ffi::OsStr) -> bool {
    VCS_DIRS.iter().any(|v| name == *v) || name.to_string_lossy().starts_with(COOKIE_PREFIX)
}

/// Whether `key` is in a repository's own directory, or is one.
fn in_repository_dir(key: &Path) -> bool {
    key.components().any(|c| matches!(c, Component::Normal(n) if VCS_DIRS.iter().any(|v| n == *v)))
}

/// A directory's identity: its device (volume) and inode (file ID).
type DirId = (u64, u128);

#[cfg(unix)]
fn dir_id(dir: &Path) -> Option<DirId> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dir).ok()?;
    Some((m.dev(), m.ino() as u128))
}

#[cfg(windows)]
fn dir_id(dir: &Path) -> Option<DirId> {
    ninja_os::windows::file_id(&ninja_os::windows::open_attributes(dir).ok()?)
}

#[cfg(not(any(unix, windows)))]
fn dir_id(_dir: &Path) -> Option<DirId> {
    None
}

/// Directories builds named (keys, with their paths), and when one last
/// did. A path is in one when one of its ancestors is: a lookup per
/// ancestor, however many there are (CMake's `try_compile` names hundreds
/// a day).
#[derive(Default)]
pub(crate) struct BuildDirs(RwLock<HashMap<PathBuf, (PathBuf, Instant)>>);

impl BuildDirs {
    /// Adds `dir` (`key`), or notes it was named again: whether it's new.
    pub(crate) fn insert(&self, key: PathBuf, dir: &Path) -> bool {
        match self.0.write().unwrap().entry(key) {
            Entry::Occupied(mut e) => {
                e.get_mut().1 = Instant::now();
                false
            }
            Entry::Vacant(e) => {
                e.insert((dir.to_path_buf(), Instant::now()));
                true
            }
        }
    }

    /// Whether `key` is in one of them.
    pub(crate) fn contains(&self, key: &Path) -> bool {
        let dirs = self.0.read().unwrap();
        !dirs.is_empty() && key.ancestors().any(|a| dirs.contains_key(a))
    }

    /// Forgets those no build named since `before`: their paths.
    pub(crate) fn expire(&self, before: Instant) -> Vec<PathBuf> {
        let mut gone = Vec::new();
        self.0.write().unwrap().retain(|_, (dir, named)| {
            let keep = *named >= before;
            if !keep {
                gone.push(dir.clone());
            }
            keep
        });
        gone
    }
}

/// How long a root whose watches ran out stays checked once per build.
const RETRY_AFTER_ENOSPC: Duration = Duration::from_secs(10 * 60);

/// How long a root's cookie may take to come back.
const COOKIE_TIMEOUT: Duration = Duration::from_secs(2);

trait Backend: Send + Sync {
    /// Starts watching `root`: everything under it, or for a `lazy`
    /// backend and not `whole`, the root and its cookie directory alone.
    fn watch(&self, root: &Path, cookie_dir: &Path, whole: bool) -> io::Result<()>;
    /// Watches one directory more, with everything under it when `whole`
    /// (`lazy` backends). Err for one in a build directory: not watched.
    fn watch_dir(&self, _dir: &Path, _whole: bool) -> io::Result<()> {
        Ok(())
    }
    /// Stops watching `dir` and what is under it.
    fn unwatch(&self, _dir: &Path) {}
    /// Watches directories as they're needed (`watch_dir`).
    fn lazy(&self) -> bool {
        false
    }
    /// `dir` became a build directory: its events aren't needed.
    fn ignore(&self, _dir: &Path) {}
    /// `dir` no longer is: its events are needed again, and it's watched
    /// if under a directory watched whole.
    fn unignore(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
}

pub struct Watcher {
    me: Weak<Watcher>,
    case_insensitive: bool,
    /// Bumped by every event that matters and every loss: a check that saw
    /// it change may have read a file mid-change.
    seq: AtomicU64,
    events: AtomicU64,
    roots: RwLock<Vec<Root>>,
    /// Build directories, and the others builds write to: the one set the
    /// daemon has (`Files` and the preprocessor's cache ask it).
    ignored: BuildDirs,
    /// Where cookies go for roots without a repository directory, with a
    /// watch per directory (`set_cookie_dir`).
    cookie_dir: OnceLock<PathBuf>,
    /// Directories above vouched-for paths (keys), whose renaming or
    /// removal drops everything under them.
    known_dirs: RwLock<HashSet<PathBuf>>,
    /// Directories (keys) and the repository each is in.
    repos: RwLock<HashMap<PathBuf, Option<PathBuf>>>,
    /// Directories (keys) and whether their path is their real one.
    real_dirs: RwLock<HashMap<PathBuf, bool>>,
    /// The directories (keys) `repos`, `real_dirs` and `watched` have
    /// entries for, and those above them: an event on a path not in it
    /// concerns none of their entries, which it then doesn't go through.
    learned: RwLock<HashSet<PathBuf>>,
    /// Syncs started: a directory watched (lazily) during the `n`th is
    /// vouched for once a later one brought its root in sync.
    epoch: AtomicU64,
    /// Directories watched lazily (keys): the epoch they were, and whether
    /// with everything under them.
    watched: RwLock<HashMap<PathBuf, (u64, bool)>>,
    /// Linux: the file systems mounted (keys), and whether each reports
    /// every change.
    mount_table: RwLock<Vec<(PathBuf, bool)>>,
    listeners: RwLock<Vec<Arc<dyn Listener>>>,
    /// Cookies waited for, and those seen.
    cookies: Mutex<HashMap<String, bool>>,
    cookie_seen: Condvar,
    backend: OnceLock<Option<Box<dyn Backend>>>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

impl Watcher {
    pub fn new(case_insensitive: bool, log: Box<dyn Fn(&str) + Send + Sync>) -> Arc<Watcher> {
        Arc::new_cyclic(|me| Watcher {
            me: me.clone(),
            case_insensitive,
            seq: AtomicU64::new(0),
            events: AtomicU64::new(0),
            roots: RwLock::default(),
            ignored: BuildDirs::default(),
            cookie_dir: OnceLock::new(),
            known_dirs: RwLock::default(),
            repos: RwLock::default(),
            real_dirs: RwLock::default(),
            learned: RwLock::default(),
            epoch: AtomicU64::new(0),
            watched: RwLock::default(),
            mount_table: RwLock::default(),
            listeners: RwLock::default(),
            cookies: Mutex::default(),
            cookie_seen: Condvar::new(),
            backend: OnceLock::new(),
            log,
        })
    }

    pub fn listen(&self, l: Arc<dyn Listener>) {
        self.listeners.write().unwrap().push(l);
    }

    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Where cookies go for roots without a repository directory, with
    /// inotify: a directory of the daemon's, watched alone (one queue
    /// carries every watch's events, in order). Made on first use; the
    /// cookies a daemon that died left there are removed.
    pub fn set_cookie_dir(&self, dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        let _ = self.cookie_dir.set(dir.to_path_buf());
    }

    /// The roots and their states.
    pub fn roots(&self) -> Vec<(PathBuf, RootState)> {
        self.roots.read().unwrap().iter().map(|r| (r.path.clone(), r.state.clone())).collect()
    }

    fn key(&self, p: &Path) -> PathBuf {
        key(p, self.case_insensitive)
    }

    fn backend(&self) -> Option<&dyn Backend> {
        self.backend.get_or_init(|| platform::backend(self.me.clone())).as_deref()
    }

    fn lazy(&self) -> bool {
        self.backend().is_some_and(|b| b.lazy())
    }

    /// Directories watched lazily.
    pub fn watched_dirs(&self) -> usize {
        self.watched.read().unwrap().len()
    }

    /// Forgets the lazy watches of `key` and under it.
    fn forget_watched(&self, key: &Path) {
        self.watched.write().unwrap().retain(|k, _| !k.starts_with(key));
    }

    /// Notes that something was learned about the directory `key` (see
    /// `learned`).
    fn learn(&self, key: &Path) {
        if self.learned.read().unwrap().contains(key) {
            return;
        }
        let mut learned = self.learned.write().unwrap();
        for a in key.ancestors() {
            if !learned.insert(a.to_path_buf()) {
                break;
            }
        }
    }

    /// Forgets what was learned about `key` and the directories under it,
    /// and their lazy watches (a directory created there again is watched
    /// again).
    fn forget_learned(&self, key: &Path) {
        if !self.learned.read().unwrap().contains(key) {
            return;
        }
        self.learned.write().unwrap().retain(|k| !k.starts_with(key));
        self.real_dirs.write().unwrap().retain(|k, _| !k.starts_with(key));
        self.repos.write().unwrap().retain(|k, _| !k.starts_with(key));
        self.forget_watched(key);
    }

    /// A build directory, or another one builds write to: its events are
    /// ignored, and its files never vouched for (those that were are
    /// checked again). Roots in it go (watched again once it expires).
    pub fn ignore(&self, dir: &Path) {
        let key = self.key(dir);
        if !self.ignored.insert(key.clone(), dir) {
            return;
        }
        let gone: Vec<Root> = {
            let mut roots = self.roots.write().unwrap();
            let (gone, kept) = std::mem::take(&mut *roots).into_iter().partition(|r| r.key.starts_with(&key));
            *roots = kept;
            gone
        };
        if let Some(b) = self.backend() {
            b.ignore(dir);
            for r in &gone {
                b.unwatch(&r.path);
            }
        }
        self.forget_watched(&key);
        self.drop_under(dir);
    }

    /// Forgets the build directories no build named since `before`: watched
    /// again with the rest (a build names its own when it begins). Nothing
    /// in them was watched; were a root made there meanwhile, it's watched
    /// again from scratch.
    pub fn expire(&self, before: Instant) {
        for dir in self.ignored.expire(before) {
            let key = self.key(&dir);
            self.forget_watched(&key);
            let inside: Vec<PathBuf> = self.roots.read().unwrap().iter().filter(|r| r.key.starts_with(&key)).map(|r| r.key.clone()).collect();
            for rkey in inside {
                self.restart(&rkey, "no longer in a build directory");
            }
            let Some(Err(e)) = self.backend().map(|b| b.unignore(&dir)) else { continue };
            let root = self.roots.read().unwrap().iter().find(|r| key.starts_with(&r.key)).map(|r| r.key.clone());
            match root {
                Some(rkey) if e.kind() == io::ErrorKind::StorageFull => self.release(&rkey, &e),
                Some(_) => self.lost(Some(&dir)),
                None => {}
            }
        }
    }

    /// Whether `key` is in a build directory (or another one builds write
    /// to).
    pub(crate) fn is_ignored(&self, key: &Path) -> bool {
        self.ignored.contains(key)
    }

    /// Watches `dir`, a repository (lazily, see the module's doc) or,
    /// `whole`, everything under it (unless a root has it already, or it's
    /// in a build directory). Roots under it become part of it: their
    /// watches and cookies would overlap its own.
    pub fn add_root(&self, dir: &Path, whole: bool) {
        let key = self.key(dir);
        if self.is_ignored(&key) {
            return;
        }
        let nested: Vec<Root> = {
            let mut roots = self.roots.write().unwrap();
            if roots.iter().any(|r| key.starts_with(&r.key)) {
                return;
            }
            let (nested, kept): (Vec<Root>, Vec<Root>) = std::mem::take(&mut *roots).into_iter().partition(|r| r.key.starts_with(&key));
            *roots = kept;
            roots.push(Root {
                path: dir.to_path_buf(),
                key: key.clone(),
                state: RootState::Starting,
                cookie_dir: dir.to_path_buf(),
                whole,
                synced: 0,
                retry_after: None,
                id: None,
                mounts: Vec::new(),
            });
            nested
        };
        for r in nested {
            if let Some(b) = self.backend() {
                b.unwatch(&r.path);
            }
            self.forget_watched(&r.key);
            self.drop_under(&r.path);
        }
        self.start(dir.to_path_buf(), key, whole);
    }

    /// Where `dir`'s cookies go (see the module's doc).
    fn cookie_dir_of(&self, dir: &Path) -> PathBuf {
        if let Some(vcs) = VCS_DIRS.iter().map(|v| dir.join(v)).find(|d| d.is_dir()) {
            return vcs;
        }
        match self.cookie_dir.get() {
            Some(own) if self.lazy() && std::fs::create_dir_all(own).is_ok() => own.clone(),
            _ => dir.to_path_buf(),
        }
    }

    /// Watches `dir` and everything under it (a toolchain): a root of its
    /// own, or in a repository watched lazily, a whole directory of it. Not
    /// in a build directory, whose files are checked every time.
    pub fn watch_tree(&self, dir: &Path) {
        let key = self.key(dir);
        if self.is_ignored(&key) {
            return;
        }
        let root = self.roots.read().unwrap().iter().find(|r| key.starts_with(&r.key)).map(|r| (r.key.clone(), r.path.clone(), r.whole, r.synced));
        match root {
            Some((rkey, rpath, false, synced)) if self.lazy() => {
                if let Err(e) = self.lazily_watched(&chain(&rpath, dir), true, synced) {
                    self.release(&rkey, &e);
                }
            }
            Some(_) => {}
            None => self.add_root(dir, true),
        }
    }

    /// Sets up a root's watch, in the background: where its cookies go,
    /// what its directory is and what's mounted in it are found again.
    fn start(&self, dir: PathBuf, key: PathBuf, whole: bool) {
        let me = self.me.clone();
        std::thread::spawn(move || {
            let Some(w) = me.upgrade() else { return };
            let t = Instant::now();
            // Watched from before the next sync, and found before it's
            // watched: a directory there since isn't the one watched.
            let epoch = w.epoch.load(Ordering::SeqCst);
            let (cookie_dir, id, mounts) = (w.cookie_dir_of(&dir), dir_id(&dir), w.mounts_in(&key));
            {
                let mut roots = w.roots.write().unwrap();
                let Some(r) = roots.iter_mut().find(|r| r.key == key) else { return };
                (r.cookie_dir, r.id, r.mounts) = (cookie_dir.clone(), id, mounts);
            }
            let (state, out_of_watches) = match (w.backend(), local_file_system(&dir)) {
                (None, _) => (RootState::Failed("file watching is not supported on this system".into()), None),
                (_, Err(why)) => (RootState::Failed(why), None),
                (Some(b), Ok(())) => match b.watch(&dir, &cookie_dir, whole) {
                    Ok(()) => (RootState::Unsynced, None),
                    Err(e) if e.kind() == io::ErrorKind::StorageFull => (RootState::Failed(e.to_string()), Some(e)),
                    Err(e) => (RootState::Failed(e.to_string()), None),
                },
            };
            if state == RootState::Unsynced && w.lazy() && !whole {
                w.learn(&key);
                w.watched.write().unwrap().insert(key.clone(), (epoch, false));
            }
            (w.log)(&format!("watching {} ({:.0?}): {state:?}", dir.display(), t.elapsed()));
            w.set_state(&key, state, |s| *s == RootState::Starting);
            if let Some(e) = out_of_watches {
                w.release(&key, &e);
            }
        });
    }

    /// Watches a root again from scratch (its watch is broken, or of a
    /// directory no longer at its path): what was vouched for under it is
    /// checked again, and vouched for after the next sync.
    fn restart(&self, key: &Path, why: &str) {
        let (path, whole) = {
            let mut roots = self.roots.write().unwrap();
            let Some(r) = roots.iter_mut().find(|r| r.key == key) else { return };
            r.state = RootState::Starting;
            (r.path.clone(), r.whole)
        };
        (self.log)(&format!("{}: {why}: watching it again", path.display()));
        if let Some(b) = self.backend() {
            b.unwatch(&path);
        }
        self.forget_watched(key);
        self.drop_under(&path);
        self.start(path, key.to_path_buf(), whole);
    }

    /// Linux: the file systems mounted in `key` (or on it), and whether
    /// each reports every change.
    fn mounts_in(&self, key: &Path) -> Vec<(PathBuf, bool)> {
        self.mount_table.read().unwrap().iter().filter(|(m, _)| m.starts_with(key)).cloned().collect()
    }

    /// Linux: the file systems mounted changed. A root with one mounted or
    /// unmounted in it is watched again from scratch: a directory mounted
    /// over is hidden from its watch, and so are the files under it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn mounts_changed(&self, table: Vec<(PathBuf, bool)>) {
        *self.mount_table.write().unwrap() = table;
        let roots: Vec<(PathBuf, Vec<(PathBuf, bool)>)> =
            self.roots.read().unwrap().iter().filter(|r| !matches!(r.state, RootState::Failed(_))).map(|r| (r.key.clone(), r.mounts.clone())).collect();
        for (key, mounts) in roots {
            if self.mounts_in(&key) != mounts {
                self.restart(&key, "file systems were mounted or unmounted there");
            }
        }
    }

    /// Whether `chain`'s directories (from a lazily watched root down) are
    /// watched since before the root's last sync (`synced`), the last one
    /// with everything under it when `tree`. Those that aren't are watched
    /// now, vouched for after the next sync. Err: out of watches (a
    /// directory that can't be watched otherwise, gone or in a build
    /// directory, is just not).
    fn lazily_watched(&self, chain: &[PathBuf], tree: bool, synced: u64) -> io::Result<bool> {
        let mut missing: Vec<(PathBuf, bool)> = Vec::new();
        let mut ok = true;
        {
            let watched = self.watched.read().unwrap();
            for (i, d) in chain.iter().enumerate() {
                let whole = tree && i + 1 == chain.len();
                match watched.get(&self.key(d)) {
                    // Everything under it is watched.
                    Some(&(e, true)) => return Ok(ok && e < synced),
                    Some(&(e, false)) if !whole => ok &= e < synced,
                    _ => {
                        missing.push((d.clone(), whole));
                        ok = false;
                    }
                }
            }
        }
        let Some(b) = self.backend() else { return Ok(false) };
        let epoch = self.epoch.load(Ordering::SeqCst);
        for (d, whole) in missing {
            match b.watch_dir(&d, whole) {
                Ok(()) => {
                    let k = self.key(&d);
                    self.learn(&k);
                    self.watched.write().unwrap().insert(k, (epoch, whole));
                }
                Err(e) if e.kind() == io::ErrorKind::StorageFull => return Err(e),
                Err(_) => return Ok(false),
            }
        }
        Ok(ok)
    }

    /// A root ran out of watches: it gives back all of its own (other
    /// programs need them) and its files are checked once per build, for a
    /// while.
    fn release(&self, key: &Path, why: &io::Error) {
        let path = self.roots.read().unwrap().iter().find(|r| r.key == key).map(|r| r.path.clone());
        let Some(path) = path else { return };
        if let Some(b) = self.backend() {
            b.unwatch(&path);
        }
        self.forget_watched(key);
        (self.log)(&format!("watching {}: {why}: watches released, checking its files once per build (raise fs.inotify.max_user_watches)", path.display()));
        {
            let mut roots = self.roots.write().unwrap();
            if let Some(r) = roots.iter_mut().find(|r| r.key == key) {
                r.state = RootState::Failed(format!("no watches left: {why}"));
                r.retry_after = Some(Instant::now() + RETRY_AFTER_ENOSPC);
            }
        }
        self.untrust(key);
    }

    fn set_state(&self, key: &Path, state: RootState, from: impl Fn(&RootState) -> bool) -> bool {
        let mut roots = self.roots.write().unwrap();
        match roots.iter_mut().find(|r| r.key == key) {
            Some(r) if from(&r.state) => {
                r.state = state;
                true
            }
            _ => false,
        }
    }

    /// The repository `dir` is in (cached for every directory on the way;
    /// until the next sync when there's none).
    fn repository(&self, dir: &Path) -> Option<PathBuf> {
        let mut walked = Vec::new();
        let mut found = None;
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let k = self.key(d);
            if let Some(r) = self.repos.read().unwrap().get(&k) {
                found = r.clone();
                break;
            }
            walked.push(k);
            if VCS_DIRS.iter().any(|m| d.join(m).exists()) {
                found = Some(d.to_path_buf());
                break;
            }
            cur = d.parent();
        }
        for k in &walked {
            self.learn(k);
        }
        let mut repos = self.repos.write().unwrap();
        for k in walked {
            repos.insert(k, found.clone());
        }
        found
    }

    /// Whether `path` names itself (no symbolic link in it): if so, whether
    /// it's a directory.
    fn real(&self, path: &Path) -> Option<bool> {
        let dir = path.parent()?;
        let dkey = self.key(dir);
        let cached = self.real_dirs.read().unwrap().get(&dkey).copied();
        let real_dir = cached.unwrap_or_else(|| {
            self.learn(&dkey);
            let real = std::fs::canonicalize(dir).is_ok_and(|c| self.key(&strip_verbatim(c)) == dkey);
            self.real_dirs.write().unwrap().insert(dkey, real);
            real
        });
        if !real_dir {
            return None;
        }
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => None,
            m => Some(m.is_ok_and(|m| m.is_dir())),
        }
    }

    /// Remembers the directories above `key` up to its root's.
    fn remember_dirs(&self, key: &Path, root: &Path) {
        if key.parent().is_some_and(|p| self.known_dirs.read().unwrap().contains(p)) {
            return;
        }
        let mut known = self.known_dirs.write().unwrap();
        let mut cur = key.parent();
        while let Some(d) = cur {
            if !d.starts_with(root) || !known.insert(d.to_path_buf()) {
                break;
            }
            cur = d.parent();
        }
    }

    /// Before a build: restarts the watches that failed or are of a
    /// directory no longer at their root's path, and brings every root in
    /// sync (see the module's doc). Roots the daemon can't write to are
    /// trusted without a cookie: system directories change rarely.
    pub fn sync(&self) {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        // A repository made since a lookup found none is found now.
        self.repos.write().unwrap().retain(|_, r| r.is_some());
        let now = Instant::now();
        let failed: Vec<(PathBuf, PathBuf, bool)> = {
            let mut roots = self.roots.write().unwrap();
            roots
                .iter_mut()
                .filter(|r| matches!(r.state, RootState::Failed(_)) && r.retry_after.is_none_or(|t| now >= t))
                .map(|r| {
                    r.state = RootState::Starting;
                    r.retry_after = None;
                    (r.path.clone(), r.key.clone(), r.whole)
                })
                .collect()
        };
        for (path, key, whole) in failed {
            self.start(path, key, whole);
        }
        // Replaced (a fresh clone where the old one was), or mounted over:
        // no event says so on Windows, nor for a mount.
        let ids: Vec<(PathBuf, PathBuf, DirId)> = self
            .roots
            .read()
            .unwrap()
            .iter()
            .filter(|r| matches!(r.state, RootState::Unsynced | RootState::Trusted))
            .filter_map(|r| Some((r.key.clone(), r.path.clone(), r.id?)))
            .collect();
        for (key, path, id) in ids {
            if dir_id(&path) != Some(id) {
                self.restart(&key, "replaced");
            }
        }
        static N: AtomicU64 = AtomicU64::new(0);
        let roots: Vec<(PathBuf, PathBuf)> = self
            .roots
            .read()
            .unwrap()
            .iter()
            .filter(|r| matches!(r.state, RootState::Unsynced | RootState::Trusted))
            .map(|r| (r.key.clone(), r.cookie_dir.clone()))
            .collect();
        let mut waiting = Vec::new();
        for (key, cookie_dir) in roots {
            let name = format!("{COOKIE_PREFIX}{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            self.cookies.lock().unwrap().insert(name.clone(), false);
            let file = cookie_dir.join(&name);
            match std::fs::File::create(&file) {
                Ok(_) => waiting.push((key, name, file)),
                Err(e) => {
                    self.cookies.lock().unwrap().remove(&name);
                    if matches!(e.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem) {
                        self.synced(&key, epoch);
                    } else {
                        // Its repository's directory went, or the daemon's
                        // own (made again): nothing tells the watch works.
                        self.restart(&key, &format!("making its cookie: {e}"));
                    }
                }
            }
        }
        let deadline = Instant::now() + COOKIE_TIMEOUT;
        for (key, name, file) in waiting {
            let mut cookies = self.cookies.lock().unwrap();
            while cookies.get(&name) == Some(&false) {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                cookies = self.cookie_seen.wait_timeout(cookies, left).unwrap().0;
            }
            let seen = cookies.remove(&name) == Some(true);
            drop(cookies);
            let _ = std::fs::remove_file(&file);
            if seen {
                self.synced(&key, epoch);
            } else {
                // The watch is broken: what it vouched for is checked again.
                self.restart(&key, &format!("no event in {COOKIE_TIMEOUT:?}"));
            }
        }
    }

    /// A root's cookie came back: in sync as of sync `epoch`.
    fn synced(&self, key: &Path, epoch: u64) {
        let mut roots = self.roots.write().unwrap();
        if let Some(r) = roots.iter_mut().find(|r| r.key == key && matches!(r.state, RootState::Unsynced | RootState::Trusted)) {
            r.state = RootState::Trusted;
            r.synced = epoch;
        }
    }

    /// Everything under a root is checked again.
    fn untrust(&self, key: &Path) {
        let path = self.roots.read().unwrap().iter().find(|r| r.key == key).map(|r| r.path.clone());
        if let Some(path) = path {
            self.drop_under(&path);
        }
    }

    /// Everything under `dir` is checked again.
    fn drop_under(&self, dir: &Path) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        for l in self.listeners.read().unwrap().iter() {
            l.changed(dir, Change::Tree);
        }
    }

    /// A backend lost events (of the root `under` is in, or of every
    /// root). With a watch per directory, those of directories replaced
    /// meanwhile went too, unseen: the roots are watched again from
    /// scratch, vouched for after the next sync.
    fn lost(&self, under: Option<&Path>) {
        let ukey = under.map(|p| self.key(p));
        let lazy = self.lazy();
        let hit: Vec<(PathBuf, PathBuf, bool, bool)> = {
            let mut roots = self.roots.write().unwrap();
            roots
                .iter_mut()
                .filter(|r| ukey.as_ref().is_none_or(|k| k.starts_with(&r.key)))
                .map(|r| {
                    let restart = lazy && !matches!(r.state, RootState::Failed(_));
                    if restart {
                        r.state = RootState::Starting;
                    } else if r.state == RootState::Trusted {
                        r.state = RootState::Unsynced;
                    }
                    (r.key.clone(), r.path.clone(), r.whole, restart)
                })
                .collect()
        };
        (self.log)(&format!("events lost ({}): checking files again", under.map_or("every root".to_string(), |r| r.display().to_string())));
        for (key, path, whole, restart) in hit {
            if restart {
                self.forget_watched(&key);
            }
            self.untrust(&key);
            if restart {
                self.start(path, key, whole);
            }
        }
    }

    /// A backend's watch of `dir`, below a root, went (its file system was
    /// unmounted, or it was removed): what was vouched for under it is
    /// checked again, and it's watched again when needed.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn unwatched(&self, dir: &Path) {
        let key = self.key(dir);
        self.forget_watched(&key);
        self.forget_learned(&key);
        self.drop_under(dir);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn is_root(&self, dir: &Path) -> bool {
        let key = self.key(dir);
        self.roots.read().unwrap().iter().any(|r| r.key == key)
    }

    /// A backend's watch of `root` stopped.
    fn failed(&self, root: &Path, why: &str) {
        let key = self.key(root);
        (self.log)(&format!("watching {} stopped: {why}", root.display()));
        self.set_state(&key, RootState::Failed(why.to_string()), |_| true);
        self.untrust(&key);
        self.forget_watched(&key);
    }

    /// Whether an event on `path` may matter (`ReadDirectoryChangesW`
    /// reports everything in a root, its build directories included): not
    /// in a build directory, nor in a repository's own directory (but for
    /// a cookie, or that directory itself).
    #[cfg_attr(not(windows), allow(dead_code))]
    fn wanted(&self, path: &Path) -> bool {
        if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(COOKIE_PREFIX)) {
            return true;
        }
        let key = self.key(path);
        !self.is_ignored(&key) && (!in_repository_dir(&key) || key.file_name().is_some_and(|n| VCS_DIRS.iter().any(|v| n == *v)))
    }

    /// Something under `dir` changed, its name unknown (a short name for a
    /// file gone): everything under it is checked again.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn changed_under(&self, dir: &Path) {
        let key = self.key(dir);
        if self.is_ignored(&key) {
            return;
        }
        self.forget_learned(&key);
        self.drop_under(dir);
    }

    /// A backend saw `path` change: `structural` when it was created,
    /// removed or renamed.
    fn event(&self, path: &Path, structural: bool) {
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with(COOKIE_PREFIX)
        {
            let mut cookies = self.cookies.lock().unwrap();
            if let Some(seen) = cookies.get_mut(name) {
                *seen = true;
                self.cookie_seen.notify_all();
            }
            return;
        }
        let key = self.key(path);
        if self.is_ignored(&key) {
            return;
        }
        if in_repository_dir(&key) {
            // Nothing there is vouched for. A repository's directory made
            // or removed: lookups of the repository beside it change.
            if structural
                && key.file_name().is_some_and(|n| VCS_DIRS.iter().any(|v| n == *v))
                && let Some(parent) = key.parent()
                && self.learned.read().unwrap().contains(parent)
            {
                self.repos.write().unwrap().retain(|k, _| !k.starts_with(parent));
            }
            return;
        }
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.events.fetch_add(1, Ordering::Relaxed);
        let listeners = self.listeners.read().unwrap().clone();
        for l in &listeners {
            l.changed(path, Change::Content);
        }
        if !structural {
            return;
        }
        if let Some(parent) = path.parent() {
            for l in &listeners {
                l.changed(parent, Change::Names);
            }
        }
        if self.known_dirs.read().unwrap().contains(&key) {
            for l in &listeners {
                l.changed(path, Change::Tree);
            }
        }
        // What was learned about directories there: a file's events (most
        // of them) concern nothing of it.
        self.forget_learned(&key);
    }
}

impl Watcher {
    /// A token for checking `path`, a file or a directory's names (or,
    /// `tree`, a directory and everything under it): `Some` when it's
    /// vouched for.
    fn vouch(&self, path: &Path, tree: bool) -> Option<u64> {
        let seq = self.seq.load(Ordering::SeqCst);
        let key = self.key(path);
        // Their events are ignored.
        if self.is_ignored(&key) || in_repository_dir(&key) {
            return None;
        }
        let root = {
            let roots = self.roots.read().unwrap();
            roots.iter().find(|r| key.starts_with(&r.key)).map(|r| {
                let remote = r.mounts.iter().any(|(m, local)| !local && key.starts_with(m));
                (r.key.clone(), r.path.clone(), r.state.clone(), r.whole, r.synced, remote)
            })
        };
        match root {
            // On a file system that may not report every change.
            Some((.., true)) => None,
            Some((rkey, rpath, state @ (RootState::Trusted | RootState::Unsynced), whole, synced, false)) => {
                let is_dir = self.real(path)?;
                // Lazily watched: the directories down to the file's, or
                // to the directory (its names come with its own events;
                // whole when asked for).
                if self.lazy() && !whole {
                    let last = if tree || is_dir { Some(path) } else { path.parent() };
                    match last.map(|d| self.lazily_watched(&chain(&rpath, d), tree, synced)) {
                        Some(Ok(true)) => {}
                        Some(Ok(false)) | None => return None,
                        Some(Err(e)) => {
                            self.release(&rkey, &e);
                            return None;
                        }
                    }
                }
                if state != RootState::Trusted {
                    return None;
                }
                self.remember_dirs(&key, &rkey);
                Some(seq)
            }
            Some(_) => None,
            None => {
                // Watched from now on: vouched for after the next sync.
                if let Some(repo) = path.parent().and_then(|d| self.repository(d)) {
                    self.add_root(&repo, false);
                }
                None
            }
        }
    }

    /// A token for checking the directory `dir` and everything under it
    /// (see `watch_tree`).
    pub fn tree_token(&self, dir: &Path) -> Option<u64> {
        self.vouch(dir, true)
    }
}

/// The directories from `root` down to `dir`, both included.
fn chain(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = dir.ancestors().take_while(|d| d.starts_with(root)).map(Path::to_path_buf).collect();
    dirs.reverse();
    dirs
}

impl cpp_deps::scan::Watch for Watcher {
    fn token(&self, path: &Path) -> Option<u64> {
        self.vouch(path, false)
    }

    fn unchanged_since(&self, token: u64) -> bool {
        self.seq.load(Ordering::SeqCst) == token
    }

    fn volatile(&self, path: &Path) -> bool {
        self.is_ignored(&self.key(path))
    }
}

/// `p` as a key: lowercase on case-insensitive file systems.
pub fn key(p: &Path, case_insensitive: bool) -> PathBuf {
    if case_insensitive { PathBuf::from(p.to_string_lossy().to_lowercase()) } else { p.to_path_buf() }
}

/// A canonical path without Windows' `\\?\` prefix.
pub(crate) fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(target_os = "linux")]
mod platform {
    //! inotify: one watch per directory, added recursively; directories
    //! created later are watched as they appear.

    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    pub(super) fn backend(sink: Weak<Watcher>) -> Option<Box<dyn Backend>> {
        // SAFETY: plain syscall.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // Read before roots are watched; changed since it was opened, it
        // says so (`poll`).
        if let (Ok(mut table), Some(w)) = (std::fs::File::open("/proc/self/mountinfo"), sink.upgrade()) {
            *w.mount_table.write().unwrap() = read_mounts(&mut table, w.case_insensitive);
            let sink = sink.clone();
            std::thread::spawn(move || watch_mounts(table, sink));
        }
        let inotify = Arc::new(Inotify { fd, wds: Mutex::default(), whole: RwLock::default(), sink });
        let reader = inotify.clone();
        std::thread::spawn(move || reader.read_loop());
        Some(Box::new(Shared(inotify)))
    }

    /// The file systems `/proc/self/mountinfo` lists (keys), and whether
    /// each reports every change.
    fn read_mounts(file: &mut std::fs::File, case_insensitive: bool) -> Vec<(PathBuf, bool)> {
        use std::io::{Read, Seek};
        let mut text = String::new();
        if file.rewind().and_then(|()| file.read_to_string(&mut text)).is_err() {
            return Vec::new();
        }
        parse_mounts(&text).into_iter().map(|(p, local)| (key(&p, case_insensitive), local)).collect()
    }

    /// The mount points of a `mountinfo` (spaces and the like escaped in
    /// octal), and whether their file system reports every change (those
    /// `local_file_system` accepts).
    pub(super) fn parse_mounts(text: &str) -> Vec<(PathBuf, bool)> {
        const LOCAL: &[&str] = &["ext2", "ext3", "ext4", "xfs", "btrfs", "tmpfs", "f2fs", "zfs", "bcachefs", "overlay", "reiserfs", "jfs", "nilfs2"];
        let unescape = |s: &str| {
            let b = s.as_bytes();
            let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
            while i < b.len() {
                match (b[i], b.get(i + 1..i + 4).and_then(|o| u8::from_str_radix(std::str::from_utf8(o).ok()?, 8).ok())) {
                    (b'\\', Some(c)) => {
                        out.push(c);
                        i += 4;
                    }
                    (c, _) => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            PathBuf::from(std::ffi::OsString::from_vec(out))
        };
        text.lines()
            .filter_map(|line| {
                let (fields, rest) = line.split_once(" - ")?;
                let point = fields.split(' ').nth(4)?;
                let kind = rest.split(' ').next()?;
                Some((unescape(point), LOCAL.contains(&kind)))
            })
            .collect()
    }

    /// Tells the watcher when file systems are mounted or unmounted.
    fn watch_mounts(mut table: std::fs::File, sink: Weak<Watcher>) {
        use std::os::fd::AsRawFd;
        loop {
            let mut pfd = libc::pollfd { fd: table.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
            // SAFETY: one valid pollfd. Every while without a change:
            // whether the watcher is still there.
            let n = unsafe { libc::poll(&mut pfd, 1, 10_000) };
            let Some(w) = sink.upgrade() else { return };
            if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return;
            }
            if n > 0 && pfd.revents & (libc::POLLPRI | libc::POLLERR) != 0 {
                w.mounts_changed(read_mounts(&mut table, w.case_insensitive));
            }
        }
    }

    struct Shared(Arc<Inotify>);

    impl Backend for Shared {
        fn watch(&self, root: &Path, cookie_dir: &Path, whole: bool) -> io::Result<()> {
            if whole {
                self.watch_dir(root, true)?;
            } else {
                self.0.add_one(root)?;
            }
            // Where its cookies go, unless watched with it.
            if cookie_dir != root && !(whole && cookie_dir.starts_with(root)) {
                self.0.add_one(cookie_dir)?;
            }
            Ok(())
        }

        fn watch_dir(&self, dir: &Path, whole: bool) -> io::Result<()> {
            if !whole {
                return self.0.add_one(dir);
            }
            self.0.whole.write().unwrap().insert(dir.to_path_buf());
            self.0.add_tree(dir).inspect_err(|_| {
                self.0.whole.write().unwrap().remove(dir);
            })
        }

        fn unwatch(&self, dir: &Path) {
            self.0.whole.write().unwrap().retain(|w| !w.starts_with(dir));
            self.0.remove_tree(dir);
        }

        fn lazy(&self) -> bool {
            true
        }

        fn ignore(&self, dir: &Path) {
            self.unwatch(dir);
        }

        fn unignore(&self, dir: &Path) -> io::Result<()> {
            // Still in another one, or watched when needed.
            if self.0.is_ignored(dir) || !under(&self.0.whole, dir) {
                return Ok(());
            }
            match self.0.add_tree(dir) {
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => Ok(()),
                r => r,
            }
        }
    }

    /// Whether `dir` is in one of `dirs`.
    fn under(dirs: &RwLock<HashSet<PathBuf>>, dir: &Path) -> bool {
        let dirs = dirs.read().unwrap();
        !dirs.is_empty() && dir.ancestors().any(|a| dirs.contains(a))
    }

    struct Inotify {
        fd: i32,
        /// Watch descriptors and their directories.
        wds: Mutex<HashMap<i32, PathBuf>>,
        /// Directories watched with everything under them: directories
        /// created there are watched as they appear.
        whole: RwLock<HashSet<PathBuf>>,
        sink: Weak<Watcher>,
    }

    const MASK: u32 = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MODIFY
        | libc::IN_CLOSE_WRITE
        | libc::IN_ATTRIB
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR
        | libc::IN_DONT_FOLLOW
        | libc::IN_EXCL_UNLINK;

    const STRUCTURAL: u32 = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;

    /// Why a directory in a build directory isn't watched.
    fn build_dir(dir: &Path) -> io::Error {
        io::Error::other(format!("{} is in a build directory", dir.display()))
    }

    impl Inotify {
        /// Whether `dir` is in a build directory (see `Watcher::ignore`).
        fn is_ignored(&self, dir: &Path) -> bool {
            self.sink.upgrade().is_some_and(|w| w.is_ignored(&w.key(dir)))
        }

        /// Watches `dir` alone (not in a build directory).
        fn add_one(&self, dir: &Path) -> io::Result<()> {
            if self.is_ignored(dir) {
                return Err(build_dir(dir));
            }
            let c = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
            // SAFETY: `c` is a valid C string.
            let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
            if wd < 0 {
                let e = io::Error::last_os_error();
                return Err(io::Error::new(e.kind(), format!("watching {}: {e}", dir.display())));
            }
            self.wds.lock().unwrap().insert(wd, dir.to_path_buf());
            Ok(())
        }

        /// Watches `dir` and the directories under it (not into
        /// repositories' own directories, nor build directories, whose
        /// files aren't vouched for). Out of watches, gives back those it
        /// added; `dir` itself in a build directory isn't watched.
        fn add_tree(&self, dir: &Path) -> io::Result<()> {
            if self.is_ignored(dir) {
                return Err(build_dir(dir));
            }
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                if d != dir && self.is_ignored(&d) {
                    continue;
                }
                let c = CString::new(d.as_os_str().as_bytes()).map_err(io::Error::other)?;
                // SAFETY: `c` is a valid C string.
                let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
                if wd < 0 {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::ENOSPC) {
                        self.remove_tree(dir);
                    }
                    if d == dir || e.raw_os_error() == Some(libc::ENOSPC) {
                        return Err(io::Error::new(e.kind(), format!("watching {}: {e}", d.display())));
                    }
                    // Gone meanwhile.
                    continue;
                }
                self.wds.lock().unwrap().insert(wd, d.clone());
                let in_vcs = d.file_name().is_some_and(|n| VCS_DIRS.iter().any(|v| n == *v));
                if in_vcs {
                    continue;
                }
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        stack.push(e.path());
                    }
                }
            }
            Ok(())
        }

        fn remove_tree(&self, dir: &Path) {
            let mut wds = self.wds.lock().unwrap();
            wds.retain(|wd, d| {
                if d.starts_with(dir) {
                    // SAFETY: plain syscall.
                    unsafe { libc::inotify_rm_watch(self.fd, *wd) };
                    false
                } else {
                    true
                }
            });
        }

        fn read_loop(&self) {
            let mut buf = vec![0u8; 1 << 16];
            loop {
                // SAFETY: `buf` is writable for its length.
                let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
                let Some(sink) = self.sink.upgrade() else { return };
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    sink.lost(None);
                    return;
                }
                let mut off = 0usize;
                let header = std::mem::size_of::<libc::inotify_event>();
                while off + header <= n as usize {
                    // SAFETY: the kernel wrote a whole event at `off`.
                    let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off).cast()) };
                    let name_bytes = &buf[off + header..off + header + ev.len as usize];
                    let name = &name_bytes[..name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len())];
                    off += header + ev.len as usize;
                    self.handle(&sink, &ev, name);
                }
            }
        }

        fn handle(&self, sink: &Watcher, ev: &libc::inotify_event, name: &[u8]) {
            if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                sink.lost(None);
                return;
            }
            let Some(dir) = self.wds.lock().unwrap().get(&ev.wd).cloned() else { return };
            if ev.mask & (libc::IN_IGNORED | libc::IN_UNMOUNT) != 0 {
                // The watch went (`remove_tree` forgets those it removes
                // first): the directory was removed, which its parent
                // reported, or its file system unmounted, which nothing did.
                self.remove_tree(&dir);
                if sink.is_root(&dir) {
                    sink.failed(&dir, "the directory is no longer watched");
                } else {
                    sink.unwatched(&dir);
                }
                return;
            }
            let path = if name.is_empty() { dir.clone() } else { dir.join(std::ffi::OsStr::from_bytes(name)) };
            let is_dir = ev.mask & libc::IN_ISDIR != 0;
            if is_dir && ev.mask & libc::IN_MOVED_FROM != 0 {
                // Its watches would report the old paths.
                self.whole.write().unwrap().retain(|w| !w.starts_with(&path));
                self.remove_tree(&path);
            }
            // Under a directory watched whole, new directories are watched
            // (elsewhere, when needed; never build directories). One gone
            // already (or a file now) has its own events; one that can't be
            // watched is a loss.
            if is_dir && ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 && under(&self.whole, &path) && !self.is_ignored(&path) {
                match self.add_tree(&path) {
                    Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {}
                    Err(_) => sink.lost(Some(&path)),
                    Ok(()) => {}
                }
            }
            if name.is_empty() && ev.mask & (libc::IN_MOVE_SELF | libc::IN_DELETE_SELF) != 0 {
                // A directory below a root: its parent reported it. A root:
                // its watches would report wrong paths.
                if sink.is_root(&dir) {
                    self.remove_tree(&dir);
                    sink.failed(&dir, "the directory was moved or removed");
                }
                return;
            }
            sink.event(&path, ev.mask & STRUCTURAL != 0);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use cpp_deps::scan::Watch;

        #[derive(Default)]
        struct Recorder(Mutex<Vec<(PathBuf, Change)>>);
        impl Listener for Recorder {
            fn changed(&self, path: &Path, change: Change) {
                self.0.lock().unwrap().push((path.to_path_buf(), change));
            }
        }

        /// A watch gone without its parent saying (its file system was
        /// unmounted): what was vouched for under it is checked again, and
        /// the directory watched again when needed; a root's stops.
        #[test]
        fn watches_gone() {
            let dir = test_dirs::base().join(format!("ninja_daemon_gone_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            std::fs::create_dir_all(dir.join("mnt/sub")).unwrap();
            let dir = std::fs::canonicalize(&dir).unwrap();
            let f = dir.join("mnt/sub/f.h");
            std::fs::write(&f, "f").unwrap();
            let w = Watcher::new(false, Box::new(|_| {}));
            let rec = Arc::new(Recorder::default());
            w.listen(rec.clone());
            assert_eq!(w.token(&f), None);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !w.roots().iter().any(|(_, s)| *s == RootState::Unsynced) {
                assert!(Instant::now() < deadline, "timed out waiting for the watch");
                std::thread::sleep(Duration::from_millis(10));
            }
            w.token(&f);
            w.sync();
            assert!(w.token(&f).is_some());
            // The events, as the backend gets them.
            let backend = Inotify {
                fd: -1,
                wds: Mutex::new(HashMap::from([(1, dir.join("mnt")), (2, dir.clone())])),
                whole: Default::default(),
                sink: Arc::downgrade(&w),
            };
            let event = |wd, mask| libc::inotify_event { wd, mask, cookie: 0, len: 0 };
            backend.handle(&w, &event(1, libc::IN_UNMOUNT), b"");
            assert!(rec.0.lock().unwrap().iter().any(|(p, c)| *p == dir.join("mnt") && *c == Change::Tree));
            assert_eq!(w.token(&f), None);
            w.sync();
            assert!(w.token(&f).is_some());
            backend.handle(&w, &event(2, libc::IN_IGNORED), b"");
            assert!(matches!(w.roots()[0].1, RootState::Failed(_)), "{:?}", w.roots());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Mount points, unescaped, and whether their file systems report
        /// every change.
        #[test]
        fn mount_tables() {
            let text = "22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n\
                        40 22 0:35 / /mnt/c rw,noatime - 9p drvfs rw\n\
                        41 22 0:36 / /home/u/my\\040repo/sub rw - tmpfs none rw\n";
            let expected = [("/", true), ("/mnt/c", false), ("/home/u/my repo/sub", true)].map(|(p, l)| (PathBuf::from(p), l));
            assert_eq!(parse_mounts(text), expected);
        }

        /// A file system mounted in a root (over a watched directory, which
        /// hides it from its watch): the root is watched again from
        /// scratch. Files on one that may not report every change aren't
        /// vouched for.
        #[test]
        fn mounts_in_roots() {
            let dir = std::env::temp_dir().join(format!("ninja_daemon_mounts_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for d in [".git", "a", "net"] {
                std::fs::create_dir_all(dir.join(d)).unwrap();
            }
            let dir = std::fs::canonicalize(&dir).unwrap();
            let (f, g) = (dir.join("a/f.h"), dir.join("net/g.h"));
            std::fs::write(&f, "f").unwrap();
            std::fs::write(&g, "g").unwrap();
            let w = Watcher::new(false, Box::new(|_| {}));
            let in_sync = || {
                w.token(&f);
                w.token(&g);
                w.sync();
                w.token(&f).is_some()
            };
            assert_eq!(w.token(&f), None);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !in_sync() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(w.token(&g).is_some());
            let mut table = w.mount_table.read().unwrap().clone();
            table.push((dir.join("a"), true));
            w.mounts_changed(table.clone());
            assert_eq!(w.token(&f), None);
            while !in_sync() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            table.push((dir.join("net"), false));
            w.mounts_changed(table);
            while !in_sync() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(w.token(&g), None);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(windows)]
mod platform {
    //! `ReadDirectoryChangesW` on each root (recursive), each read by its
    //! own thread. A zero-length result or `ERROR_NOTIFY_ENUM_DIR` means
    //! the buffer overflowed: events were lost.

    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::atomic::AtomicBool;

    use ninja_os::windows::wide;
    use windows_sys::Win32::Foundation::{ERROR_NOTIFY_ENUM_DIR, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ACTION_MODIFIED, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetLongPathNameW, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::CancelIoEx;

    const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION
        | FILE_NOTIFY_CHANGE_SECURITY;

    pub(super) fn backend(sink: Weak<Watcher>) -> Option<Box<dyn Backend>> {
        Some(Box::new(Directories { sink, active: Arc::default() }))
    }

    struct Directories {
        sink: Weak<Watcher>,
        /// Roots with a running reader.
        active: Arc<Mutex<HashMap<PathBuf, Reader>>>,
    }

    struct Reader {
        stop: Arc<AtomicBool>,
        /// The directory's, shared with the reader: open while either has
        /// it.
        handle: Arc<OwnedHandle>,
    }

    impl Reader {
        /// Stops the reader: its read is cancelled.
        fn stop(&self) {
            self.stop.store(true, Ordering::SeqCst);
            // SAFETY: an open handle (this reference keeps it open).
            unsafe { CancelIoEx(self.handle.as_raw_handle(), std::ptr::null()) };
        }
    }

    impl Backend for Directories {
        /// A reader already there reads what was at `root` then (a root
        /// replaced since by a fresh clone, say): it's stopped, and the
        /// directory there now opened.
        fn watch(&self, root: &Path, _cookie_dir: &Path, _whole: bool) -> io::Result<()> {
            if let Some(old) = self.active.lock().unwrap().remove(root) {
                old.stop();
            }
            let wide = wide(root);
            // SAFETY: `wide` is NUL-terminated.
            let h = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a new handle, owned from now on.
            let handle = Arc::new(unsafe { OwnedHandle::from_raw_handle(h) });
            // The first read sets up the watch: events from then on are
            // kept between reads (in a buffer of the first read's size).
            let (tx, rx) = std::sync::mpsc::channel();
            let root = root.to_path_buf();
            let sink = self.sink.clone();
            let stop = Arc::new(AtomicBool::new(false));
            self.active.lock().unwrap().insert(root.clone(), Reader { stop: stop.clone(), handle: handle.clone() });
            let active = self.active.clone();
            std::thread::spawn(move || {
                read_loop(&handle, &root, sink, tx, &stop);
                let mut active = active.lock().unwrap();
                if active.get(&root).is_some_and(|r| Arc::ptr_eq(&r.stop, &stop)) {
                    active.remove(&root);
                }
            });
            // The thread is about to read; let it start (the next sync
            // checks that it did).
            let _ = rx.recv_timeout(Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        }

        fn unwatch(&self, dir: &Path) {
            self.active.lock().unwrap().retain(|root, reader| {
                let under = root.starts_with(dir);
                if under {
                    reader.stop();
                }
                !under
            });
        }
    }

    fn read_loop(handle: &OwnedHandle, root: &Path, sink: Weak<Watcher>, started: std::sync::mpsc::Sender<()>, stop: &AtomicBool) {
        let mut buf = vec![0u32; 1 << 18];
        let _ = started.send(());
        // Stopped between reads, or during one (cancelled).
        while !stop.load(Ordering::SeqCst) {
            let mut n = 0u32;
            // SAFETY: `buf` is u32-aligned and writable for its byte length.
            let ok = unsafe {
                ReadDirectoryChangesW(handle.as_raw_handle(), buf.as_mut_ptr().cast(), (buf.len() * 4) as u32, 1, FILTER, &mut n, std::ptr::null_mut(), None)
            };
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let Some(w) = sink.upgrade() else { return };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                    w.lost(Some(root));
                    continue;
                }
                w.failed(root, &e.to_string());
                return;
            }
            if n == 0 {
                w.lost(Some(root));
                continue;
            }
            // SAFETY: the call filled the first `n` bytes of `buf` (at most
            // its byte length), which isn't changed while `bytes` lives.
            let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), n as usize) };
            let mut off = 0usize;
            loop {
                let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
                let (next, action, len) = (u32_at(off), u32_at(off + 4), u32_at(off + 8) as usize);
                let name: Vec<u16> = bytes[off + 12..off + 12 + len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let path = root.join(std::ffi::OsString::from_wide(&name));
                match long_path(&path) {
                    // Build directories' (most of a build's events) go
                    // first, before any stat: a burst of them would
                    // overflow the buffer.
                    Some(Named::Path(p)) if !w.wanted(&p) => {}
                    // A directory's timestamps follow its files', whose
                    // own events tell what changed.
                    Some(Named::Path(p)) if action == FILE_ACTION_MODIFIED && p.is_dir() => {}
                    Some(Named::Path(p)) => w.event(&p, action != FILE_ACTION_MODIFIED),
                    Some(Named::Under(d)) => w.changed_under(&d),
                    None => w.lost(Some(root)),
                }
                if next == 0 {
                    break;
                }
                off += next as usize;
            }
        }
    }

    /// What an event's path names.
    #[derive(Debug, PartialEq)]
    enum Named {
        /// That path, with long names.
        Path(PathBuf),
        /// Something under that directory, whose name is unknown.
        Under(PathBuf),
    }

    /// Whether `name` has the shape of a short (8.3) name: a base of up to
    /// 8 characters with `~` and digits in it, and an extension of up to 3
    /// (not `x.c.~1~`, a backup file).
    fn short(name: &std::ffi::OsStr) -> bool {
        let s = name.to_string_lossy();
        let (base, ext) = s.split_once('.').unwrap_or((&s, ""));
        base.len() <= 8 && ext.len() <= 3 && !ext.contains('.') && base.find('~').is_some_and(|i| base[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
    }

    /// `path` with long names: events may use a short name. One that can't
    /// be expanded (the file is gone) names something under the deepest
    /// directory that can; None when none can (the root went).
    fn long_path(path: &Path) -> Option<Named> {
        if !path.components().any(|c| short(c.as_os_str())) {
            return Some(Named::Path(path.to_path_buf()));
        }
        let expand = |p: &Path| -> Option<PathBuf> {
            let wide = wide(p);
            let mut out = vec![0u16; 32768];
            // SAFETY: both buffers are valid; `out` is writable for its length.
            let n = unsafe { GetLongPathNameW(wide.as_ptr(), out.as_mut_ptr(), out.len() as u32) } as usize;
            (n > 0 && n < out.len()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&out[..n])))
        };
        if let Some(p) = expand(path) {
            return Some(Named::Path(p));
        }
        // Gone: its directory, if only the directory has short names.
        let (dir, name) = (path.parent()?, path.file_name()?);
        if !short(name)
            && let Some(d) = expand(dir)
        {
            return Some(Named::Path(d.join(name)));
        }
        dir.ancestors().find_map(expand).map(Named::Under)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Events' short names are expanded; a name merely like one (a
        /// backup file's) is kept as is, though gone; a short name gone
        /// names something under its directory.
        #[test]
        fn long_names() {
            let dir = std::env::temp_dir().join(format!("ninja_daemon_short_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
            let backup = dir.join("x.c.~1~");
            assert_eq!(long_path(&backup), Some(Named::Path(backup.clone())));
            assert_eq!(long_path(&dir.join("ABCDEF~1.H")), Some(Named::Under(dir.clone())));
            assert_eq!(long_path(&dir.join("GONE~1").join("x.h")), Some(Named::Under(dir.clone())));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;

    pub(super) fn backend(_sink: Weak<Watcher>) -> Option<Box<dyn Backend>> {
        None
    }
}

/// Checks that `dir`'s file system reports every change: a network or
/// virtual one (NFS, SMB, WSL's view of Windows drives, a VM's shared
/// folders) only reports this machine's, so the daemon would vouch for
/// files others changed. Such roots are checked once per build instead.
#[cfg(target_os = "linux")]
fn local_file_system(dir: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    // ext2-4, XFS, Btrfs, tmpfs, F2FS, ZFS, bcachefs, overlayfs,
    // ReiserFS, JFS, NILFS.
    const LOCAL: &[i64] =
        &[0xEF53, 0x5846_5342, 0x9123_683E, 0x0102_1994, 0xF2F5_2010, 0x2FC1_2FC1, 0xCA45_1A4E, 0x794C_7630, 0x5265_4973, 0x3153_464A, 0x3434];
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: `statfs` is plain data, filled by the call.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a valid buffer.
    if unsafe { libc::statfs(path.as_ptr(), &mut st) } != 0 {
        return Err(format!("statfs {}: {}", dir.display(), io::Error::last_os_error()));
    }
    let kind = st.f_type as i64;
    if LOCAL.contains(&kind) { Ok(()) } else { Err(format!("{} is on a file system (type {kind:#x}) that may not report every change", dir.display())) }
}

#[cfg(windows)]
fn local_file_system(dir: &Path) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;
    let path = ninja_os::windows::wide(dir);
    let mut volume = vec![0u16; 1024];
    // SAFETY: a NUL-terminated path and a buffer of the given length.
    if unsafe { GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0 {
        return Err(format!("the volume of {}: {}", dir.display(), io::Error::last_os_error()));
    }
    // SAFETY: a NUL-terminated volume path.
    if unsafe { GetDriveTypeW(volume.as_ptr()) } == DRIVE_REMOTE {
        return Err(format!("{} is on a network drive, which may not report every change", dir.display()));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn local_file_system(_dir: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpp_deps::scan::Watch;

    #[test]
    fn local_file_systems() {
        assert_eq!(local_file_system(&std::env::temp_dir()), Ok(()));
        #[cfg(target_os = "linux")]
        if std::path::Path::new("/proc/self").exists() {
            assert!(local_file_system(Path::new("/proc")).is_err());
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(PathBuf, Change)>>);
    impl Listener for Recorder {
        fn changed(&self, path: &Path, change: Change) {
            self.0.lock().unwrap().push((path.to_path_buf(), change));
        }
    }

    fn wait_for(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A repository's files are vouched for once its root is in sync, and
    /// their changes reported.
    #[test]
    fn watches_repositories() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let dir = test_dirs::base().join(format!("ninja_daemon_watch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("inc/sub")).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let a = dir.join("inc/sub/a.h");
        std::fs::write(&a, "x").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        w.ignore(&dir.join("build"));
        // Discovered, not vouched for yet.
        assert_eq!(w.token(&a), None);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        assert_eq!(w.token(&a), None);
        w.sync();
        assert_eq!(w.roots(), vec![(dir.clone(), RootState::Trusted)]);
        // Checked under the watch.
        let t = w.token(&a).expect("vouched for");
        std::thread::sleep(Duration::from_millis(100));
        assert!(w.unchanged_since(t), "{:?}", rec.0.lock().unwrap());
        // A change: reported, and checks that saw it aren't vouched for.
        std::fs::write(&a, "yy").unwrap();
        wait_for("the change", || rec.0.lock().unwrap().iter().any(|(p, c)| *p == a && *c == Change::Content));
        assert!(!w.unchanged_since(t));
        // Build directories and repositories' own files are ignored.
        rec.0.lock().unwrap().clear();
        std::fs::write(dir.join("build/x.o"), "o").unwrap();
        std::fs::write(dir.join(".git/index"), "i").unwrap();
        std::fs::write(dir.join("inc/b.h"), "b").unwrap();
        wait_for("b.h", || rec.0.lock().unwrap().iter().any(|(p, _)| p.ends_with("b.h")));
        assert!(!rec.0.lock().unwrap().iter().any(|(p, _)| p.ends_with("x.o") || p.ends_with("index")), "{:?}", rec.0.lock().unwrap());
        // A new file: its directory's names changed.
        assert!(rec.0.lock().unwrap().iter().any(|(p, c)| *p == dir.join("inc") && *c == Change::Names));
        // A directory above vouched-for files renamed: everything under it.
        rec.0.lock().unwrap().clear();
        std::fs::rename(dir.join("inc/sub"), dir.join("inc/moved")).unwrap();
        wait_for("the rename", || rec.0.lock().unwrap().iter().any(|(p, c)| *p == dir.join("inc/sub") && *c == Change::Tree));
        // Files in the moved directory are vouched for at their new paths
        // (after a sync, when watched lazily), and their changes reported.
        let moved = dir.join("inc/moved/a.h");
        w.token(&moved);
        w.sync();
        assert_eq!(w.roots(), vec![(dir.clone(), RootState::Trusted)]);
        assert!(w.token(&moved).is_some());
        rec.0.lock().unwrap().clear();
        std::fs::write(&moved, "zzz").unwrap();
        wait_for("the moved file", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == moved));
        // Paths through symbolic links aren't vouched for.
        let outside = test_dirs::base().join(format!("ninja_daemon_watch_out_{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
            assert_eq!(w.token(&dir.join("link/c.h")), None);
        }
        assert!(w.token(&dir.join("inc/b.h")).is_some());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// inotify: a repository's directories are watched as files in them
    /// are checked, with those above them, not the rest; a directory asked
    /// for whole (a toolchain in the checkout) with everything under it,
    /// directories created there since included.
    #[cfg(target_os = "linux")]
    #[test]
    fn watches_lazily() {
        let dir = test_dirs::base().join(format!("ninja_daemon_lazy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in [".git", "src/a", "other/deep/er", "tc/bin/sub"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let x = dir.join("src/a/x.h");
        std::fs::write(&x, "x").unwrap();
        let w = Watcher::new(false, Box::new(|_| {}));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        assert_eq!(w.token(&x), None);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        // Its directories are watched now: vouched for after a sync.
        assert_eq!(w.token(&x), None);
        w.watch_tree(&dir.join("tc"));
        assert_eq!(w.tree_token(&dir.join("tc")), None);
        w.sync();
        assert!(w.token(&x).is_some());
        assert!(w.tree_token(&dir.join("tc")).is_some());
        // The root, src, src/a and tc (whole); not other/.
        assert_eq!(w.watched_dirs(), 4);
        // Deep in the toolchain, a directory created since is watched too.
        std::fs::create_dir_all(dir.join("tc/bin/sub/new")).unwrap();
        wait_for("the new directory", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == dir.join("tc/bin/sub/new")));
        std::fs::write(dir.join("tc/bin/sub/new/f"), "f").unwrap();
        wait_for("its file", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == dir.join("tc/bin/sub/new/f")));
        // Nothing under other/ is watched.
        rec.0.lock().unwrap().clear();
        std::fs::write(dir.join("other/deep/er/y.h"), "y").unwrap();
        std::fs::write(&x, "xx").unwrap();
        wait_for("x.h", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == x));
        assert!(!rec.0.lock().unwrap().iter().any(|(p, _)| p.starts_with(dir.join("other"))), "{:?}", rec.0.lock().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory searched for headers (listed) is watched itself: a
    /// header created there later is seen, though no file of it was checked
    /// (it would shadow one found further down the search path).
    #[test]
    fn watches_listed_directories() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let dir = test_dirs::base().join(format!("ninja_daemon_listed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in [".git", "inc1", "inc2"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        std::fs::write(dir.join("inc2/foo.h"), "// the old one").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let driver = Arc::new(cpp_deps::Driver::new(cpp_deps::Cache::new(vec![], cfg!(windows))));
        driver.cache.set_watch(w.clone());
        w.listen(driver.clone());
        let shadow = dir.join("inc1/foo.h");
        let build = || {
            w.sync();
            driver.cache.new_session();
            driver.cache.exists(&shadow)
        };
        // Found, then vouched for: inc1 is listed, with nothing in it.
        assert!(!build());
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        assert!(!build());
        assert!(!build());
        std::fs::write(&shadow, "// the new one").unwrap();
        wait_for("the new header", build);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// inotify: events lost (a queue overflow) while a watched directory
    /// was replaced, which took its watch: the root is watched again from
    /// scratch, so the new directory's changes are seen.
    #[cfg(target_os = "linux")]
    #[test]
    fn rewatches_after_lost_events() {
        /// Blocks the reader while `stall` is set.
        #[derive(Default)]
        struct Staller {
            stall: std::sync::atomic::AtomicBool,
            blocked: std::sync::atomic::AtomicBool,
        }
        impl Listener for Staller {
            fn changed(&self, _: &Path, _: Change) {
                while self.stall.load(Ordering::SeqCst) {
                    self.blocked.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let dir = test_dirs::base().join(format!("ninja_daemon_lost_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("d")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let f = dir.join("d/f.h");
        std::fs::write(&f, "v1").unwrap();
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let l2 = lines.clone();
        let w = Watcher::new(false, Box::new(move |m| l2.lock().unwrap().push(m.to_string())));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        let staller = Arc::new(Staller::default());
        w.listen(staller.clone());
        assert_eq!(w.token(&f), None);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        w.token(&f);
        w.sync();
        assert!(w.token(&f).is_some());
        // The reader stalls, the queue overflows, d is replaced meanwhile.
        staller.stall.store(true, Ordering::SeqCst);
        std::fs::write(dir.join("trigger"), "x").unwrap();
        wait_for("the stalled reader", || staller.blocked.load(Ordering::SeqCst));
        let max: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(16384);
        for i in 0..max + 100 {
            std::fs::File::create(dir.join(format!("fill{i}"))).unwrap();
        }
        std::fs::remove_dir_all(dir.join("d")).unwrap();
        std::fs::create_dir(dir.join("d")).unwrap();
        std::fs::write(&f, "v2").unwrap();
        staller.stall.store(false, Ordering::SeqCst);
        wait_for("the loss", || lines.lock().unwrap().iter().any(|l| l.contains("events lost")));
        // Vouched for again once watched again, and in sync.
        assert_eq!(w.token(&f), None);
        wait_for("the root in sync", || {
            w.sync();
            w.token(&f).is_some()
        });
        rec.0.lock().unwrap().clear();
        std::fs::write(&f, "v3, longer").unwrap();
        wait_for("the new directory's change", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == f));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cookies stay out of the trees builds list: in a repository's own
    /// directory, else (inotify) in the daemon's; and a root found inside
    /// another's becomes part of it.
    #[test]
    fn cookie_dirs_and_nested_roots() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let dir = test_dirs::base().join(format!("ninja_daemon_cookies_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["repo/.git", "repo/tc/bin", "tc2/bin"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let (repo, tc2) = (dir.join("repo"), dir.join("tc2"));
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.set_cookie_dir(&dir.join("cookies"));
        // A toolchain in the checkout listed before any of its files: a root
        // of its own until the checkout's is found.
        w.watch_tree(&repo.join("tc"));
        w.watch_tree(&tc2);
        w.add_root(&repo, false);
        wait_for("the watches", || w.roots().iter().all(|(_, s)| *s == RootState::Unsynced));
        assert_eq!(w.roots().into_iter().map(|(p, _)| p).collect::<Vec<_>>(), vec![tc2.clone(), repo.clone()]);
        let cookie_dir = |root: &Path| w.roots.read().unwrap().iter().find(|r| r.path == root).map(|r| r.cookie_dir.clone()).unwrap();
        assert_eq!(cookie_dir(&repo), repo.join(".git"));
        // ReadDirectoryChangesW: only a cookie inside the root comes through
        // its handle (walks skip it).
        assert_eq!(cookie_dir(&tc2), if cfg!(windows) { tc2.clone() } else { dir.join("cookies") });
        w.sync();
        assert!(w.roots().iter().all(|(_, s)| *s == RootState::Trusted), "{:?}", w.roots());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A build directory no build named for a while is forgotten: watched
    /// again (inotify: with the toolchain directory it's in), and its files
    /// vouched for.
    #[test]
    fn expires_build_dirs() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let dir = test_dirs::base().join(format!("ninja_daemon_expire_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tc/out")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let (tc, f) = (dir.join("tc"), dir.join("tc/out/f.h"));
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        w.ignore(&tc.join("out"));
        w.watch_tree(&tc);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        w.sync();
        assert_eq!(w.token(&f), None);
        w.expire(Instant::now());
        assert!(w.token(&f).is_some());
        std::fs::write(&f, "f").unwrap();
        wait_for("its change", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == f));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing in a repository's own directory is vouched for (`.git/index`,
    /// a ref a version stamp depends on): its events are ignored.
    #[test]
    fn repositories_own_files() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ninja_daemon_vcs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let (a, head, index) = (dir.join("src/a.h"), dir.join(".git/refs/heads/main"), dir.join(".git/index"));
        for f in [&a, &head, &index] {
            std::fs::write(f, "x").unwrap();
        }
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        assert_eq!(w.token(&a), None);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        for f in [&a, &head, &index] {
            w.token(f);
        }
        w.sync();
        assert!(w.token(&a).is_some());
        assert_eq!((w.token(&head), w.token(&index), w.token(&dir.join(".git/refs"))), (None, None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repository's directory created in a watched root: lookups of the
    /// repository there see it.
    #[test]
    fn repositories_created() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ninja_daemon_newrepo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tc/sub")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let (tc, sub) = (dir.join("tc"), dir.join("tc/sub"));
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.watch_tree(&tc);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        assert_ne!(w.repository(&sub), Some(sub.clone()));
        std::fs::create_dir(sub.join(".git")).unwrap();
        wait_for("the new repository", || w.repository(&sub) == Some(sub.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A root whose cookie can't be made (its repository's directory went)
    /// isn't in sync: it's watched again, its cookies made elsewhere.
    #[test]
    fn cookie_errors() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ninja_daemon_cookie_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo/.git")).unwrap();
        std::fs::create_dir_all(dir.join("repo/src")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let a = dir.join("repo/src/a.h");
        std::fs::write(&a, "a").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.set_cookie_dir(&dir.join("cookies"));
        assert_eq!(w.token(&a), None);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        std::fs::remove_dir(dir.join("repo/.git")).unwrap();
        w.sync();
        assert_ne!(w.roots()[0].1, RootState::Trusted);
        wait_for("in sync", || {
            w.sync();
            w.roots()[0].1 == RootState::Trusted
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A burst of files created costs the same however many directories the
    /// watcher knows: only a directory's own events go through what it
    /// learned under it.
    #[test]
    fn event_cost() {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_cost_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let sub = |i: usize| dir.join(format!("m{}", i % 50)).join(format!("d{i}"));
        for i in 0..4000 {
            w.real(&sub(i).join("f.h"));
            w.repository(&sub(i));
        }
        let t = Instant::now();
        for i in 0..10_000 {
            w.event(&sub(0).join(format!("new{i}.h")), true);
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        // A directory's own event still drops what was learned there.
        let k = w.key(&sub(1));
        assert!(w.real_dirs.read().unwrap().contains_key(&k) && w.repos.read().unwrap().contains_key(&k));
        w.event(&dir.join("m1"), true);
        assert!(!w.real_dirs.read().unwrap().contains_key(&k) && !w.repos.read().unwrap().contains_key(&k));
        assert!(w.repos.read().unwrap().contains_key(&w.key(&sub(2))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// inotify: nothing in a build directory is watched or vouched for, not
    /// even a toolchain asked for whole (it would be trusted unwatched, its
    /// cookies waited for in vain); once it expires, it's watched again and
    /// its changes seen.
    #[cfg(target_os = "linux")]
    #[test]
    fn build_dirs_unwatched() {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_unwatched_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["repo/.git", "repo/src", "repo/build/_deps/tc/bin", "out/tc/.git", "out/tc/bin"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (repo, tc1, tc2) = (dir.join("repo"), dir.join("repo/build/_deps/tc"), dir.join("out/tc"));
        let a = repo.join("src/a.h");
        std::fs::write(&a, "a").unwrap();
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let l2 = lines.clone();
        let w = Watcher::new(false, Box::new(move |m| l2.lock().unwrap().push(m.to_string())));
        w.set_cookie_dir(&dir.join("cookies"));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        w.ignore(&repo.join("build"));
        w.ignore(&dir.join("out"));
        assert_eq!(w.token(&a), None);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        w.watch_tree(&tc1);
        w.watch_tree(&tc2);
        w.sync();
        assert_eq!(w.roots(), vec![(repo.clone(), RootState::Trusted)]);
        assert!(!w.watched.read().unwrap().keys().any(|k| k.starts_with(&tc1)), "{:?}", w.watched.read().unwrap().keys());
        assert_eq!((w.tree_token(&tc1), w.tree_token(&tc2)), (None, None));
        w.expire(Instant::now());
        w.watch_tree(&tc1);
        w.watch_tree(&tc2);
        wait_for("the watches", || w.roots().iter().all(|(_, s)| *s == RootState::Unsynced || *s == RootState::Trusted));
        w.sync();
        assert!(w.tree_token(&tc1).is_some() && w.tree_token(&tc2).is_some(), "{:?}", w.roots());
        for tc in [&tc1, &tc2] {
            std::fs::write(tc.join("bin/cc"), "cc").unwrap();
            wait_for("its change", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == tc.join("bin/cc")));
        }
        // A build directory made in a tree watched whole isn't watched (nor
        // is that a loss).
        w.ignore(&tc2.join("out"));
        std::fs::create_dir(tc2.join("out")).unwrap();
        w.sync();
        assert!(w.roots().iter().all(|(_, s)| *s == RootState::Trusted), "{:?}", lines.lock().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows: a root replaced (a fresh clone where the old one was) is
    /// watched again, from its new directory.
    #[cfg(windows)]
    #[test]
    fn replaced_roots() {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_replaced_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo/.git")).unwrap();
        std::fs::create_dir_all(dir.join("repo/src")).unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        let (repo, a) = (dir.join("repo"), dir.join("repo/src/a.h"));
        std::fs::write(&a, "a").unwrap();
        let w = Watcher::new(true, Box::new(|_| {}));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        assert_eq!(w.token(&a), None);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        w.sync();
        assert!(w.token(&a).is_some());
        std::fs::rename(&repo, dir.join("repo.old")).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(&a, "the new clone's").unwrap();
        wait_for("the new root in sync", || {
            w.sync();
            w.token(&a).is_some()
        });
        rec.0.lock().unwrap().clear();
        std::fs::write(&a, "changed").unwrap();
        wait_for("its change", || rec.0.lock().unwrap().iter().any(|(p, _)| w.key(p) == w.key(&a)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
