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
//!   looked up by hash, and indexed by path (`PathMap`), so that dropping
//!   what's under a directory costs what it held, not what the daemon
//!   knows (a branch switch removing thousands of them), and a lookup
//!   doesn't cost more with what it knows.
//! - **Paths.** Only paths without symbolic links are vouched for: events
//!   come for real paths (a root reached through one says so, rather than
//!   being watched for nothing). Nor are files with other names (hard
//!   links, whose changes through another name aren't reported here; a
//!   tree whose walk saw them all in it is, see `Files::tree`), files in a
//!   directory ignoring case (Linux's casefold: events give the names on
//!   disk), paths in a repository's own directory, whose events are
//!   ignored (git writes there all the time), nor, on Linux, on file
//!   systems under a root that may not report every change. A hard link
//!   made to a file vouched for, and a change through it, go unseen (no
//!   event names the file): package managers replace files rather than
//!   link to them.
//!
//! Backends: inotify on Linux (the build directories aren't watched, so
//!   that compiles writing objects don't flood the queue),
//!   `ReadDirectoryChangesW` on Windows (whose events in build directories
//!   are dropped before anything else is done with them). Elsewhere
//!   nothing is watched. One that can't be set up (out of inotify
//!   instances) is tried again at the next sync.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

pub use cpp_deps::path_map::{PathMap, PathSet};
pub use cpp_deps::scan::Change;

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
    /// Its latest set-up (`Watcher::starts`): one started before (a sync
    /// and an event both restarting it) doesn't set its state.
    start: u64,
}

impl Root {
    /// Marks it starting: the set-up `start` gets to do.
    fn starting(&mut self, start: &AtomicU64) -> u64 {
        self.state = RootState::Starting;
        self.start = start.fetch_add(1, Ordering::Relaxed) + 1;
        self.start
    }
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

/// A lookup in progress (see `Watcher::look`), until dropped.
struct Looking<'a> {
    w: &'a Watcher,
    key: PathBuf,
    forgotten: u64,
}

impl Looking<'_> {
    /// Whether what it found may be recorded: nothing under a lookup in
    /// progress was forgotten since it started.
    fn current(&self) -> bool {
        self.w.forgotten.load(Ordering::SeqCst) == self.forgotten
    }
}

impl Drop for Looking<'_> {
    fn drop(&mut self) {
        let mut looking = self.w.looking.lock().unwrap();
        if let Some(i) = looking.iter().position(|k| *k == self.key) {
            looking.swap_remove(i);
        }
    }
}

/// Linux: whether `dir` compares its names ignoring case (`chattr +F`).
#[cfg(target_os = "linux")]
fn folds_case(dir: &Path) -> bool {
    use std::os::fd::AsRawFd;
    const FS_CASEFOLD_FL: libc::c_int = 0x4000_0000;
    let Ok(d) = std::fs::File::open(dir) else { return false };
    let mut flags: libc::c_int = 0;
    // SAFETY: an open descriptor, and the int the call fills.
    let ok = unsafe { libc::ioctl(d.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } == 0;
    ok && flags & FS_CASEFOLD_FL != 0
}

/// Names are compared as the watcher keys them elsewhere.
#[cfg(not(target_os = "linux"))]
fn folds_case(_dir: &Path) -> bool {
    false
}

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

/// A directory builds write to.
struct BuildDir {
    path: PathBuf,
    /// When a build last named it.
    named: Instant,
    /// Everything under it is theirs, else only its own files (see
    /// `Watcher::ignore`).
    whole: bool,
}

/// Whether a path may change during builds (see `BuildDirs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Volatile {
    No,
    Yes,
    /// Under a directory whose own files only are the builds' (one holding
    /// roots), not its own: unless a root in sync watches it (whose events
    /// say when builds write there), as builds without the daemon see it.
    UnlessWatched,
}

/// Directories builds named (keys). A path is in one when one of its
/// ancestors is: a lookup per ancestor, however many there are (CMake's
/// `try_compile` names hundreds a day).
#[derive(Default)]
pub(crate) struct BuildDirs {
    dirs: RwLock<HashMap<PathBuf, BuildDir>>,
}

#[cfg(test)]
thread_local! {
    /// Lookups of build directories this thread made (what a check costs;
    /// not events' handling, in the threads reading them).
    static PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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

    /// Yes when `key` is in one whole, or is `own` and one of those whose
    /// own files only are the builds'; `UnlessWatched` when it's under one
    /// of those otherwise.
    fn find(&self, key: &Path, own: Option<&Path>) -> Volatile {
        let dirs = self.dirs.read().unwrap();
        if dirs.is_empty() {
            return Volatile::No;
        }
        let mut found = Volatile::No;
        for a in key.ancestors() {
            #[cfg(test)]
            PROBES.with(|p| p.set(p.get() + 1));
            match dirs.get(a) {
                Some(d) if d.whole || Some(a) == own => return Volatile::Yes,
                Some(_) => found = Volatile::UnlessWatched,
                None => {}
            }
        }
        found
    }

    /// Whether `key` is in one whole: nothing there is watched.
    pub(crate) fn covers(&self, key: &Path) -> bool {
        self.find(key, None) == Volatile::Yes
    }

    /// Whether the file `key` may change during builds.
    pub(crate) fn has_file(&self, key: &Path) -> Volatile {
        self.find(key, key.parent())
    }

    /// Whether the directory `key`'s names may change during builds.
    pub(crate) fn has_dir(&self, key: &Path) -> Volatile {
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
    /// removal drops everything under them. As the maps below, looked up by
    /// hash, and what's under a path goes without going through the rest
    /// (see `PathMap`).
    known_dirs: RwLock<PathSet>,
    /// Directories (keys) and the repository each is in.
    repos: RwLock<PathMap<Option<PathBuf>>>,
    /// Directories (keys) and whether their path is their real one.
    real_dirs: RwLock<PathMap<bool>>,
    /// Lookups in progress: the keys under which they'll record what they
    /// find (see `look`).
    looking: Mutex<Vec<PathBuf>>,
    /// Bumped whenever entries of `repos`, `real_dirs` or `watched` go
    /// under a lookup in progress: what it found may be stale, and isn't
    /// kept. (Only then: every event bumping it dropped what lookups found
    /// elsewhere, and a watch not recorded was set up again, vouched for a
    /// sync later.)
    forgotten: AtomicU64,
    /// Bumped by every sync, which forgets that directories are in no
    /// repository: a lookup that started before doesn't record that.
    purged: AtomicU64,
    /// Syncs started: a directory watched (lazily) during the `n`th is
    /// vouched for once a later one brought its root in sync.
    epoch: AtomicU64,
    /// Roots' set-ups started (see `Root::start`).
    starts: AtomicU64,
    /// Roots' set-ups (keys; as many as roots ever were): a root's watch is
    /// set up, and given back, one at a time, so that a root gone or
    /// watched again during its set-up leaves no watch behind (a Windows
    /// reader holding its directory for the daemon's life).
    setups: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Directories watched lazily (keys): the epoch they were, and whether
    /// with everything under them.
    watched: RwLock<PathMap<(u64, bool)>>,
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
            looking: Mutex::default(),
            forgotten: AtomicU64::new(0),
            purged: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            starts: AtomicU64::new(0),
            setups: Mutex::default(),
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

    /// Lookups of build directories this thread made.
    #[cfg(test)]
    pub(crate) fn probes() -> u64 {
        PROBES.with(|p| p.get())
    }

    /// Forgets the lazy watches of `key` and under it.
    fn forget_watched(&self, key: &Path) {
        self.forgetting(key);
        let n = self.watched.write().unwrap().remove_under(key);
        self.sweep(n + 1);
    }

    /// Starts a lookup that records what it finds under `key` (and above):
    /// see `forgetting`.
    fn look(&self, key: &Path) -> Looking<'_> {
        let mut looking = self.looking.lock().unwrap();
        looking.push(key.to_path_buf());
        Looking { w: self, key: key.to_path_buf(), forgotten: self.forgotten.load(Ordering::SeqCst) }
    }

    /// Before what's known under `key` goes: lookups in progress there
    /// (which may have seen it, and would put it back) don't record what
    /// they found. One starting later finds what's there after the change.
    fn forgetting(&self, key: &Path) {
        if self.looking.lock().unwrap().iter().any(|k| k.starts_with(key)) {
            self.forgotten.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Forgets what was learned about `key` and the directories under it,
    /// and their lazy watches (a directory created there again is watched
    /// again).
    fn forget(&self, key: &Path) {
        // Before anything goes: a lookup that started earlier then doesn't
        // put it back.
        self.forgetting(key);
        let mut n = 0;
        if self.repos.read().unwrap().has_under(key) {
            n += self.repos.write().unwrap().remove_under(key);
        }
        if self.real_dirs.read().unwrap().has_under(key) {
            n += self.real_dirs.write().unwrap().remove_under(key);
        }
        if self.watched.read().unwrap().has_under(key) {
            n += self.watched.write().unwrap().remove_under(key);
        }
        self.sweep(n + 3);
    }

    /// Whether something under `key` is known: vouched for, watched or
    /// learned.
    fn known(&self, key: &Path) -> bool {
        self.known_dirs.read().unwrap().contains(key)
            || self.watched.read().unwrap().has_under(key)
            || self.real_dirs.read().unwrap().has_under(key)
            || self.repos.read().unwrap().has_under(key)
    }

    /// Records `key` as watched lazily since `epoch` (with everything under
    /// it, `whole`), unless it was forgotten since the lookup that watched
    /// it started (`looking`): the directory may be gone, and its watch
    /// with it.
    fn note_watched(&self, key: &Path, epoch: u64, whole: bool, looking: &Looking) -> bool {
        let mut watched = self.watched.write().unwrap();
        if !looking.current() {
            return false;
        }
        watched.insert(key, (epoch, whole));
        true
    }

    /// A directory builds write to: their build directory, or another one
    /// (an output outside it). Its events are ignored, its files never
    /// vouched for (those that were are checked again), and roots in it go
    /// (watched again once it expires). Not `whole`: its own files and names
    /// (it holds a root, see `ignore_outputs`); the rest is checked every
    /// time too, but for what roots in sync watch there.
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
        }
        for r in &gone {
            self.unwatch_root(&r.key, &r.path);
        }
        self.forget_watched(&key);
    }

    /// A directory a build writes outputs to, outside its build directory:
    /// everything under it is the build's (generated files, in directories
    /// of their own), unless it holds a root: then its own files, and what
    /// no root in sync watches there (a build writes outputs it doesn't
    /// name too), lest a file copied to a repository's root
    /// (`compile_commands.json`) or to the directory above checkouts leaves
    /// them unwatched. It holds one when it's a repository's root, isn't in
    /// any repository (checkouts may be found under it later), or a root is
    /// under it.
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
        self.unless_watched(key, self.ignored.has_file(key))
    }

    /// Whether the directory `key` or what's under it may change during
    /// builds (see `ignore`).
    pub(crate) fn volatile_dir(&self, key: &Path) -> bool {
        self.unless_watched(key, self.ignored.has_dir(key))
    }

    /// Whether `v` makes `key` volatile: `UnlessWatched` unless a root in
    /// sync watches it (on a file system reporting every change).
    fn unless_watched(&self, key: &Path, v: Volatile) -> bool {
        match v {
            Volatile::No => false,
            Volatile::Yes => true,
            Volatile::UnlessWatched => !self
                .roots
                .read()
                .unwrap()
                .iter()
                .any(|r| r.state == RootState::Trusted && key.starts_with(&r.key) && !r.mounts.iter().any(|(m, local)| !local && key.starts_with(m))),
        }
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
        let start;
        let nested: Vec<Root> = {
            let mut roots = self.roots.write().unwrap();
            if roots.iter().any(|r| key.starts_with(&r.key)) {
                return;
            }
            let (nested, kept): (Vec<Root>, Vec<Root>) = std::mem::take(&mut *roots).into_iter().partition(|r| r.key.starts_with(&key));
            *roots = kept;
            let mut root = Root {
                path: dir.to_path_buf(),
                key: key.clone(),
                state: RootState::Starting,
                cookie_dir: dir.to_path_buf(),
                whole,
                synced: 0,
                retry_after: None,
                id: None,
                mounts: Vec::new(),
                start: 0,
            };
            start = root.starting(&self.starts);
            roots.push(root);
            for r in &nested {
                self.drop_under(&r.path);
            }
            nested
        };
        for r in nested {
            self.unwatch_root(&r.key, &r.path);
            self.forget_watched(&r.key);
        }
        self.start(dir.to_path_buf(), key, whole, start);
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

    /// Sets up a root's watch (its set-up `start`), in the background:
    /// where its cookies go, what its directory is and what's mounted in it
    /// are found again. One whose directory is gone is forgotten; one that
    /// fails is tried again a while later. A set-up superseded (the root
    /// restarted meanwhile) leaves it to the later one.
    fn start(&self, dir: PathBuf, key: PathBuf, whole: bool, start: u64) {
        let me = self.me.clone();
        std::thread::spawn(move || {
            let Some(w) = me.upgrade() else { return };
            if std::fs::metadata(&dir).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
                w.forget_root(&key, &dir, start);
                return;
            }
            let t = Instant::now();
            // One at a time, and none once the root went or was watched
            // again meanwhile (see `setups`).
            let lock = w.setup_lock(&key);
            let setup = lock.lock().unwrap();
            if !w.roots.read().unwrap().iter().any(|r| r.key == key && r.start == start) {
                return;
            }
            // Set up first: it reads what's mounted.
            let backend = w.set_up(false);
            // Watched from before the next sync, and found before it's
            // watched: a directory there since isn't the one watched.
            let epoch = w.epoch.load(Ordering::SeqCst);
            let looking = w.look(&key);
            let (cookie_dir, id, mounts) = (w.cookie_dir_of(&dir), dir_id(&dir), w.mounts_in(&key));
            {
                let mut roots = w.roots.write().unwrap();
                let Some(r) = roots.iter_mut().find(|r| r.key == key && r.start == start) else { return };
                (r.cookie_dir, r.id, r.mounts) = (cookie_dir.clone(), id, mounts);
            }
            // Events name real paths: nothing under a root reached through
            // a link would ever be vouched for (it's said, not watched).
            let linked = std::fs::canonicalize(&dir).map(strip_verbatim).ok().filter(|real| w.key(real) != key);
            // A failure is tried again a while later; without a backend,
            // when a sync sets one up.
            let (state, out_of_watches, later) = match (backend, local_file_system(&dir), linked) {
                (Err(why), ..) => (RootState::Failed(why), None, false),
                (_, Err(why), _) => (RootState::Failed(why), None, true),
                (_, _, Some(real)) => (RootState::Failed(format!("its real path is {}: paths through links aren't vouched for", real.display())), None, true),
                (Ok(b), Ok(()), None) => match b.watch(&dir, &cookie_dir, whole) {
                    Ok(()) => (RootState::Unsynced, None, false),
                    Err(e) if e.kind() == io::ErrorKind::StorageFull => (RootState::Failed(e.to_string()), Some(e), false),
                    Err(e) => (RootState::Failed(e.to_string()), None, true),
                },
            };
            if state == RootState::Unsynced && w.lazy() && !whole {
                w.note_watched(&key, epoch, false, &looking);
            }
            drop(looking);
            {
                let mut roots = w.roots.write().unwrap();
                let Some(r) = roots.iter_mut().find(|r| r.key == key && r.start == start && r.state == RootState::Starting) else { return };
                r.retry_after = later.then(|| Instant::now() + RETRY_FAILED);
                r.state = state.clone();
            }
            drop(setup);
            (w.log)(&format!("watching {} ({:.0?}): {state:?}", dir.display(), t.elapsed()));
            if let Some(e) = out_of_watches {
                w.release(&key, &e);
            }
            // Mounted or unmounted meanwhile.
            w.check_mounts(&key);
        });
    }

    /// A root whose directory is gone (a checkout removed): forgotten,
    /// found again when a file there is checked. What was kept for it goes,
    /// digests included (its own files' too, under no directory known).
    fn forget_root(&self, key: &Path, dir: &Path, start: u64) {
        {
            let mut roots = self.roots.write().unwrap();
            let before = roots.len();
            roots.retain(|r| !(r.key == key && r.start == start && r.state == RootState::Starting));
            if roots.len() == before {
                return;
            }
            self.seq.fetch_add(1, Ordering::SeqCst);
            for l in self.listeners.read().unwrap().iter() {
                l.forget(dir);
            }
            self.drop_under(dir);
        }
        (self.log)(&format!("{}: gone: no longer watched", dir.display()));
        self.unwatch_root(key, dir);
        self.forget_watched(key);
    }

    /// The lock of the root `key`'s set-ups (see `setups`).
    fn setup_lock(&self, key: &Path) -> Arc<Mutex<()>> {
        self.setups.lock().unwrap().entry(key.to_path_buf()).or_default().clone()
    }

    /// Stops watching the root `key` (at `path`): once a set-up of it in
    /// progress is done, whose watch would be left otherwise.
    fn unwatch_root(&self, key: &Path, path: &Path) {
        let lock = self.setup_lock(key);
        let _setup = lock.lock().unwrap();
        if let Some(b) = self.backend() {
            b.unwatch(path);
        }
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
        let (path, whole, start) = {
            let mut roots = self.roots.write().unwrap();
            let Some(r) = roots.iter_mut().find(|r| r.key == key && cond(r)) else { return };
            let start = r.starting(&self.starts);
            self.drop_under(&r.path);
            (r.path.clone(), r.whole, start)
        };
        (self.log)(&format!("{}: {why}: watching it again", path.display()));
        self.unwatch_root(key, &path);
        self.forget_watched(key);
        self.start(path, key.to_path_buf(), whole, start);
    }

    /// `restart`s a root that's watched (not one being set up again
    /// already, by another sync that saw the same).
    fn restart_watched(&self, key: &Path, why: &str) {
        self.restart_if(key, why, |r| matches!(r.state, RootState::Unsynced | RootState::Trusted));
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
            let key = self.key(&d);
            let looking = self.look(&key);
            match b.watch_dir(&d, whole) {
                Ok(()) => {
                    self.note_watched(&key, epoch, whole, &looking);
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
        self.unwatch_root(key, &path);
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
        let key = self.key(dir);
        if let Some(r) = self.repos.read().unwrap().get(&key) {
            return r.clone();
        }
        let (looking, purged) = (self.look(&key), self.purged.load(Ordering::SeqCst));
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
        if looking.current() && self.purged.load(Ordering::SeqCst) == purged {
            for k in walked {
                repos.insert(k, found.clone());
            }
        }
        found
    }

    /// What `path` is (one missing, a file), if its directory names itself
    /// (no symbolic link in it).
    fn real(&self, path: &Path) -> Option<EntryKind> {
        let dir = path.parent()?;
        let dkey = self.key(dir);
        let cached = self.real_dirs.read().unwrap().get(&dkey).copied();
        let real_dir = cached.unwrap_or_else(|| {
            let looking = self.look(&dkey);
            // Names in a directory ignoring case (Linux's casefold) are
            // keyed as checked, events come with the names on disk.
            let real = std::fs::canonicalize(dir).is_ok_and(|c| self.key(&strip_verbatim(c)) == dkey) && !folds_case(dir);
            let mut real_dirs = self.real_dirs.write().unwrap();
            if looking.current() {
                real_dirs.insert(dkey.clone(), real);
            }
            real
        });
        if !real_dir {
            return None;
        }
        Some(entry_kind(path).unwrap_or(EntryKind { link: false, dir: false, links: 1 }))
    }

    /// Remembers the directories above `key` up to its root's.
    fn remember_dirs(&self, key: &Path, root: &Path) {
        if key.parent().is_some_and(|p| self.known_dirs.read().unwrap().contains(p)) {
            return;
        }
        let mut known = self.known_dirs.write().unwrap();
        let mut cur = key.parent();
        while let Some(d) = cur {
            if !d.starts_with(root) || !known.add(d) {
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
        self.purged.fetch_add(1, Ordering::SeqCst);
        self.repos.write().unwrap().retain(|_, r| r.is_some());
        let now = Instant::now();
        let failed: Vec<(PathBuf, PathBuf, bool, u64)> = {
            let mut roots = self.roots.write().unwrap();
            roots
                .iter_mut()
                .filter(|r| matches!(r.state, RootState::Failed(_)) && r.retry_after.is_none_or(|t| now >= t))
                .map(|r| {
                    r.retry_after = None;
                    (r.path.clone(), r.key.clone(), r.whole, r.starting(&self.starts))
                })
                .collect()
        };
        for (path, key, whole, start) in failed {
            self.start(path, key, whole, start);
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
        self.restart_replaced(&ids);
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
                        self.restart_watched(&key, &format!("making its cookie: {e}"));
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
                self.restart_watched(&key, &format!("no event in {COOKIE_TIMEOUT:?}"));
            }
        }
    }

    /// Roots whose directory (`ids`, as a sync found them) isn't the one
    /// watched any more: watched again, once (concurrent syncs see the
    /// same; a root restarted twice had two readers, one of which kept its
    /// directory open for the daemon's life).
    fn restart_replaced(&self, ids: &[(PathBuf, PathBuf, DirId)]) {
        for (key, path, id) in ids {
            if dir_id(path) != Some(*id) {
                self.restart_if(key, "replaced", |r| r.id == Some(*id) && matches!(r.state, RootState::Unsynced | RootState::Trusted));
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
        let n = self.known_dirs.write().unwrap().remove_under(&self.key(dir));
        self.sweep(n + 1);
    }

    /// A backend lost events (of the root `under` is in, or of every
    /// root). With a watch per directory, those of directories replaced
    /// meanwhile went too, unseen: the roots are watched again from
    /// scratch, vouched for after the next sync.
    fn lost(&self, under: Option<&Path>) {
        let ukey = under.map(|p| self.key(p));
        let lazy = self.lazy();
        let mut hit: Vec<(PathBuf, PathBuf, bool, u64)> = Vec::new();
        {
            let mut roots = self.roots.write().unwrap();
            for r in roots.iter_mut().filter(|r| ukey.as_ref().is_none_or(|k| k.starts_with(&r.key))) {
                if lazy && !matches!(r.state, RootState::Failed(_)) {
                    hit.push((r.key.clone(), r.path.clone(), r.whole, r.starting(&self.starts)));
                } else if r.state == RootState::Trusted {
                    r.state = RootState::Unsynced;
                }
                self.drop_under(&r.path);
            }
        }
        (self.log)(&format!("events lost ({}): checking files again", under.map_or("every root".to_string(), |r| r.display().to_string())));
        for (key, path, whole, start) in hit {
            self.forget_watched(&key);
            self.start(path, key, whole, start);
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
        if self.ignored.covers(&key) || (self.ignored.has_file(&key) == Volatile::Yes && !self.known(&key)) {
            return;
        }
        if in_repository_dir(&key) {
            // Nothing there is vouched for. A repository's directory made
            // or removed: lookups of the repository beside it change.
            if structural
                && key.file_name().is_some_and(|n| VCS_DIRS.iter().any(|v| n == *v))
                && let Some(parent) = key.parent()
            {
                self.forgetting(parent);
                let n = self.repos.write().unwrap().remove_under(parent);
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
            let n = self.known_dirs.write().unwrap().remove_under(&key);
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
        self.vouch_names(path, tree, false).map(|(token, _)| token)
    }

    /// For a tree's walk: a token for checking `path`, a file in the tree,
    /// but for its other names (a symbolic link's target, hard links),
    /// which the walk sees: in the tree (watched whole), a change through
    /// any is reported under it. With its number of names (a link's own).
    pub(crate) fn walked_token(&self, path: &Path) -> Option<(u64, u64)> {
        self.vouch_names(path, false, true)
    }

    /// `vouch`, with the number of names `path` has; `walked`: one with
    /// others isn't refused for them.
    fn vouch_names(&self, path: &Path, tree: bool, walked: bool) -> Option<(u64, u64)> {
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
                let entry = self.real(path)?;
                // Changed through another name (a link's target, a hard
                // link in another directory, or on Linux in this one), it
                // isn't reported for this one.
                let names = if entry.link { 1 } else { entry.links };
                if !walked && (entry.link || (!entry.dir && names > 1)) {
                    return None;
                }
                let is_dir = entry.dir;
                // Written by builds beside the root's files (under them,
                // watched: their events say).
                let volatile = if is_dir { self.ignored.has_dir(&key) } else { self.ignored.has_file(&key) };
                if volatile == Volatile::Yes {
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
                Some((seq, names))
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
#[path = "linux.rs"]
mod platform;

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;

    pub(super) fn backend(_sink: Weak<Watcher>) -> io::Result<Box<dyn Backend>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "file watching is not supported on this system"))
    }

    /// Nothing is watched: any will do.
    pub(super) fn local_file_system(_dir: &Path) -> Result<(), String> {
        Ok(())
    }
}

use platform::local_file_system;

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
        assert!(!w.watched.read().unwrap().keys().any(|k| k.starts_with(&tc1)), "{:?}", w.watched.read().unwrap());
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

    /// Windows: two syncs at once find a root replaced: it's watched again
    /// once (both did, and the two set-ups raced), and once builds write
    /// there (its root goes, its reader stopped), nothing holds its
    /// directory (a reader left behind kept it, and those above it, from
    /// being renamed for the daemon's life).
    #[cfg(windows)]
    #[test]
    fn concurrent_syncs() {
        for round in 0..10 {
            let base = TestDir::new(&format!("ninja_daemon_syncs{round}"));
            std::fs::create_dir_all(base.join("p/repo/.git")).unwrap();
            std::fs::create_dir_all(base.join("p/repo/src")).unwrap();
            let dir = real(&base);
            let (parent, repo) = (dir.join("p"), dir.join("p/repo"));
            let a = repo.join("src/a.h");
            std::fs::write(&a, "a").unwrap();
            let (lines, log) = kept_log();
            let w = Watcher::new(true, log);
            w.set_cookie_dir(&dir.join("cookies"));
            wait_for("the root in sync", || {
                w.token(&a);
                w.sync();
                w.token(&a).is_some()
            });
            std::fs::rename(&repo, parent.join("repo.old")).unwrap();
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            let both = Arc::new(std::sync::Barrier::new(2));
            let syncs: Vec<_> = (0..2)
                .map(|_| {
                    let (w, both) = (w.clone(), both.clone());
                    std::thread::spawn(move || {
                        both.wait();
                        w.sync();
                    })
                })
                .collect();
            for s in syncs {
                s.join().unwrap();
            }
            wait_for("the set-up", || w.roots().iter().all(|(_, s)| *s != RootState::Starting));
            let lines = lines.lock().unwrap().clone();
            assert_eq!(lines.iter().filter(|l| l.ends_with(": replaced: watching it again")).count(), 1, "{lines:?}");
            w.ignore(&dir, true);
            let deadline = Instant::now() + Duration::from_secs(60);
            while let Err(e) = std::fs::rename(&parent, dir.join("moved")) {
                assert!(Instant::now() < deadline, "a reader still holds {}: {e}", repo.display());
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// Syncs at once seeing a root replaced (as they found its directory)
    /// watch it again once.
    #[cfg(windows)]
    #[test]
    fn replaced_once() {
        let base = TestDir::new("ninja_daemon_once");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        let dir = real(&base);
        let repo = dir.join("repo");
        let (lines, log) = kept_log();
        let w = Watcher::new(true, log);
        w.add_root(&repo, false);
        wait_for("the watch", || w.roots().iter().all(|(_, s)| *s == RootState::Unsynced));
        let ids: Vec<(PathBuf, PathBuf, DirId)> = w.roots.read().unwrap().iter().filter_map(|r| Some((r.key.clone(), r.path.clone(), r.id?))).collect();
        std::fs::rename(&repo, dir.join("repo.old")).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        w.restart_replaced(&ids);
        w.restart_replaced(&ids);
        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines.iter().filter(|l| l.ends_with(": replaced: watching it again")).count(), 1, "{lines:?}");
    }

    /// A set-up superseded (the root watched again meanwhile) leaves the
    /// root to the later one: it doesn't forget it (its directory was gone
    /// when it looked), nor set its state.
    #[test]
    fn superseded_set_ups() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_superseded");
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        let dir = real(&base);
        let (repo, key) = (dir.join("repo"), w_key(&dir.join("repo")));
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.set_cookie_dir(&dir.join("cookies"));
        w.add_root(&repo, false);
        wait_for("the watch", || w.roots().iter().all(|(_, s)| *s == RootState::Unsynced));
        let first = w.roots.read().unwrap()[0].start;
        w.restart(&key, "a test");
        w.forget_root(&key, &repo, first);
        assert_eq!(w.roots().len(), 1);
        wait_for("the later set-up", || w.roots().iter().all(|(_, s)| *s == RootState::Unsynced));
        w.sync();
        assert_eq!(w.roots(), vec![(repo, RootState::Trusted)]);
    }

    /// A path as a watcher of this system's names keys it.
    fn w_key(p: &Path) -> PathBuf {
        key(p, cfg!(windows))
    }

    /// What a lookup finds is recorded unless what it looked at was
    /// forgotten meanwhile: events elsewhere, and syncs, don't drop it
    /// (every structural event did, and a lazy watch set up meanwhile went
    /// unrecorded: it was set up again, and vouched for a sync later).
    #[test]
    fn lookups_kept() {
        let base = TestDir::new("ninja_daemon_lookups");
        let dir = real(&base);
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        let looking = w.look(&w_key(&dir.join("a/x")));
        w.forget(&w_key(&dir.join("b")));
        w.event(&dir.join("b/new.h"), true);
        w.sync();
        assert!(looking.current());
        w.event(&dir.join("a"), true);
        assert!(!looking.current());
    }

    /// Makes `link` a link to the directory `target`: a symbolic link on
    /// Unix, a junction on Windows (which needs no privilege).
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let backslashes = |p: &Path| p.to_string_lossy().replace('/', "\\");
            let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J", &backslashes(link), &backslashes(target)]).output().unwrap();
            assert!(made.status.success(), "{made:?}");
        }
    }

    /// A checkout reached through a link isn't watched for nothing (events
    /// come for real paths: nothing there was ever vouched for, silently):
    /// its root says why.
    #[test]
    fn linked_roots() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        let base = TestDir::new("ninja_daemon_linked");
        std::fs::create_dir_all(base.join("real/repo/.git")).unwrap();
        std::fs::create_dir_all(base.join("real/repo/src")).unwrap();
        let dir = real(&base);
        link_dir(&dir.join("real"), &dir.join("link"));
        std::fs::write(dir.join("real/repo/src/a.h"), "a").unwrap();
        let a = dir.join("link/repo/src/a.h");
        let w = Watcher::new(cfg!(windows), Box::new(|_| {}));
        w.set_cookie_dir(&dir.join("cookies"));
        assert_eq!(w.token(&a), None);
        wait_for("the watch", || w.roots().iter().all(|(_, s)| *s != RootState::Starting));
        let roots = w.roots();
        assert!(matches!(&roots[..], [(p, RootState::Failed(why))] if *p == dir.join("link/repo") && why.contains("real path")), "{roots:?}");
    }

    /// Linux: files in a directory ignoring case (`chattr +F`) aren't
    /// vouched for: they're keyed as checked, and events come with the
    /// names on disk (a change to `foo.h` didn't drop `Foo.h`).
    #[cfg(target_os = "linux")]
    #[test]
    fn case_folding_dirs() {
        let base = TestDir::new("ninja_daemon_casefold");
        std::fs::create_dir_all(base.join(".git")).unwrap();
        std::fs::create_dir_all(base.join("folded")).unwrap();
        std::fs::create_dir_all(base.join("plain")).unwrap();
        let dir = real(&base);
        let folded = std::process::Command::new("chattr").arg("+F").arg(dir.join("folded")).output().is_ok_and(|o| o.status.success());
        if !folded {
            // Past libtest's capture, which shows nothing of a test that
            // passed.
            let said = "NOT TESTED: watch::tests::case_folding_dirs: no case-folding file system here (chattr +F)\n";
            let _ = std::io::Write::write_all(&mut std::io::stderr(), said.as_bytes());
            return;
        }
        let (f, p) = (dir.join("folded/foo.h"), dir.join("plain/foo.h"));
        std::fs::write(&f, "f").unwrap();
        std::fs::write(&p, "p").unwrap();
        let w = Watcher::new(false, Box::new(|_| {}));
        wait_for("the root in sync", || {
            w.token(&p);
            w.token(&f);
            w.sync();
            w.token(&p).is_some()
        });
        assert_eq!((w.token(&f), w.token(&dir.join("folded/Foo.h"))), (None, None));
    }
}
