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
//!   made there. A directory builds write to that holds a root (a
//!   repository's own, or one above checkouts) only has its own files
//!   checked every time. A root found around others (a checkout whose
//!   toolchain was listed first) takes them over; one whose directory is
//!   gone is forgotten.
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
//!   everything under a directory that was. What the daemon keeps is
//!   ordered by path, so that dropping what's under a directory costs what
//!   it held, not what the daemon knows (a branch switch removing
//!   thousands of them).
//! - **Paths.** Only paths without symbolic links are vouched for: events
//!   come for real paths. Nor are files with other names (hard links,
//!   whose changes through another name aren't reported here), paths in a
//!   repository's own directory, whose events are ignored (git writes
//!   there all the time), nor, on Linux, on file systems under a root that
//!   may not report every change.
//!
//! Backends: inotify on Linux (the build directories aren't watched, so
//!   that compiles writing objects don't flood the queue),
//!   `ReadDirectoryChangesW` on Windows (whose events in build directories
//!   are dropped before anything else is done with them). Elsewhere
//!   nothing is watched. One that can't be set up (out of inotify
//!   instances) is tried again at the next sync.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

pub use cpp_deps::scan::{Change, remove_under};

/// Gets what changed.
pub trait Listener: Send + Sync {
    fn changed(&self, path: &Path, change: Change);
    /// Nothing is asked for again at the paths under `dir` (it was removed
    /// or replaced, or builds no longer write there): what's kept for them
    /// goes, not just checked again.
    fn forget(&self, dir: &Path) {
        self.changed(dir, Change::Tree);
    }
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
    /// Failed: not started again before then.
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

/// What a path is, symbolic links not followed.
struct EntryKind {
    link: bool,
    dir: bool,
    /// Its names (hard links).
    links: u64,
}

#[cfg(unix)]
fn entry_kind(path: &Path) -> io::Result<EntryKind> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    Ok(EntryKind { link: m.file_type().is_symlink(), dir: m.is_dir(), links: m.nlink() })
}

/// From one handle, as `symlink_metadata` reads it, with the number of
/// links (which std doesn't give).
#[cfg(windows)]
fn entry_kind(path: &Path) -> io::Result<EntryKind> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_STANDARD_INFO, FileAttributeTagInfo, FileStandardInfo, GetFileInformationByHandleEx,
    };
    /// Symbolic links and junctions (name surrogates), as std tells links.
    const NAME_SURROGATE: u32 = 0x2000_0000;
    let f = std::fs::OpenOptions::new().access_mode(FILE_READ_ATTRIBUTES).custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
    let (mut tag, mut standard) = (FILE_ATTRIBUTE_TAG_INFO::default(), FILE_STANDARD_INFO::default());
    // SAFETY: an open handle, and buffers writable for their sizes.
    let ok = unsafe {
        GetFileInformationByHandleEx(f.as_raw_handle(), FileAttributeTagInfo, (&raw mut tag).cast(), size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32) != 0
            && GetFileInformationByHandleEx(f.as_raw_handle(), FileStandardInfo, (&raw mut standard).cast(), size_of::<FILE_STANDARD_INFO>() as u32) != 0
    };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    let link = tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag.ReparseTag & NAME_SURROGATE != 0;
    Ok(EntryKind { link, dir: standard.Directory, links: u64::from(standard.NumberOfLinks) })
}

#[cfg(not(any(unix, windows)))]
fn entry_kind(path: &Path) -> io::Result<EntryKind> {
    let m = std::fs::symlink_metadata(path)?;
    Ok(EntryKind { link: m.file_type().is_symlink(), dir: m.is_dir(), links: 1 })
}

/// Removes `key` and what's under it from `set` (see `remove_under`).
fn remove_set_under(set: &mut BTreeSet<PathBuf>, key: &Path) -> usize {
    let under: Vec<PathBuf> = set.range::<Path, _>((Bound::Included(key), Bound::Unbounded)).take_while(|k| k.starts_with(key)).cloned().collect();
    for k in &under {
        set.remove(k);
    }
    under.len()
}

/// Whether `map` has `key` or entries under it.
pub(crate) fn has_under<V>(map: &BTreeMap<PathBuf, V>, key: &Path) -> bool {
    map.range::<Path, _>((Bound::Included(key), Bound::Unbounded)).next().is_some_and(|(k, _)| k.starts_with(key))
}

/// A directory builds write to.
struct BuildDir {
    path: PathBuf,
    /// When a build last named it.
    named: Instant,
    /// Everything under it is theirs, else only its own files (see
    /// `Watcher::ignore`).
    whole: bool,
}

/// Directories builds named (keys). A path is in one when one of its
/// ancestors is: a lookup per ancestor, however many there are (CMake's
/// `try_compile` names hundreds a day).
#[derive(Default)]
pub(crate) struct BuildDirs {
    dirs: RwLock<HashMap<PathBuf, BuildDir>>,
    /// Lookups made (what a check costs, for tests).
    probes: AtomicU64,
}

impl BuildDirs {
    /// Adds `dir` (`key`), or notes it was named again: whether it was
    /// whole before (None: it's new).
    pub(crate) fn insert(&self, key: PathBuf, dir: &Path, whole: bool) -> Option<bool> {
        match self.dirs.write().unwrap().entry(key) {
            Entry::Occupied(mut e) => {
                let d = e.get_mut();
                d.named = Instant::now();
                Some(std::mem::replace(&mut d.whole, whole))
            }
            Entry::Vacant(e) => {
                e.insert(BuildDir { path: dir.to_path_buf(), named: Instant::now(), whole });
                None
            }
        }
    }

    /// Whether `key` is in one whole, or is `own` and one of those whose own
    /// files only are the builds'.
    fn find(&self, key: &Path, own: Option<&Path>) -> bool {
        let dirs = self.dirs.read().unwrap();
        !dirs.is_empty()
            && key.ancestors().any(|a| {
                self.probes.fetch_add(1, Ordering::Relaxed);
                dirs.get(a).is_some_and(|d| d.whole || Some(a) == own)
            })
    }

    /// Whether `key` is in one whole: nothing there is watched.
    pub(crate) fn covers(&self, key: &Path) -> bool {
        self.find(key, None)
    }

    /// Whether the file `key` may change during builds.
    pub(crate) fn has_file(&self, key: &Path) -> bool {
        self.find(key, key.parent())
    }

    /// Whether the directory `key`'s names may change during builds.
    pub(crate) fn has_dir(&self, key: &Path) -> bool {
        self.find(key, Some(key))
    }

    /// Forgets those no build named since `before`: their paths, and
    /// whether they were whole.
    pub(crate) fn expire(&self, before: Instant) -> Vec<(PathBuf, bool)> {
        let mut gone = Vec::new();
        self.dirs.write().unwrap().retain(|_, d| {
            let keep = d.named >= before;
            if !keep {
                gone.push((d.path.clone(), d.whole));
            }
            keep
        });
        gone
    }

    /// Their paths, and whether each is whole, those named last first.
    pub(crate) fn list(&self) -> Vec<(PathBuf, bool)> {
        let mut dirs: Vec<(Instant, PathBuf, bool)> = self.dirs.read().unwrap().values().map(|d| (d.named, d.path.clone(), d.whole)).collect();
        dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        dirs.into_iter().map(|(_, p, w)| (p, w)).collect()
    }
}

/// How long a root whose watches ran out stays checked once per build.
const RETRY_AFTER_ENOSPC: Duration = Duration::from_secs(10 * 60);

/// How long a root whose watch couldn't be set up (on a network file
/// system, unreadable) waits before it's tried again.
const RETRY_FAILED: Duration = Duration::from_secs(60);

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
    /// Before a sync: what it found since, and what it gave up on, again.
    fn sync(&self) {}
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
    /// removal drops everything under them. Ordered by path, as the maps
    /// below: an event drops what's under its path without going through
    /// the rest.
    known_dirs: RwLock<BTreeSet<PathBuf>>,
    /// Directories (keys) and the repository each is in.
    repos: RwLock<BTreeMap<PathBuf, Option<PathBuf>>>,
    /// Directories (keys) and whether their path is their real one.
    real_dirs: RwLock<BTreeMap<PathBuf, bool>>,
    /// Bumped whenever entries of `repos`, `real_dirs` or `watched` are
    /// forgotten: what a lookup that started before found may be stale,
    /// and isn't kept.
    forgotten: AtomicU64,
    /// Syncs started: a directory watched (lazily) during the `n`th is
    /// vouched for once a later one brought its root in sync.
    epoch: AtomicU64,
    /// Directories watched lazily (keys): the epoch they were, and whether
    /// with everything under them.
    watched: RwLock<BTreeMap<PathBuf, (u64, bool)>>,
    /// Linux: the file systems mounted (keys), and whether each reports
    /// every change.
    mount_table: RwLock<Vec<(PathBuf, bool)>>,
    listeners: RwLock<Vec<Arc<dyn Listener>>>,
    /// Cookies waited for, and those seen.
    cookies: Mutex<HashMap<String, bool>>,
    cookie_seen: Condvar,
    backend: OnceLock<Box<dyn Backend>>,
    /// Why the backend couldn't be set up, when it couldn't: tried again
    /// at the next sync.
    no_backend: Mutex<Option<String>>,
    /// Entries invalidations dropped, and lookups they made: what events
    /// cost (for tests).
    swept: AtomicU64,
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
            forgotten: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            watched: RwLock::default(),
            mount_table: RwLock::default(),
            listeners: RwLock::default(),
            cookies: Mutex::default(),
            cookie_seen: Condvar::new(),
            backend: OnceLock::new(),
            no_backend: Mutex::default(),
            swept: AtomicU64::new(0),
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

    /// The directories builds write to, and whether everything under each
    /// is theirs (else only its own files), those named last first.
    pub fn ignored_dirs(&self) -> Vec<(PathBuf, bool)> {
        self.ignored.list()
    }

    fn key(&self, p: &Path) -> PathBuf {
        key(p, self.case_insensitive)
    }

    /// Whether names are compared ignoring case (see `key`).
    pub fn case_insensitive(&self) -> bool {
        self.case_insensitive
    }

    fn backend(&self) -> Option<&dyn Backend> {
        match self.backend.get() {
            Some(b) => Some(b.as_ref()),
            None => self.set_up(false).ok(),
        }
    }

    /// The backend, set up now unless that failed already (`again`: even
    /// then); Err: why there's none.
    fn set_up(&self, again: bool) -> Result<&dyn Backend, String> {
        let mut why = self.no_backend.lock().unwrap();
        if let Some(b) = self.backend.get() {
            return Ok(b.as_ref());
        }
        if let Some(w) = why.as_ref().filter(|_| !again) {
            return Err(w.clone());
        }
        match platform::backend(self.me.clone()) {
            Ok(b) => {
                *why = None;
                Ok(self.backend.get_or_init(|| b).as_ref())
            }
            Err(e) => {
                let e = e.to_string();
                if why.as_ref() != Some(&e) {
                    (self.log)(&format!("no file watching: {e}"));
                }
                *why = Some(e.clone());
                Err(e)
            }
        }
    }

    fn lazy(&self) -> bool {
        self.backend().is_some_and(|b| b.lazy())
    }

    /// Directories watched lazily.
    pub fn watched_dirs(&self) -> usize {
        self.watched.read().unwrap().len()
    }

    /// Counts what an invalidation dropped, and its lookups.
    pub(crate) fn sweep(&self, n: usize) {
        self.swept.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Lookups of build directories made.
    #[cfg(test)]
    pub(crate) fn probes(&self) -> u64 {
        self.ignored.probes.load(Ordering::Relaxed)
    }

    /// Forgets the lazy watches of `key` and under it.
    fn forget_watched(&self, key: &Path) {
        let n = remove_under(&mut self.watched.write().unwrap(), key);
        self.sweep(n + 1);
    }

    /// Forgets what was learned about `key` and the directories under it,
    /// and their lazy watches (a directory created there again is watched
    /// again).
    fn forget(&self, key: &Path) {
        // Before anything goes: a lookup that started earlier then doesn't
        // put it back.
        self.forgotten.fetch_add(1, Ordering::SeqCst);
        let mut n = 0;
        if has_under(&self.repos.read().unwrap(), key) {
            n += remove_under(&mut self.repos.write().unwrap(), key);
        }
        if has_under(&self.real_dirs.read().unwrap(), key) {
            n += remove_under(&mut self.real_dirs.write().unwrap(), key);
        }
        if has_under(&self.watched.read().unwrap(), key) {
            n += remove_under(&mut self.watched.write().unwrap(), key);
        }
        self.sweep(n + 3);
    }

    /// Whether something under `key` is known: vouched for, watched or
    /// learned.
    fn known(&self, key: &Path) -> bool {
        self.known_dirs.read().unwrap().contains(key)
            || has_under(&self.watched.read().unwrap(), key)
            || has_under(&self.real_dirs.read().unwrap(), key)
            || has_under(&self.repos.read().unwrap(), key)
    }

    /// Records `key` as watched lazily since `epoch` (with everything under
    /// it, `whole`), unless something was forgotten since `forgotten` was
    /// read: the directory may be gone, and its watch with it.
    fn note_watched(&self, key: &Path, epoch: u64, whole: bool, forgotten: u64) -> bool {
        let mut watched = self.watched.write().unwrap();
        if self.forgotten.load(Ordering::SeqCst) != forgotten {
            return false;
        }
        watched.insert(key.to_path_buf(), (epoch, whole));
        true
    }

    /// A directory builds write to: their build directory, or another one
    /// (an output outside it). Its events are ignored, its files never
    /// vouched for (those that were are checked again), and roots in it go
    /// (watched again once it expires). Not `whole`: only its own files and
    /// names (it holds a root, see `ignore_outputs`), the rest is watched
    /// as before.
    pub fn ignore(&self, dir: &Path, whole: bool) {
        let key = self.key(dir);
        let before = self.ignored.insert(key.clone(), dir, whole);
        if before == Some(whole) {
            return;
        }
        if !whole {
            (self.log)(&format!("{}: builds write there, and roots are in it: its own files are checked every time", dir.display()));
            if before == Some(true) {
                self.unignore(&key, dir);
            }
            // Its files and names, and the trees above them.
            self.seq.fetch_add(1, Ordering::SeqCst);
            for l in self.listeners.read().unwrap().iter() {
                l.changed(dir, Change::Names);
            }
            return;
        }
        let gone: Vec<Root> = {
            let mut roots = self.roots.write().unwrap();
            let (gone, kept) = std::mem::take(&mut *roots).into_iter().partition(|r| r.key.starts_with(&key));
            *roots = kept;
            self.drop_under(dir);
            gone
        };
        if let Some(b) = self.backend() {
            b.ignore(dir);
            for r in &gone {
                b.unwatch(&r.path);
            }
        }
        self.forget_watched(&key);
    }

    /// A directory a build writes outputs to, outside its build directory:
    /// everything under it is the build's (generated files, in directories
    /// of their own), unless it holds a root: then its own files only,
    /// lest a file copied to a repository's root (`compile_commands.json`)
    /// or to the directory above checkouts leaves them unwatched. It holds
    /// one when it's a repository's root, isn't in any repository, or a
    /// root is under it.
    pub fn ignore_outputs(&self, dir: &Path) {
        let key = self.key(dir);
        let whole = match self.repository(dir) {
            Some(repo) if self.key(&repo) != key => !self.roots.read().unwrap().iter().any(|r| r.key.starts_with(&key)),
            _ => false,
        };
        self.ignore(dir, whole);
    }

    /// Forgets the build directories no build named since `before`: what
    /// builds wrote there goes, and they're watched again with the rest (a
    /// build names its own when it begins). Nothing in them was watched;
    /// were a root made there meanwhile, it's watched again from scratch.
    pub fn expire(&self, before: Instant) {
        for (dir, whole) in self.ignored.expire(before) {
            // Its own files only: nothing was taken from the rest.
            if !whole {
                continue;
            }
            for l in self.listeners.read().unwrap().iter() {
                l.forget(&dir);
            }
            self.unignore(&self.key(&dir), &dir);
        }
    }

    /// `dir` (`key`) is no longer a build directory whole: what it took
    /// from the watcher is given back.
    fn unignore(&self, key: &Path, dir: &Path) {
        self.forget_watched(key);
        let inside: Vec<PathBuf> = self.roots.read().unwrap().iter().filter(|r| r.key.starts_with(key)).map(|r| r.key.clone()).collect();
        for rkey in inside {
            self.restart(&rkey, "no longer in a build directory");
        }
        let Some(Err(e)) = self.backend().map(|b| b.unignore(dir)) else { return };
        let root = self.roots.read().unwrap().iter().find(|r| key.starts_with(&r.key)).map(|r| r.key.clone());
        match root {
            Some(rkey) if e.kind() == io::ErrorKind::StorageFull => self.release(&rkey, &e),
            Some(_) => self.lost(Some(dir)),
            None => {}
        }
    }

    /// Whether the file `key` may change during builds (see `ignore`).
    pub(crate) fn volatile_file(&self, key: &Path) -> bool {
        self.ignored.has_file(key)
    }

    /// Whether the directory `key` or what's under it may change during
    /// builds (see `ignore`).
    pub(crate) fn volatile_dir(&self, key: &Path) -> bool {
        self.ignored.has_dir(key)
    }

    /// Watches `dir`, a repository (lazily, see the module's doc) or,
    /// `whole`, everything under it (unless a root has it already, or it's
    /// in a build directory). Roots under it become part of it: their
    /// watches and cookies would overlap its own.
    pub fn add_root(&self, dir: &Path, whole: bool) {
        let key = self.key(dir);
        if self.ignored.covers(&key) {
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
            for r in &nested {
                self.drop_under(&r.path);
            }
            nested
        };
        for r in nested {
            if let Some(b) = self.backend() {
                b.unwatch(&r.path);
            }
            self.forget_watched(&r.key);
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
        if self.ignored.covers(&key) {
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
    /// what its directory is and what's mounted in it are found again. One
    /// whose directory is gone is forgotten; one that fails is tried again
    /// a while later.
    fn start(&self, dir: PathBuf, key: PathBuf, whole: bool) {
        let me = self.me.clone();
        std::thread::spawn(move || {
            let Some(w) = me.upgrade() else { return };
            if std::fs::metadata(&dir).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
                w.forget_root(&key, &dir);
                return;
            }
            let t = Instant::now();
            // Set up first: it reads what's mounted.
            let backend = w.set_up(false);
            // Watched from before the next sync, and found before it's
            // watched: a directory there since isn't the one watched.
            let epoch = w.epoch.load(Ordering::SeqCst);
            let forgotten = w.forgotten.load(Ordering::SeqCst);
            let (cookie_dir, id, mounts) = (w.cookie_dir_of(&dir), dir_id(&dir), w.mounts_in(&key));
            {
                let mut roots = w.roots.write().unwrap();
                let Some(r) = roots.iter_mut().find(|r| r.key == key) else { return };
                (r.cookie_dir, r.id, r.mounts) = (cookie_dir.clone(), id, mounts);
            }
            // A failure is tried again a while later; without a backend,
            // when a sync sets one up.
            let (state, out_of_watches, later) = match (backend, local_file_system(&dir)) {
                (Err(why), _) => (RootState::Failed(why), None, false),
                (_, Err(why)) => (RootState::Failed(why), None, true),
                (Ok(b), Ok(())) => match b.watch(&dir, &cookie_dir, whole) {
                    Ok(()) => (RootState::Unsynced, None, false),
                    Err(e) if e.kind() == io::ErrorKind::StorageFull => (RootState::Failed(e.to_string()), Some(e), false),
                    Err(e) => (RootState::Failed(e.to_string()), None, true),
                },
            };
            if state == RootState::Unsynced && w.lazy() && !whole {
                w.note_watched(&key, epoch, false, forgotten);
            }
            (w.log)(&format!("watching {} ({:.0?}): {state:?}", dir.display(), t.elapsed()));
            {
                let mut roots = w.roots.write().unwrap();
                if let Some(r) = roots.iter_mut().find(|r| r.key == key && r.state == RootState::Starting) {
                    r.retry_after = later.then(|| Instant::now() + RETRY_FAILED);
                    r.state = state;
                }
            }
            if let Some(e) = out_of_watches {
                w.release(&key, &e);
            }
            // Mounted or unmounted meanwhile.
            w.check_mounts(&key);
        });
    }

    /// A root whose directory is gone (a checkout removed): forgotten,
    /// found again when a file there is checked.
    fn forget_root(&self, key: &Path, dir: &Path) {
        {
            let mut roots = self.roots.write().unwrap();
            let before = roots.len();
            roots.retain(|r| !(r.key == key && r.state == RootState::Starting));
            if roots.len() == before {
                return;
            }
            self.drop_under(dir);
        }
        (self.log)(&format!("{}: gone: no longer watched", dir.display()));
        if let Some(b) = self.backend() {
            b.unwatch(dir);
        }
        self.forget_watched(key);
    }

    /// Watches a root again from scratch (its watch is broken, or of a
    /// directory no longer at its path): what was vouched for under it is
    /// checked again, and vouched for after the next sync.
    fn restart(&self, key: &Path, why: &str) {
        self.restart_if(key, why, |_| true);
    }

    /// `restart`s a root if `cond`, checked as it's marked starting (by
    /// one of those that may look at once).
    fn restart_if(&self, key: &Path, why: &str, cond: impl Fn(&Root) -> bool) {
        let (path, whole) = {
            let mut roots = self.roots.write().unwrap();
            let Some(r) = roots.iter_mut().find(|r| r.key == key && cond(r)) else { return };
            r.state = RootState::Starting;
            self.drop_under(&r.path);
            (r.path.clone(), r.whole)
        };
        (self.log)(&format!("{}: {why}: watching it again", path.display()));
        if let Some(b) = self.backend() {
            b.unwatch(&path);
        }
        self.forget_watched(key);
        self.start(path, key.to_path_buf(), whole);
    }

    /// Linux: the file systems mounted in `key` (or on it), and whether
    /// each reports every change.
    fn mounts_in(&self, key: &Path) -> Vec<(PathBuf, bool)> {
        self.mount_table.read().unwrap().iter().filter(|(m, _)| m.starts_with(key)).cloned().collect()
    }

    /// Linux: the file systems mounted are `table` (maybe as the mount
    /// poller found them already, before it looked at roots: a sync and
    /// the poller both look).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn mounts_changed(&self, table: Vec<(PathBuf, bool)>) {
        *self.mount_table.write().unwrap() = table;
        let keys: Vec<PathBuf> = self.roots.read().unwrap().iter().map(|r| r.key.clone()).collect();
        for key in keys {
            self.check_mounts(&key);
        }
    }

    /// A root with a file system mounted or unmounted in it since its watch
    /// was set up is watched again from scratch: a directory mounted over
    /// is hidden from its watch, and so are the files under it. Not one
    /// being set up (it reads them then), nor one that failed.
    fn check_mounts(&self, key: &Path) {
        let moved = |r: &Root| matches!(r.state, RootState::Unsynced | RootState::Trusted) && self.mounts_in(&r.key) != r.mounts;
        self.restart_if(key, "file systems were mounted or unmounted there", moved);
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
            let forgotten = self.forgotten.load(Ordering::SeqCst);
            match b.watch_dir(&d, whole) {
                Ok(()) => {
                    self.note_watched(&self.key(&d), epoch, whole, forgotten);
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
        let mut roots = self.roots.write().unwrap();
        if let Some(r) = roots.iter_mut().find(|r| r.key == key) {
            r.state = RootState::Failed(format!("no watches left: {why}"));
            r.retry_after = Some(Instant::now() + RETRY_AFTER_ENOSPC);
            self.drop_under(&r.path);
        }
    }

    /// The repository `dir` is in (cached for every directory on the way;
    /// until the next sync when there's none).
    fn repository(&self, dir: &Path) -> Option<PathBuf> {
        let forgotten = self.forgotten.load(Ordering::SeqCst);
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
        let mut repos = self.repos.write().unwrap();
        if self.forgotten.load(Ordering::SeqCst) == forgotten {
            for k in walked {
                repos.insert(k, found.clone());
            }
        }
        found
    }

    /// Whether `path` names itself (no symbolic link in it) and has no
    /// other name (hard links): if so, whether it's a directory.
    fn real(&self, path: &Path) -> Option<bool> {
        let dir = path.parent()?;
        let dkey = self.key(dir);
        let cached = self.real_dirs.read().unwrap().get(&dkey).copied();
        let real_dir = cached.unwrap_or_else(|| {
            let forgotten = self.forgotten.load(Ordering::SeqCst);
            let real = std::fs::canonicalize(dir).is_ok_and(|c| self.key(&strip_verbatim(c)) == dkey);
            let mut real_dirs = self.real_dirs.write().unwrap();
            if self.forgotten.load(Ordering::SeqCst) == forgotten {
                real_dirs.insert(dkey, real);
            }
            real
        });
        if !real_dir {
            return None;
        }
        match entry_kind(path) {
            Ok(e) if e.link => None,
            // Changed through a name in another directory, it isn't
            // reported here (nor, on Linux, through another one here).
            Ok(e) if !e.dir && e.links > 1 => None,
            e => Some(e.is_ok_and(|e| e.dir)),
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

    /// Before a build: sets the backend up if it couldn't be before,
    /// restarts the watches that failed (a while ago) or are of a directory
    /// no longer at their root's path, and brings every root in sync (see
    /// the module's doc). Roots the daemon can't write to are trusted
    /// without a cookie: system directories change rarely.
    pub fn sync(&self) {
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(b) = self.set_up(true) {
            b.sync();
        }
        // A repository made since a lookup found none is found now.
        self.forgotten.fetch_add(1, Ordering::SeqCst);
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

    /// Everything under `dir` is checked again (vouched for again once
    /// checked). Under a root, done as the root's state changes, with its
    /// lock held: a sync that finds the root out of sync goes on, and what
    /// was vouched for under it must be gone by then.
    fn drop_under(&self, dir: &Path) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        for l in self.listeners.read().unwrap().iter() {
            l.changed(dir, Change::Tree);
        }
        let n = remove_set_under(&mut self.known_dirs.write().unwrap(), &self.key(dir));
        self.sweep(n + 1);
    }

    /// A backend lost events (of the root `under` is in, or of every
    /// root). With a watch per directory, those of directories replaced
    /// meanwhile went too, unseen: the roots are watched again from
    /// scratch, vouched for after the next sync.
    fn lost(&self, under: Option<&Path>) {
        let ukey = under.map(|p| self.key(p));
        let lazy = self.lazy();
        let mut hit: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
        {
            let mut roots = self.roots.write().unwrap();
            for r in roots.iter_mut().filter(|r| ukey.as_ref().is_none_or(|k| k.starts_with(&r.key))) {
                if lazy && !matches!(r.state, RootState::Failed(_)) {
                    r.state = RootState::Starting;
                    hit.push((r.key.clone(), r.path.clone(), r.whole));
                } else if r.state == RootState::Trusted {
                    r.state = RootState::Unsynced;
                }
                self.drop_under(&r.path);
            }
        }
        (self.log)(&format!("events lost ({}): checking files again", under.map_or("every root".to_string(), |r| r.display().to_string())));
        for (key, path, whole) in hit {
            self.forget_watched(&key);
            self.start(path, key, whole);
        }
    }

    /// A backend's watch of `dir`, below a root, went (its file system was
    /// unmounted, or it was removed): what was vouched for under it is
    /// checked again, and it's watched again when needed.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn unwatched(&self, dir: &Path) {
        self.forget(&self.key(dir));
        self.drop_under(dir);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn is_root(&self, dir: &Path) -> bool {
        let key = self.key(dir);
        self.roots.read().unwrap().iter().any(|r| r.key == key)
    }

    /// A backend's watch of `root` stopped: tried again at the next sync
    /// (forgotten then if the directory is gone).
    fn failed(&self, root: &Path, why: &str) {
        let key = self.key(root);
        (self.log)(&format!("watching {} stopped: {why}", root.display()));
        if let Some(r) = self.roots.write().unwrap().iter_mut().find(|r| r.key == key) {
            r.state = RootState::Failed(why.to_string());
            self.drop_under(&r.path);
        }
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
        !self.ignored.covers(&key) && (!in_repository_dir(&key) || key.file_name().is_some_and(|n| VCS_DIRS.iter().any(|v| n == *v)))
    }

    /// Something under `dir` changed, its name unknown (a short name for a
    /// file gone): everything under it is checked again.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn changed_under(&self, dir: &Path) {
        let key = self.key(dir);
        if self.ignored.covers(&key) {
            return;
        }
        self.forget(&key);
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
        // A build's output (or, beside a root's files, a file it writes):
        // nothing there is vouched for, unless under a directory there.
        if self.ignored.covers(&key) || (self.ignored.has_file(&key) && !self.known(&key)) {
            return;
        }
        if in_repository_dir(&key) {
            // Nothing there is vouched for. A repository's directory made
            // or removed: lookups of the repository beside it change.
            if structural
                && key.file_name().is_some_and(|n| VCS_DIRS.iter().any(|v| n == *v))
                && let Some(parent) = key.parent()
            {
                self.forgotten.fetch_add(1, Ordering::SeqCst);
                let n = remove_under(&mut self.repos.write().unwrap(), parent);
                self.sweep(n + 1);
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
        // A directory above vouched-for files replaced, removed or renamed:
        // nothing under it is at these paths any more.
        if self.known_dirs.read().unwrap().contains(&key) {
            for l in &listeners {
                l.forget(path);
            }
            let n = remove_set_under(&mut self.known_dirs.write().unwrap(), &key);
            self.sweep(n + 1);
        }
        // What was learned about directories there: a file's events (most
        // of them) concern nothing of it.
        self.forget(&key);
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
        if self.ignored.covers(&key) || in_repository_dir(&key) {
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
                // Written by builds beside the root's files.
                let volatile = if is_dir { self.ignored.has_dir(&key) } else { self.ignored.has_file(&key) };
                if volatile {
                    return None;
                }
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

    /// The preprocessor asks for files.
    fn volatile(&self, path: &Path) -> bool {
        self.volatile_file(&self.key(path))
    }
}

/// `p` as a key: lowercase on case-insensitive file systems.
pub fn key(p: &Path, case_insensitive: bool) -> PathBuf {
    if case_insensitive { PathBuf::from(p.to_string_lossy().to_lowercase()) } else { p.to_path_buf() }
}

/// A canonical path without Windows' `\\?\` prefix.
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
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
    use std::sync::atomic::AtomicBool;

    pub(super) fn backend(sink: Weak<Watcher>) -> io::Result<Box<dyn Backend>> {
        // SAFETY: plain syscall.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            let hint = match e.raw_os_error() {
                Some(libc::EMFILE) => ": out of inotify instances (raise fs.inotify.max_user_instances) or of file descriptors",
                Some(libc::ENFILE | libc::ENOMEM) => ": out of resources",
                _ => "",
            };
            return Err(io::Error::new(e.kind(), format!("inotify_init1: {e}{hint}")));
        }
        let inotify = Arc::new(Inotify {
            fd,
            wds: Mutex::default(),
            whole: RwLock::default(),
            sink,
            mounts: Mutex::default(),
            polling: Arc::new(AtomicBool::new(false)),
            mounts_error: Mutex::default(),
        });
        // Read before roots are watched.
        inotify.refresh_mounts();
        let reader = inotify.clone();
        std::thread::spawn(move || reader.read_loop());
        Ok(Box::new(Shared(inotify)))
    }

    /// The file systems `/proc/self/mountinfo` lists (keys), and whether
    /// each reports every change.
    fn read_mounts(file: &mut std::fs::File, case_insensitive: bool) -> io::Result<Vec<(PathBuf, bool)>> {
        use std::io::{Read, Seek};
        let mut text = String::new();
        file.rewind()?;
        file.read_to_string(&mut text)?;
        Ok(parse_mounts(&text).into_iter().map(|(p, local)| (key(&p, case_insensitive), local)).collect())
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

    /// Tells the watcher when file systems are mounted or unmounted, until
    /// polling fails (said in the log; the next sync starts it again).
    fn watch_mounts(mut table: std::fs::File, sink: Weak<Watcher>, polling: Arc<AtomicBool>) {
        use std::os::fd::AsRawFd;
        let stop = |w: &Watcher, why: String| {
            (w.log)(&format!("polling the mount table: {why}"));
            polling.store(false, Ordering::SeqCst);
        };
        loop {
            let mut pfd = libc::pollfd { fd: table.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
            // SAFETY: one valid pollfd. Every while without a change:
            // whether the watcher is still there.
            let n = unsafe { libc::poll(&mut pfd, 1, 10_000) };
            let Some(w) = sink.upgrade() else { return };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return stop(&w, e.to_string());
            }
            if n > 0 && pfd.revents & (libc::POLLPRI | libc::POLLERR) != 0 {
                match read_mounts(&mut table, w.case_insensitive) {
                    Ok(t) => w.mounts_changed(t),
                    Err(e) => return stop(&w, e.to_string()),
                }
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
            remove_set_under(&mut self.0.whole.write().unwrap(), dir);
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

        /// A file system mounted just before isn't polled yet.
        fn sync(&self) {
            self.0.refresh_mounts();
        }
    }

    /// Whether `dir` is in one of `dirs`.
    fn under(dirs: &RwLock<BTreeSet<PathBuf>>, dir: &Path) -> bool {
        let dirs = dirs.read().unwrap();
        !dirs.is_empty() && dir.ancestors().any(|a| dirs.contains(a))
    }

    /// Watch descriptors and their directories, and the other way round
    /// (ordered by path: those under a directory go together).
    #[derive(Default)]
    struct Wds {
        dirs: HashMap<i32, PathBuf>,
        by_dir: BTreeMap<PathBuf, i32>,
    }

    impl Wds {
        /// Whether it's new (not a directory watched already).
        fn insert(&mut self, wd: i32, dir: &Path) -> bool {
            // Another directory's at that path before: its events, if any
            // are left, still name it, until it's gone.
            let new = self.by_dir.insert(dir.to_path_buf(), wd) != Some(wd);
            self.dirs.insert(wd, dir.to_path_buf());
            new
        }

        /// Forgets the watches of `dir` and under it: theirs.
        fn take_under(&mut self, dir: &Path) -> Vec<i32> {
            let under: Vec<(PathBuf, i32)> = self
                .by_dir
                .range::<Path, _>((Bound::Included(dir), Bound::Unbounded))
                .take_while(|(d, _)| d.starts_with(dir))
                .map(|(d, wd)| (d.clone(), *wd))
                .collect();
            under
                .into_iter()
                .map(|(d, wd)| {
                    self.by_dir.remove(&d);
                    self.dirs.remove(&wd);
                    wd
                })
                .collect()
        }
    }

    struct Inotify {
        fd: i32,
        wds: Mutex<Wds>,
        /// Directories watched with everything under them: directories
        /// created there are watched as they appear.
        whole: RwLock<BTreeSet<PathBuf>>,
        sink: Weak<Watcher>,
        /// The mount table as syncs read it (see `refresh_mounts`).
        mounts: Mutex<Option<std::fs::File>>,
        /// Whether a thread polls the mount table between syncs.
        polling: Arc<AtomicBool>,
        /// Why the mount table couldn't be read, said once.
        mounts_error: Mutex<Option<String>>,
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

    /// Whether `e` says a directory went (or is a file now) meanwhile.
    fn gone(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR))
    }

    impl Inotify {
        /// Whether `dir` is in a build directory (see `Watcher::ignore`).
        fn is_ignored(&self, dir: &Path) -> bool {
            self.sink.upgrade().is_some_and(|w| w.ignored.covers(&w.key(dir)))
        }

        /// Once set up, and before each sync: tells the watcher what's
        /// mounted if that changed since (a file system mounted just before
        /// isn't polled yet: polling the table says whether it changed since
        /// it was last polled), and starts the thread polling it between
        /// syncs if it isn't running. A table that can't be read is said
        /// once (mounts in roots go unnoticed then), and read again at the
        /// next sync.
        fn refresh_mounts(&self) {
            use std::os::fd::AsRawFd;
            const MOUNTINFO: &str = "/proc/self/mountinfo";
            let Some(w) = self.sink.upgrade() else { return };
            let mut mounts = self.mounts.lock().unwrap();
            let read = match mounts.as_mut() {
                None => std::fs::File::open(MOUNTINFO).and_then(|mut f| {
                    let table = read_mounts(&mut f, w.case_insensitive)?;
                    *mounts = Some(f);
                    Ok(Some(table))
                }),
                Some(f) => {
                    let mut pfd = libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLPRI, revents: 0 };
                    // SAFETY: one valid pollfd; no waiting.
                    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
                    if n > 0 && pfd.revents & (libc::POLLPRI | libc::POLLERR) != 0 { read_mounts(f, w.case_insensitive).map(Some) } else { Ok(None) }
                }
            };
            match read {
                Ok(table) => {
                    *self.mounts_error.lock().unwrap() = None;
                    if let Some(table) = table {
                        w.mounts_changed(table);
                    }
                }
                Err(e) => {
                    *mounts = None;
                    let e = e.to_string();
                    let mut said = self.mounts_error.lock().unwrap();
                    if said.as_ref() != Some(&e) {
                        (w.log)(&format!("reading {MOUNTINFO}: {e}: file systems mounted in roots go unnoticed"));
                        *said = Some(e);
                    }
                    return;
                }
            }
            drop(mounts);
            if !self.polling.swap(true, Ordering::SeqCst) {
                match std::fs::File::open(MOUNTINFO) {
                    Ok(f) => {
                        let (sink, polling) = (self.sink.clone(), self.polling.clone());
                        std::thread::spawn(move || watch_mounts(f, sink, polling));
                    }
                    Err(_) => self.polling.store(false, Ordering::SeqCst),
                }
            }
        }

        /// Watches `dir` alone (not in a build directory).
        fn add_one(&self, dir: &Path) -> io::Result<()> {
            if self.is_ignored(dir) {
                return Err(build_dir(dir));
            }
            self.add(dir).map(drop).map_err(|e| io::Error::new(e.kind(), format!("watching {}: {e}", dir.display())))
        }

        /// Adds a watch on `dir`: whether it's new.
        fn add(&self, dir: &Path) -> io::Result<bool> {
            let c = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
            // SAFETY: `c` is a valid C string.
            let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
            if wd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(self.wds.lock().unwrap().insert(wd, dir))
        }

        /// Watches `dir` and the directories under it (not into
        /// repositories' own directories, nor build directories, whose
        /// files aren't vouched for). One gone meanwhile has its own event;
        /// one that can't be watched or listed otherwise (out of watches,
        /// of file descriptors, unreadable) fails it all: those it added
        /// are given back (out of watches, all under `dir`), lest the
        /// directory be trusted with parts unwatched.
        fn add_tree(&self, dir: &Path) -> io::Result<()> {
            if self.is_ignored(dir) {
                return Err(build_dir(dir));
            }
            let mut added = Vec::new();
            let fail = |added: &[PathBuf], d: &Path, e: io::Error| {
                if e.raw_os_error() == Some(libc::ENOSPC) {
                    self.remove_tree(dir);
                } else {
                    for a in added {
                        self.remove_one(a);
                    }
                }
                Err(io::Error::new(e.kind(), format!("watching {}: {e}", d.display())))
            };
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                let top = d == dir;
                if !top && self.is_ignored(&d) {
                    continue;
                }
                match self.add(&d) {
                    Ok(true) => added.push(d.clone()),
                    Ok(false) => {}
                    Err(e) if !top && gone(&e) => continue,
                    Err(e) => return fail(&added, &d, e),
                }
                if d.file_name().is_some_and(|n| VCS_DIRS.iter().any(|v| n == *v)) {
                    continue;
                }
                let entries = match std::fs::read_dir(&d) {
                    Ok(rd) => rd,
                    Err(e) if !top && gone(&e) => continue,
                    Err(e) => return fail(&added, &d, e),
                };
                for e in entries {
                    match e.and_then(|e| e.file_type().map(|t| (e, t))) {
                        Ok((e, t)) if t.is_dir() => stack.push(e.path()),
                        Ok(_) => {}
                        Err(e) if gone(&e) => {}
                        Err(e) => return fail(&added, &d, e),
                    }
                }
            }
            Ok(())
        }

        /// Stops watching `dir` alone.
        fn remove_one(&self, dir: &Path) {
            let mut wds = self.wds.lock().unwrap();
            if let Some(wd) = wds.by_dir.remove(dir) {
                wds.dirs.remove(&wd);
                // SAFETY: plain syscall.
                unsafe { libc::inotify_rm_watch(self.fd, wd) };
            }
        }

        /// Stops watching `dir` and the directories under it: their
        /// watches' last events (`IN_IGNORED`) then name nothing.
        fn remove_tree(&self, dir: &Path) {
            let removed = self.wds.lock().unwrap().take_under(dir);
            for wd in &removed {
                // SAFETY: plain syscall (one the kernel removed already
                // fails, harmlessly).
                unsafe { libc::inotify_rm_watch(self.fd, *wd) };
            }
            if let Some(w) = self.sink.upgrade() {
                w.sweep(removed.len() + 1);
            }
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
            let Some(dir) = self.wds.lock().unwrap().dirs.get(&ev.wd).cloned() else { return };
            if ev.mask & (libc::IN_IGNORED | libc::IN_UNMOUNT) != 0 {
                // The watch went. A directory removed or renamed: its parent
                // reported it first, which forgot its watches (they name
                // nothing then). One replaced since: its path is watched
                // again, with another descriptor. Otherwise its file system
                // was unmounted, which nothing reported.
                let current = {
                    let mut wds = self.wds.lock().unwrap();
                    let current = wds.by_dir.get(&dir) == Some(&ev.wd);
                    if !current {
                        wds.dirs.remove(&ev.wd);
                    }
                    current
                };
                if !current {
                    return;
                }
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
            if is_dir && ev.mask & (libc::IN_MOVED_FROM | libc::IN_DELETE) != 0 {
                // Its watches would report the old paths, or are going: the
                // event on the path drops what was under it.
                remove_set_under(&mut self.whole.write().unwrap(), &path);
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
        use test_dirs::TestDir;

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

        /// A backend with these watches, for its events as it gets them.
        fn inotify(w: &Arc<Watcher>, wds: &[(i32, PathBuf)]) -> Inotify {
            let mut table = Wds::default();
            for (wd, dir) in wds {
                table.insert(*wd, dir);
            }
            Inotify {
                fd: -1,
                wds: Mutex::new(table),
                whole: Default::default(),
                sink: Arc::downgrade(w),
                mounts: Mutex::default(),
                polling: Arc::default(),
                mounts_error: Mutex::default(),
            }
        }

        fn event(wd: i32, mask: u32) -> libc::inotify_event {
            libc::inotify_event { wd, mask, cookie: 0, len: 0 }
        }

        /// Vouches for `f`, once its root is in sync.
        fn in_sync(w: &Watcher, f: &Path) {
            assert_eq!(w.token(f), None);
            wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
            w.token(f);
            w.sync();
            assert!(w.token(f).is_some());
        }

        /// A watch gone without its parent saying (its file system was
        /// unmounted): what was vouched for under it is checked again, and
        /// the directory watched again when needed; a root's stops.
        #[test]
        fn watches_gone() {
            let base = TestDir::new("ninja_daemon_gone");
            std::fs::create_dir_all(base.join(".git")).unwrap();
            std::fs::create_dir_all(base.join("mnt/sub")).unwrap();
            let dir = std::fs::canonicalize(&*base).unwrap();
            let f = dir.join("mnt/sub/f.h");
            std::fs::write(&f, "f").unwrap();
            let w = Watcher::new(false, Box::new(|_| {}));
            let rec = Arc::new(Recorder::default());
            w.listen(rec.clone());
            in_sync(&w, &f);
            let backend = inotify(&w, &[(1, dir.join("mnt")), (2, dir.clone())]);
            backend.handle(&w, &event(1, libc::IN_UNMOUNT), b"");
            assert!(rec.0.lock().unwrap().iter().any(|(p, c)| *p == dir.join("mnt") && *c == Change::Tree));
            assert_eq!(w.token(&f), None);
            w.sync();
            assert!(w.token(&f).is_some());
            backend.handle(&w, &event(2, libc::IN_IGNORED), b"");
            assert!(matches!(w.roots()[0].1, RootState::Failed(_)), "{:?}", w.roots());
        }

        /// A directory removed: its parent's event drops what was under it,
        /// and the directory's watch going (`IN_IGNORED`, after) drops
        /// nothing again. With thousands of directories removed (a branch
        /// switch), each went through everything the daemon keeps twice.
        #[test]
        fn removed_directories() {
            let base = TestDir::new("ninja_daemon_removed");
            std::fs::create_dir_all(base.join(".git")).unwrap();
            std::fs::create_dir_all(base.join("d/sub")).unwrap();
            let dir = std::fs::canonicalize(&*base).unwrap();
            let (sub, f) = (dir.join("d/sub"), dir.join("d/sub/f.h"));
            std::fs::write(&f, "f").unwrap();
            let w = Watcher::new(false, Box::new(|_| {}));
            let rec = Arc::new(Recorder::default());
            w.listen(rec.clone());
            in_sync(&w, &f);
            // `rm -r d/sub`, as reported: its file, its parent's event, its own.
            let backend = inotify(&w, &[(1, dir.join("d")), (2, sub.clone())]);
            backend.handle(&w, &event(2, libc::IN_DELETE), b"f.h");
            backend.handle(&w, &event(1, libc::IN_DELETE | libc::IN_ISDIR), b"sub");
            backend.handle(&w, &event(2, libc::IN_DELETE_SELF), b"");
            backend.handle(&w, &event(2, libc::IN_IGNORED), b"");
            let drops = rec.0.lock().unwrap().iter().filter(|(p, c)| *p == sub && *c == Change::Tree).count();
            assert_eq!(drops, 1, "{:?}", rec.0.lock().unwrap());
            assert!(backend.wds.lock().unwrap().dirs.keys().eq([1].iter()));
            // Watched again when needed.
            assert!(!w.watched.read().unwrap().contains_key(&sub));
        }

        /// A tree watched whole with a directory that can't be watched
        /// (unreadable; or out of file descriptors) isn't watched at all: it
        /// used to be trusted with that directory unwatched, whose changes
        /// went unseen.
        #[test]
        fn unwatchable_trees() {
            use std::os::unix::fs::PermissionsExt;
            // SAFETY: plain call.
            if unsafe { libc::geteuid() } == 0 {
                return;
            }
            let base = TestDir::new("ninja_daemon_unwatchable");
            std::fs::create_dir_all(base.join("tc/bin/locked")).unwrap();
            let dir = std::fs::canonicalize(&*base).unwrap();
            let (tc, locked) = (dir.join("tc"), dir.join("tc/bin/locked"));
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0)).unwrap();
            let w = Watcher::new(false, Box::new(|_| {}));
            w.set_cookie_dir(&dir.join("cookies"));
            w.watch_tree(&tc);
            wait_for("the watch", || w.roots().iter().all(|(_, s)| *s != RootState::Starting));
            let state = w.roots()[0].1.clone();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(&state, RootState::Failed(why) if why.contains("locked")), "{state:?}");
            assert_eq!(w.tree_token(&tc), None);
        }

        /// Out of inotify instances (here, of file descriptors) when the
        /// daemon first watches: its roots fail, saying why, and the next
        /// sync sets watching up again (it used to be off for the daemon's
        /// life). In a process of its own, which runs out of descriptors.
        #[test]
        fn set_up_again() {
            const CHILD: &str = "NINJA_DAEMON_TEST_SET_UP_AGAIN";
            if std::env::var_os(CHILD).is_none() {
                let out = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "watch::platform::tests::set_up_again", "--nocapture", "--test-threads=1"])
                    .env(CHILD, "1")
                    .output()
                    .unwrap();
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                assert!(out.status.success() && text.contains("1 passed"), "{text}");
                return;
            }
            let base = TestDir::new("ninja_daemon_set_up");
            std::fs::create_dir_all(base.join(".git")).unwrap();
            std::fs::create_dir_all(base.join("src")).unwrap();
            let dir = std::fs::canonicalize(&*base).unwrap();
            let f = dir.join("src/f.h");
            std::fs::write(&f, "f").unwrap();
            let w = Watcher::new(false, Box::new(|_| {}));
            let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            // SAFETY: valid out-parameters.
            unsafe {
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit);
                limit.rlim_cur = limit.rlim_cur.min(256);
                libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
            }
            let mut held = Vec::new();
            while let Ok(f) = std::fs::File::open("/dev/null") {
                held.push(f);
            }
            assert_eq!(w.token(&f), None);
            wait_for("the root failing", || matches!(w.roots().first(), Some((_, RootState::Failed(_)))));
            drop(held);
            let state = w.roots()[0].1.clone();
            assert!(matches!(&state, RootState::Failed(why) if why.contains("max_user_instances")), "{state:?}");
            wait_for("the root in sync", || {
                w.token(&f);
                w.sync();
                w.token(&f).is_some()
            });
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
            let base = TestDir::new("ninja_daemon_mounts");
            for d in [".git", "a", "net"] {
                std::fs::create_dir_all(base.join(d)).unwrap();
            }
            let dir = std::fs::canonicalize(&*base).unwrap();
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
            wait_for("the root in sync", in_sync);
            assert!(w.token(&g).is_some());
            let mut table = w.mount_table.read().unwrap().clone();
            table.push((dir.join("a"), true));
            w.mounts_changed(table.clone());
            assert_eq!(w.token(&f), None);
            wait_for("the root in sync again", in_sync);
            table.push((dir.join("net"), false));
            w.mounts_changed(table);
            wait_for("the root in sync once more", in_sync);
            assert_eq!(w.token(&g), None);
        }
    }
}

#[cfg(windows)]
mod platform {
    //! `ReadDirectoryChangesW` on each root (recursive), each read by its
    //! own thread, overlapped: it waits for changes or to be stopped,
    //! whichever comes first. A zero-length result or
    //! `ERROR_NOTIFY_ENUM_DIR` means the buffer overflowed: events were
    //! lost.

    use super::*;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::atomic::AtomicBool;

    use ninja_os::windows::wide;
    use windows_sys::Win32::Foundation::{ERROR_NOTIFY_ENUM_DIR, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ACTION_MODIFIED, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
        FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetLongPathNameW, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects};

    const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION
        | FILE_NOTIFY_CHANGE_SECURITY;

    pub(super) fn backend(sink: Weak<Watcher>) -> io::Result<Box<dyn Backend>> {
        Ok(Box::new(Directories { sink, active: Arc::default() }))
    }

    struct Directories {
        sink: Weak<Watcher>,
        /// Roots with a running reader, and how to stop it.
        active: Arc<Mutex<HashMap<PathBuf, Arc<Stop>>>>,
    }

    /// Stops a reader: the flag, which it checks between events, and the
    /// event it waits for changes with (set before it waits, the wait ends
    /// at once).
    struct Stop {
        flag: AtomicBool,
        event: OwnedHandle,
    }

    impl Stop {
        fn stop(&self) {
            self.flag.store(true, Ordering::SeqCst);
            // SAFETY: an event this owns.
            unsafe { SetEvent(self.event.as_raw_handle()) };
        }

        fn stopped(&self) -> bool {
            self.flag.load(Ordering::SeqCst)
        }
    }

    /// A manual-reset event, not set.
    fn event() -> io::Result<OwnedHandle> {
        // SAFETY: plain call.
        let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new handle, owned from now on.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    impl Backend for Directories {
        /// A reader already there reads what was at `root` then (a root
        /// replaced since by a fresh clone, say): it's stopped, and the
        /// directory there now opened. Watched once the new reader's first
        /// read is issued: events from then on are kept between reads (in
        /// a buffer of the first read's size).
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
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a new handle, owned from now on (by the reader, which
            // closes it when it ends).
            let handle = unsafe { OwnedHandle::from_raw_handle(h) };
            let stop = Arc::new(Stop { flag: AtomicBool::new(false), event: event()? });
            let (tx, rx) = std::sync::mpsc::channel();
            let root = root.to_path_buf();
            self.active.lock().unwrap().insert(root.clone(), stop.clone());
            let (sink, active) = (self.sink.clone(), self.active.clone());
            std::thread::spawn(move || {
                read_loop(&handle, &root, sink, tx, &stop);
                let mut active = active.lock().unwrap();
                if active.get(&root).is_some_and(|s| Arc::ptr_eq(s, &stop)) {
                    active.remove(&root);
                }
            });
            rx.recv().unwrap_or_else(|_| Err(io::Error::other("the reader ended")))
        }

        fn unwatch(&self, dir: &Path) {
            self.active.lock().unwrap().retain(|root, stop| {
                let under = root.starts_with(dir);
                if under {
                    stop.stop();
                }
                !under
            });
        }
    }

    /// Reads `root`'s changes until stopped, or its watch fails. Says
    /// through `started` whether its first read was issued.
    fn read_loop(handle: &OwnedHandle, root: &Path, sink: Weak<Watcher>, started: std::sync::mpsc::Sender<io::Result<()>>, stop: &Stop) {
        let done = match event() {
            Ok(e) => e,
            Err(e) => {
                let _ = started.send(Err(e));
                return;
            }
        };
        let mut started = Some(started);
        let mut buf = vec![0u32; 1 << 18];
        let h = handle.as_raw_handle();
        loop {
            let mut ov = OVERLAPPED { hEvent: done.as_raw_handle(), ..Default::default() };
            // SAFETY: `buf` is u32-aligned and writable for its byte length;
            // it and `ov` stay alive, unused, until the read is over (done,
            // or cancelled and waited for).
            let ok = unsafe {
                ResetEvent(done.as_raw_handle());
                ReadDirectoryChangesW(h, buf.as_mut_ptr().cast(), (buf.len() * 4) as u32, 1, FILTER, std::ptr::null_mut(), &mut ov, None)
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                match (started.take(), sink.upgrade()) {
                    (Some(s), _) => drop(s.send(Err(e))),
                    (None, Some(w)) if !stop.stopped() => w.failed(root, &e.to_string()),
                    _ => {}
                }
                return;
            }
            if let Some(s) = started.take() {
                let _ = s.send(Ok(()));
            }
            let waits = [done.as_raw_handle(), stop.event.as_raw_handle()];
            let mut n = 0u32;
            // SAFETY: two valid handles.
            if unsafe { WaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE) } != WAIT_OBJECT_0 {
                // Stopped (or the wait failed): the read is cancelled, and
                // waited for before its buffer goes.
                // SAFETY: the read of `ov`.
                unsafe {
                    CancelIoEx(h, &ov);
                    GetOverlappedResult(h, &ov, &mut n, 1);
                }
                return;
            }
            // SAFETY: the read of `ov`, over.
            let ok = unsafe { GetOverlappedResult(h, &ov, &mut n, 0) };
            if stop.stopped() {
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
            // SAFETY: the read filled the first `n` bytes of `buf` (at most
            // its byte length), which isn't changed while `bytes` lives.
            let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), n as usize) };
            let mut off = 0usize;
            loop {
                // Stopped meanwhile: the rest is of the directory before (a
                // root replaced), not of the one at its path now.
                if stop.stopped() {
                    return;
                }
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
            let base = test_dirs::TestDir::new("ninja_daemon_short");
            let dir = strip_verbatim(std::fs::canonicalize(&*base).unwrap());
            let backup = dir.join("x.c.~1~");
            assert_eq!(long_path(&backup), Some(Named::Path(backup.clone())));
            assert_eq!(long_path(&dir.join("ABCDEF~1.H")), Some(Named::Under(dir.clone())));
            assert_eq!(long_path(&dir.join("GONE~1").join("x.h")), Some(Named::Under(dir.clone())));
        }

        /// A stopped reader ends, closing its directory's handle (which
        /// keeps the directories above it from being renamed), whenever it's
        /// stopped: while it waits for changes, or before it reads (it used
        /// to miss that, and kept the handle until a change).
        #[test]
        fn stopped_readers_let_go() {
            let base = test_dirs::TestDir::new("ninja_daemon_stopped");
            let w = Watcher::new(true, Box::new(|_| {}));
            let b = backend(Arc::downgrade(&w)).unwrap();
            for i in 0..20 {
                let (parent, moved) = (base.join(format!("p{i}")), base.join(format!("moved{i}")));
                let root = parent.join("root");
                std::fs::create_dir_all(&root).unwrap();
                b.watch(&root, &root, true).unwrap();
                // Stopped at once, or after a change it read.
                if i % 2 == 1 {
                    std::fs::write(root.join("f"), "f").unwrap();
                }
                b.unwatch(&root);
                let deadline = Instant::now() + Duration::from_secs(60);
                while let Err(e) = std::fs::rename(&parent, &moved) {
                    assert!(Instant::now() < deadline, "the reader of {} still holds it: {e}", root.display());
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;

    pub(super) fn backend(_sink: Weak<Watcher>) -> io::Result<Box<dyn Backend>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "file watching is not supported on this system"))
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
    use test_dirs::TestDir;

    #[test]
    fn local_file_systems() {
        assert_eq!(local_file_system(&test_dirs::base()), Ok(()));
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

    /// `base`'s real path, without `\\?\`: as events name it.
    fn real(base: &TestDir) -> PathBuf {
        strip_verbatim(std::fs::canonicalize(&**base).unwrap())
    }

    type Lines = Arc<Mutex<Vec<String>>>;
    type Log = Box<dyn Fn(&str) + Send + Sync>;

    /// A log kept for tests to read.
    fn kept_log() -> (Lines, Log) {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let l2 = lines.clone();
        (lines, Box::new(move |m| l2.lock().unwrap().push(m.to_string())))
    }

    /// A repository's files are vouched for once its root is in sync, and
    /// their changes reported.
    #[test]
    fn watches_repositories() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_watch");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(base.join("inc/sub")).unwrap();
        std::fs::create_dir_all(base.join("build")).unwrap();
        let dir = real(&base);
        let a = dir.join("inc/sub/a.h");
        std::fs::write(&a, "x").unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        w.ignore(&dir.join("build"), true);
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
        #[cfg(unix)]
        {
            let outside = TestDir::new("ninja_daemon_watch_out");
            std::os::unix::fs::symlink(&*outside, dir.join("link")).unwrap();
            assert_eq!(w.token(&dir.join("link/c.h")), None);
        }
        assert!(w.token(&dir.join("inc/b.h")).is_some());
    }

    /// A file with other names (hard links) isn't vouched for: changed
    /// through a name in another directory, or (inotify) through another
    /// one in its own, it isn't reported for this one.
    #[test]
    fn hard_links() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_links");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/src")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        let dir = real(&base);
        let (a, b) = (dir.join("repo/src/a.h"), dir.join("repo/src/b.h"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        std::fs::hard_link(&b, dir.join("elsewhere/b.h")).unwrap();
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        assert_eq!(w.token(&a), None);
        wait_for("the root in sync", || {
            w.token(&a);
            w.sync();
            w.token(&a).is_some()
        });
        assert_eq!(w.token(&b), None);
        std::fs::remove_file(dir.join("elsewhere/b.h")).unwrap();
        assert!(w.token(&b).is_some());
    }

    /// inotify: a repository's directories are watched as files in them
    /// are checked, with those above them, not the rest; a directory asked
    /// for whole (a toolchain in the checkout) with everything under it,
    /// directories created there since included.
    #[cfg(target_os = "linux")]
    #[test]
    fn watches_lazily() {
        let base = TestDir::new("ninja_daemon_lazy");
        for d in [".git", "src/a", "other/deep/er", "tc/bin/sub"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = real(&base);
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
    }

    /// A directory searched for headers (listed) is watched itself: a
    /// header created there later is seen, though no file of it was checked
    /// (it would shadow one found further down the search path).
    #[test]
    fn watches_listed_directories() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_listed");
        for d in [".git", "inc1", "inc2"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = real(&base);
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
        let base = TestDir::new("ninja_daemon_lost");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(base.join("d")).unwrap();
        let dir = real(&base);
        let f = dir.join("d/f.h");
        std::fs::write(&f, "v1").unwrap();
        let (lines, log) = kept_log();
        let w = Watcher::new(false, log);
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
    }

    /// Cookies stay out of the trees builds list: in a repository's own
    /// directory, else (inotify) in the daemon's; and a root found inside
    /// another's becomes part of it.
    #[test]
    fn cookie_dirs_and_nested_roots() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_cookies");
        for d in ["repo/.git", "repo/tc/bin", "tc2/bin"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = real(&base);
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
    }

    /// A build directory no build named for a while is forgotten: watched
    /// again (inotify: with the toolchain directory it's in), and its files
    /// vouched for.
    #[test]
    fn expires_build_dirs() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_expire");
        std::fs::create_dir_all(base.join("tc/out")).unwrap();
        let dir = real(&base);
        let (tc, f) = (dir.join("tc"), dir.join("tc/out/f.h"));
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        w.ignore(&tc.join("out"), true);
        w.watch_tree(&tc);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        w.sync();
        assert_eq!(w.token(&f), None);
        w.expire(Instant::now());
        assert!(w.token(&f).is_some());
        std::fs::write(&f, "f").unwrap();
        wait_for("its change", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == f));
    }

    /// Outputs in a repository's root (`compile_commands.json` copied
    /// there), or in the directory above checkouts: only that directory's
    /// own files and names are the builds', its roots stay watched (they
    /// were unwatched for a day, for every build); outputs in a directory
    /// of generated files make all of it the builds'.
    #[test]
    fn outputs_beside_roots() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_beside");
        for d in ["repo/.git", "repo/src", "repo/gen/sub", "other/.git", "other/src"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = real(&base);
        let (repo, other) = (dir.join("repo"), dir.join("other"));
        let (a, o, generated, copied) = (repo.join("src/a.h"), other.join("src/o.h"), repo.join("gen/sub/g.h"), repo.join("compile_commands.json"));
        for f in [&a, &o, &generated, &copied] {
            std::fs::write(f, "x").unwrap();
        }
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.set_cookie_dir(&dir.join("cookies"));
        let in_sync = |files: &[&PathBuf]| {
            for f in files {
                w.token(f);
            }
            w.sync();
            files.iter().all(|f| w.token(f).is_some())
        };
        wait_for("the roots in sync", || in_sync(&[&a, &o, &generated, &copied]));
        w.ignore_outputs(&repo);
        w.ignore_outputs(&dir);
        w.ignore_outputs(&repo.join("gen"));
        let mut ignored = w.ignored_dirs();
        ignored.sort();
        assert_eq!(ignored, vec![(dir.clone(), false), (repo.clone(), false), (repo.join("gen"), true)]);
        assert!(in_sync(&[&a, &o]), "{:?}", w.roots());
        assert_eq!(w.roots().len(), 2);
        // Their own files and names aren't vouched for, nor anything in a
        // directory of generated files; the names of a directory in one are.
        assert_eq!((w.token(&copied), w.token(&repo), w.token(&generated)), (None, None, None));
        assert!(w.token(&repo.join("src")).is_some());
        // Builds stop writing there.
        w.expire(Instant::now());
        assert!(w.ignored_dirs().is_empty());
        assert!(in_sync(&[&copied, &generated]));
    }

    /// Nothing in a repository's own directory is vouched for (`.git/index`,
    /// a ref a version stamp depends on): its events are ignored.
    #[test]
    fn repositories_own_files() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_vcs");
        std::fs::create_dir_all(base.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(base.join("src")).unwrap();
        let dir = real(&base);
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
    }

    /// A repository's directory created in a watched root: lookups of the
    /// repository there see it.
    #[test]
    fn repositories_created() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_newrepo");
        std::fs::create_dir_all(base.join("tc/sub")).unwrap();
        let dir = real(&base);
        let (tc, sub) = (dir.join("tc"), dir.join("tc/sub"));
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.watch_tree(&tc);
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        assert_ne!(w.repository(&sub), Some(sub.clone()));
        std::fs::create_dir(sub.join(".git")).unwrap();
        wait_for("the new repository", || w.repository(&sub) == Some(sub.clone()));
    }

    /// A root whose cookie can't be made (its repository's directory went)
    /// isn't in sync: it's watched again, its cookies made elsewhere.
    #[test]
    fn cookie_errors() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_cookie_err");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/src")).unwrap();
        let dir = real(&base);
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
    }

    /// A root whose directory is gone (a checkout removed) is forgotten,
    /// not tried again at every sync; found again when a file there is
    /// checked.
    #[test]
    fn roots_gone() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_root_gone");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/src")).unwrap();
        let dir = real(&base);
        let (repo, a) = (dir.join("repo"), dir.join("repo/src/a.h"));
        std::fs::write(&a, "a").unwrap();
        let (lines, log) = kept_log();
        let w = Watcher::new(cfg!(windows), log);
        w.set_cookie_dir(&dir.join("cookies"));
        wait_for("the root in sync", || {
            w.token(&a);
            w.sync();
            w.token(&a).is_some()
        });
        test_dirs::remove_all(&repo).unwrap();
        wait_for("the root forgotten", || {
            w.sync();
            w.roots().is_empty()
        });
        let watched = |l: &String| l.starts_with(&format!("watching {}", repo.display()));
        let starts = lines.lock().unwrap().iter().filter(|l| watched(l)).count();
        for _ in 0..3 {
            w.sync();
        }
        assert_eq!(lines.lock().unwrap().iter().filter(|l| watched(l)).count(), starts, "{:?}", lines.lock().unwrap());
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(&a, "a again").unwrap();
        wait_for("the root back in sync", || {
            w.token(&a);
            w.sync();
            w.token(&a).is_some()
        });
    }

    /// A burst of files created costs the same however many directories the
    /// watcher knows: a few lookups each, none going through what it learned
    /// elsewhere.
    #[test]
    fn event_cost() {
        let base = TestDir::new("ninja_daemon_cost");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let dir = real(&base);
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let sub = |i: usize| dir.join(format!("m{}", i % 50)).join(format!("d{i}"));
        for i in 0..4000 {
            w.real(&sub(i).join("f.h"));
            w.repository(&sub(i));
        }
        let before = w.swept.load(Ordering::Relaxed);
        for i in 0..10_000 {
            w.event(&sub(0).join(format!("new{i}.h")), true);
        }
        let cost = w.swept.load(Ordering::Relaxed) - before;
        assert!(cost <= 10 * 10_000, "{cost}");
        // A directory's own event still drops what was learned there.
        let k = w.key(&sub(1));
        assert!(w.real_dirs.read().unwrap().contains_key(&k) && w.repos.read().unwrap().contains_key(&k));
        w.event(&dir.join("m1"), true);
        assert!(!w.real_dirs.read().unwrap().contains_key(&k) && !w.repos.read().unwrap().contains_key(&k));
        assert!(w.repos.read().unwrap().contains_key(&w.key(&sub(2))));
    }

    /// Removing directories the watcher knows (a branch switch, a
    /// dependency updated): their events drop what was under them, without
    /// going through what else the daemon keeps. Every directory removed
    /// used to go through all of it (minutes for thousands, while builds
    /// timed out waiting for their sync).
    #[test]
    fn removal_cost() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_removal");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let dir = real(&base);
        let ci = cfg!(windows);
        let w = Watcher::new(ci, Box::new(|_| {}));
        let files = Arc::new(crate::files::Files::new(w.clone(), ci));
        w.listen(files.clone());
        let driver = Arc::new(cpp_deps::Driver::new(cpp_deps::Cache::new(vec![], ci)));
        driver.cache.set_watch(w.clone());
        w.listen(driver.clone());
        let n = 1000;
        let file = |i: usize| dir.join(format!("m{}", i % 20)).join(format!("d{i}")).join("f.h");
        for i in 0..n {
            std::fs::create_dir_all(file(i).parent().unwrap()).unwrap();
            std::fs::write(file(i), "#define X 1\n").unwrap();
        }
        assert!(files.get(&file(0)).is_ok());
        wait_for("the watch", || w.roots().iter().any(|(_, s)| *s == RootState::Unsynced));
        for _ in 0..2 {
            w.sync();
            files.new_session();
            driver.cache.new_session();
            for i in 0..n {
                assert!(files.get(&file(i)).is_ok());
                assert!(driver.cache.directives(&file(i)).is_some() && driver.cache.exists(&file(i)));
            }
        }
        assert!((0..n).all(|i| w.token(&file(i)).is_some()));
        // A fifth removed, as backends report it: each file, then its
        // directory.
        let before = w.swept.load(Ordering::Relaxed);
        for i in 0..n / 5 {
            w.event(&file(i), true);
            w.event(file(i).parent().unwrap(), true);
        }
        let cost = w.swept.load(Ordering::Relaxed) - before;
        // What each held, and a few lookups per event.
        assert!(cost < 30 * (n / 5) as u64, "{cost}");
        assert_eq!((files.len().0, driver.cache.len().0), (n - n / 5, n - n / 5));
        assert!(w.token(&file(n - 1)).is_some());
    }

    /// inotify: nothing in a build directory is watched or vouched for, not
    /// even a toolchain asked for whole (it would be trusted unwatched, its
    /// cookies waited for in vain); once it expires, it's watched again and
    /// its changes seen.
    #[cfg(target_os = "linux")]
    #[test]
    fn build_dirs_unwatched() {
        let base = TestDir::new("ninja_daemon_unwatched");
        for d in ["repo/.git", "repo/src", "repo/build/_deps/tc/bin", "out/tc/.git", "out/tc/bin"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = real(&base);
        let (repo, tc1, tc2) = (dir.join("repo"), dir.join("repo/build/_deps/tc"), dir.join("out/tc"));
        let a = repo.join("src/a.h");
        std::fs::write(&a, "a").unwrap();
        let (lines, log) = kept_log();
        let w = Watcher::new(false, log);
        w.set_cookie_dir(&dir.join("cookies"));
        let rec = Arc::new(Recorder::default());
        w.listen(rec.clone());
        w.ignore(&repo.join("build"), true);
        w.ignore(&dir.join("out"), true);
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
        w.ignore(&tc2.join("out"), true);
        std::fs::create_dir(tc2.join("out")).unwrap();
        w.sync();
        assert!(w.roots().iter().all(|(_, s)| *s == RootState::Trusted), "{:?}", lines.lock().unwrap());
    }

    /// Windows: a root replaced (a fresh clone where the old one was) is
    /// watched again, from its new directory, as soon as a sync sees its
    /// directory's identity changed (not after its cookie, which can't
    /// come through the old directory's watch, timed out).
    #[cfg(windows)]
    #[test]
    fn replaced_roots() {
        let base = TestDir::new("ninja_daemon_replaced");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("repo/src")).unwrap();
        let dir = real(&base);
        let (repo, a) = (dir.join("repo"), dir.join("repo/src/a.h"));
        std::fs::write(&a, "a").unwrap();
        let (lines, log) = kept_log();
        let w = Watcher::new(true, log);
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
        let lines = lines.lock().unwrap().clone();
        assert!(lines.iter().any(|l| l.contains("replaced")) && !lines.iter().any(|l| l.contains("no event in")), "{lines:?}");
        rec.0.lock().unwrap().clear();
        std::fs::write(&a, "changed").unwrap();
        wait_for("its change", || rec.0.lock().unwrap().iter().any(|(p, _)| w.key(p) == w.key(&a)));
    }
}
