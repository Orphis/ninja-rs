//! Watching the files the daemon knows, so that what it checked stays
//! valid until they change.
//!
//! - **Roots**, watched recursively: the repository a checked file is in
//!   (the nearest directory with `.git`, `.hg`, `.jj`, `.sl` or `.svn`),
//!   and directories the daemon was asked for whole (toolchain inputs).
//!   Files elsewhere (system headers) aren't watched: they're checked once
//!   per build, as without the daemon. So are build directories, whose
//!   files change during builds.
//! - **Trust.** A root's events are only trusted once it's in sync: before
//!   each build, the daemon creates a file (a cookie) in every root it can
//!   write to (in `.git` when there is one) and waits for its event, which
//!   comes after every change made before (Watchman's technique). A root
//!   whose watcher lost events (a queue overflow) is out of sync again, and
//!   everything under it is checked again.
//! - **Events** drop the entries they concern (`Listener`): a file's, the
//!   names of its directory when it's created, removed or renamed, and
//!   everything under a directory that was.
//! - **Paths.** Only paths without symbolic links are vouched for: events
//!   come for real paths.
//!
//! Backends: inotify on Linux (the build directories aren't watched, so
//!   that compiles writing objects don't flood the queue),
//!   `ReadDirectoryChangesW` on Windows. Elsewhere nothing is watched.

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
}

/// Directories marking a repository: its root is watched; events inside
/// them are ignored.
const VCS_DIRS: &[&str] = &[".git", ".hg", ".jj", ".sl", ".svn"];

const COOKIE_PREFIX: &str = ".ninja-rs-cookie-";

/// How long a root's cookie may take to come back.
const COOKIE_TIMEOUT: Duration = Duration::from_secs(2);

trait Backend: Send + Sync {
    /// Starts watching `root` recursively.
    fn watch(&self, root: &Path) -> io::Result<()>;
    /// `dir`'s events aren't needed (a build directory).
    fn ignore(&self, _dir: &Path) {}
}

pub struct Watcher {
    me: Weak<Watcher>,
    case_insensitive: bool,
    /// Bumped by every event that matters and every loss: a check that saw
    /// it change may have read a file mid-change.
    seq: AtomicU64,
    events: AtomicU64,
    roots: RwLock<Vec<Root>>,
    /// Build directories (keys).
    ignored: RwLock<Vec<PathBuf>>,
    /// Directories above vouched-for paths (keys), whose renaming or
    /// removal drops everything under them.
    known_dirs: RwLock<HashSet<PathBuf>>,
    /// Directories (keys) and the repository each is in.
    repos: RwLock<HashMap<PathBuf, Option<PathBuf>>>,
    /// Directories (keys) and whether their path is their real one.
    real_dirs: RwLock<HashMap<PathBuf, bool>>,
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
            ignored: RwLock::default(),
            known_dirs: RwLock::default(),
            repos: RwLock::default(),
            real_dirs: RwLock::default(),
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

    /// A build directory: its events are ignored, and its files never
    /// vouched for.
    pub fn ignore(&self, dir: &Path) {
        let key = self.key(dir);
        {
            let mut ignored = self.ignored.write().unwrap();
            if ignored.contains(&key) {
                return;
            }
            ignored.push(key);
        }
        if let Some(b) = self.backend() {
            b.ignore(dir);
        }
    }

    fn is_ignored(&self, key: &Path) -> bool {
        self.ignored.read().unwrap().iter().any(|i| key.starts_with(i))
    }

    /// Watches `dir` recursively (unless a root has it already).
    pub fn add_root(&self, dir: &Path) {
        let key = self.key(dir);
        {
            let mut roots = self.roots.write().unwrap();
            if roots.iter().any(|r| key.starts_with(&r.key)) {
                return;
            }
            let git = dir.join(".git");
            let cookie_dir = if git.is_dir() { git } else { dir.to_path_buf() };
            roots.push(Root { path: dir.to_path_buf(), key: key.clone(), state: RootState::Starting, cookie_dir });
        }
        self.start(dir.to_path_buf(), key);
    }

    /// Sets up a root's watch, in the background.
    fn start(&self, dir: PathBuf, key: PathBuf) {
        let me = self.me.clone();
        std::thread::spawn(move || {
            let Some(w) = me.upgrade() else { return };
            let t = Instant::now();
            let state = match (w.backend(), local_file_system(&dir)) {
                (None, _) => RootState::Failed("file watching is not supported on this system".into()),
                (_, Err(why)) => RootState::Failed(why),
                (Some(b), Ok(())) => match b.watch(&dir) {
                    Ok(()) => RootState::Unsynced,
                    Err(e) => RootState::Failed(e.to_string()),
                },
            };
            (w.log)(&format!("watching {} ({:.0?}): {state:?}", dir.display(), t.elapsed()));
            w.set_state(&key, state, |s| *s == RootState::Starting);
        });
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

    /// The repository `dir` is in (cached for every directory on the way).
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
        let mut repos = self.repos.write().unwrap();
        for k in walked {
            repos.insert(k, found.clone());
        }
        found
    }

    /// Whether `path` names itself: no symbolic link in it.
    fn is_real(&self, path: &Path) -> bool {
        let Some(dir) = path.parent() else { return false };
        let dkey = self.key(dir);
        let cached = self.real_dirs.read().unwrap().get(&dkey).copied();
        let real_dir = cached.unwrap_or_else(|| {
            let real = std::fs::canonicalize(dir).is_ok_and(|c| self.key(&strip_verbatim(c)) == dkey);
            self.real_dirs.write().unwrap().insert(dkey, real);
            real
        });
        real_dir && !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
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

    /// Before a build: restarts the watches that failed, and brings every
    /// root in sync (see the module's doc). Roots the daemon can't write to
    /// are trusted without a cookie: system directories change rarely.
    pub fn sync(&self) {
        let failed: Vec<(PathBuf, PathBuf)> = {
            let mut roots = self.roots.write().unwrap();
            roots
                .iter_mut()
                .filter(|r| matches!(r.state, RootState::Failed(_)))
                .map(|r| {
                    r.state = RootState::Starting;
                    (r.path.clone(), r.key.clone())
                })
                .collect()
        };
        for (path, key) in failed {
            self.start(path, key);
        }
        static N: AtomicU64 = AtomicU64::new(0);
        let roots: Vec<(PathBuf, PathBuf, PathBuf)> = self
            .roots
            .read()
            .unwrap()
            .iter()
            .filter(|r| matches!(r.state, RootState::Unsynced | RootState::Trusted))
            .map(|r| (r.key.clone(), r.path.clone(), r.cookie_dir.clone()))
            .collect();
        let mut waiting = Vec::new();
        for (key, path, cookie_dir) in roots {
            let name = format!("{COOKIE_PREFIX}{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            self.cookies.lock().unwrap().insert(name.clone(), false);
            let file = cookie_dir.join(&name);
            if std::fs::File::create(&file).is_ok() {
                waiting.push((key, path, name, file));
            } else {
                self.cookies.lock().unwrap().remove(&name);
                self.set_state(&key, RootState::Trusted, |s| *s == RootState::Unsynced);
            }
        }
        let deadline = Instant::now() + COOKIE_TIMEOUT;
        for (key, path, name, file) in waiting {
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
                self.set_state(&key, RootState::Trusted, |s| *s == RootState::Unsynced);
            } else {
                // The watch is broken: what it vouched for is checked again.
                (self.log)(&format!("no event from {} in {COOKIE_TIMEOUT:?}: watching it again", path.display()));
                self.untrust(&key);
                self.set_state(&key, RootState::Starting, |_| true);
                self.start(path, key);
            }
        }
    }

    /// Everything under a root is checked again.
    fn untrust(&self, key: &Path) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        let path = self.roots.read().unwrap().iter().find(|r| r.key == key).map(|r| r.path.clone());
        if let Some(path) = path {
            for l in self.listeners.read().unwrap().iter() {
                l.changed(&path, Change::Tree);
            }
        }
    }

    /// A backend lost events (of `root`, or of every root).
    fn lost(&self, root: Option<&Path>) {
        let keys: Vec<PathBuf> = {
            let mut roots = self.roots.write().unwrap();
            let rkey = root.map(|r| self.key(r));
            roots
                .iter_mut()
                .filter(|r| rkey.as_ref().is_none_or(|k| *k == r.key))
                .map(|r| {
                    if r.state == RootState::Trusted {
                        r.state = RootState::Unsynced;
                    }
                    r.key.clone()
                })
                .collect()
        };
        (self.log)(&format!("events lost ({}): checking files again", root.map_or("every root".to_string(), |r| r.display().to_string())));
        for k in keys {
            self.untrust(&k);
        }
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
        if self.is_ignored(&key) || key.components().any(|c| matches!(c, Component::Normal(n) if VCS_DIRS.iter().any(|v| n == *v))) {
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
        // What was learned about directories there.
        self.real_dirs.write().unwrap().retain(|k, _| !k.starts_with(&key));
        let repos_under = match key.file_name().and_then(|n| n.to_str()) {
            // A repository appeared or went: lookups beside it change.
            Some(n) if VCS_DIRS.contains(&n) => key.parent().map(Path::to_path_buf).unwrap_or_else(|| key.clone()),
            _ => key.clone(),
        };
        self.repos.write().unwrap().retain(|k, _| !k.starts_with(&repos_under));
    }
}

impl cpp_deps::scan::Watch for Watcher {
    fn token(&self, path: &Path) -> Option<u64> {
        let seq = self.seq.load(Ordering::SeqCst);
        let key = self.key(path);
        if self.is_ignored(&key) {
            return None;
        }
        let root = {
            let roots = self.roots.read().unwrap();
            roots.iter().find(|r| key.starts_with(&r.key)).map(|r| (r.key.clone(), r.state == RootState::Trusted))
        };
        match root {
            Some((root, true)) if self.is_real(path) => {
                self.remember_dirs(&key, &root);
                Some(seq)
            }
            Some(_) => None,
            None => {
                // Watched from now on: vouched for after the next sync.
                if let Some(repo) = path.parent().and_then(|d| self.repository(d)) {
                    self.add_root(&repo);
                }
                None
            }
        }
    }

    fn unchanged_since(&self, token: u64) -> bool {
        self.seq.load(Ordering::SeqCst) == token
    }
}

/// `p` as a key: lowercase on case-insensitive file systems.
pub fn key(p: &Path, case_insensitive: bool) -> PathBuf {
    if case_insensitive { PathBuf::from(p.to_string_lossy().to_lowercase()) } else { p.to_path_buf() }
}

/// A canonical path without Windows' `\\?\` prefix.
fn strip_verbatim(p: PathBuf) -> PathBuf {
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
    use std::os::unix::ffi::OsStrExt;

    pub(super) fn backend(sink: Weak<Watcher>) -> Option<Box<dyn Backend>> {
        // SAFETY: plain syscall.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let inotify = Arc::new(Inotify { fd, wds: Mutex::default(), ignored: RwLock::default(), sink });
        let reader = inotify.clone();
        std::thread::spawn(move || reader.read_loop());
        Some(Box::new(Shared(inotify)))
    }

    struct Shared(Arc<Inotify>);

    impl Backend for Shared {
        fn watch(&self, root: &Path) -> io::Result<()> {
            self.0.add_tree(root)
        }

        fn ignore(&self, dir: &Path) {
            self.0.ignored.write().unwrap().push(dir.to_path_buf());
            self.0.remove_tree(dir);
        }
    }

    struct Inotify {
        fd: i32,
        /// Watch descriptors and their directories.
        wds: Mutex<HashMap<i32, PathBuf>>,
        ignored: RwLock<Vec<PathBuf>>,
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

    impl Inotify {
        fn is_ignored(&self, dir: &Path) -> bool {
            self.ignored.read().unwrap().iter().any(|i| dir.starts_with(i))
        }

        /// Watches `dir` and the directories under it (not into
        /// repositories' own directories, nor build directories).
        fn add_tree(&self, dir: &Path) -> io::Result<()> {
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                if self.is_ignored(&d) {
                    continue;
                }
                let c = CString::new(d.as_os_str().as_bytes()).map_err(io::Error::other)?;
                // SAFETY: `c` is a valid C string.
                let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
                if wd < 0 {
                    let e = io::Error::last_os_error();
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
            if ev.mask & libc::IN_IGNORED != 0 {
                self.wds.lock().unwrap().remove(&ev.wd);
                return;
            }
            let path = if name.is_empty() { dir.clone() } else { dir.join(std::ffi::OsStr::from_bytes(name)) };
            let is_dir = ev.mask & libc::IN_ISDIR != 0;
            if is_dir && ev.mask & libc::IN_MOVED_FROM != 0 {
                // Its watches would report the old paths.
                self.remove_tree(&path);
            }
            if is_dir && ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 && self.add_tree(&path).is_err() {
                sink.lost(None);
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
}

#[cfg(windows)]
mod platform {
    //! `ReadDirectoryChangesW` on each root (recursive), each read by its
    //! own thread. A zero-length result or `ERROR_NOTIFY_ENUM_DIR` means
    //! the buffer overflowed: events were lost.

    use super::*;
    use std::ffi::c_void;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const FILE_LIST_DIRECTORY: u32 = 0x1;
    const FILE_SHARE_ALL: u32 = 0x7;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    // File and directory names, attributes, size, last write, creation, security.
    const FILTER: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x40 | 0x100;
    const ERROR_NOTIFY_ENUM_DIR: i32 = 1022;
    const FILE_ACTION_MODIFIED: u32 = 3;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(name: *const u16, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
        fn ReadDirectoryChangesW(
            dir: Handle,
            buffer: *mut c_void,
            len: u32,
            subtree: i32,
            filter: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
            routine: *const c_void,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLongPathNameW(short: *const u16, long: *mut u16, len: u32) -> u32;
    }

    pub(super) fn backend(sink: Weak<Watcher>) -> Option<Box<dyn Backend>> {
        Some(Box::new(Directories { sink, active: Arc::default() }))
    }

    struct Directories {
        sink: Weak<Watcher>,
        /// Roots with a running reader.
        active: Arc<Mutex<HashSet<PathBuf>>>,
    }

    struct Owned(Handle);
    unsafe impl Send for Owned {}
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle is owned.
            unsafe { CloseHandle(self.0) };
        }
    }

    impl Backend for Directories {
        fn watch(&self, root: &Path) -> io::Result<()> {
            if self.active.lock().unwrap().contains(root) {
                return Ok(());
            }
            let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
            // SAFETY: `wide` is NUL-terminated.
            let h = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_ALL,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let handle = Owned(h);
            // The first read sets up the watch: events from then on are
            // kept between reads (in a buffer of the first read's size).
            let (tx, rx) = std::sync::mpsc::channel();
            let root = root.to_path_buf();
            let sink = self.sink.clone();
            self.active.lock().unwrap().insert(root.clone());
            let active = self.active.clone();
            std::thread::spawn(move || {
                read_loop(handle, &root, sink, tx);
                active.lock().unwrap().remove(&root);
            });
            // The thread is about to read; let it start (the next sync
            // checks that it did).
            let _ = rx.recv_timeout(Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(20));
            Ok(())
        }
    }

    fn read_loop(handle: Owned, root: &Path, sink: Weak<Watcher>, started: std::sync::mpsc::Sender<()>) {
        let mut buf = vec![0u32; 1 << 18];
        let _ = started.send(());
        loop {
            let mut n = 0u32;
            // SAFETY: `buf` is u32-aligned and writable for its byte length.
            let ok = unsafe {
                ReadDirectoryChangesW(handle.0, buf.as_mut_ptr().cast(), (buf.len() * 4) as u32, 1, FILTER, &mut n, std::ptr::null_mut(), std::ptr::null())
            };
            let Some(w) = sink.upgrade() else { return };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR) {
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
            let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), n as usize) };
            let mut off = 0usize;
            loop {
                let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
                let (next, action, len) = (u32_at(off), u32_at(off + 4), u32_at(off + 8) as usize);
                let name: Vec<u16> = bytes[off + 12..off + 12 + len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let path = root.join(std::ffi::OsString::from_wide(&name));
                match long_path(&path) {
                    // A directory's timestamps follow its files', whose
                    // own events tell what changed.
                    Some(p) if action == FILE_ACTION_MODIFIED && p.is_dir() => {}
                    Some(p) => w.event(&p, action != FILE_ACTION_MODIFIED),
                    None => w.lost(Some(root)),
                }
                if next == 0 {
                    break;
                }
                off += next as usize;
            }
        }
    }

    /// `path` with long names: events may use a short (8.3) name. None when
    /// it has one and can't be expanded (the file is gone).
    fn long_path(path: &Path) -> Option<PathBuf> {
        let short = |c: &std::ffi::OsStr| {
            let s = c.to_string_lossy();
            s.find('~').is_some_and(|i| s[i + 1..].starts_with(|ch: char| ch.is_ascii_digit()))
        };
        if !path.components().any(|c| short(c.as_os_str())) {
            return Some(path.to_path_buf());
        }
        let expand = |p: &Path| -> Option<PathBuf> {
            let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
            let mut out = vec![0u16; 32768];
            // SAFETY: both buffers are valid; `out` is writable for its length.
            let n = unsafe { GetLongPathNameW(wide.as_ptr(), out.as_mut_ptr(), out.len() as u32) } as usize;
            (n > 0 && n < out.len()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&out[..n])))
        };
        expand(path).or_else(|| {
            // Gone: its directory, if only the directory has short names.
            let (dir, name) = (path.parent()?, path.file_name()?);
            if short(name) { None } else { expand(dir).map(|d| d.join(name)) }
        })
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
    use std::os::windows::ffi::OsStrExt;
    const DRIVE_REMOTE: u32 = 4;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumePathNameW(path: *const u16, volume: *mut u16, len: u32) -> i32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
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
        let dir = std::env::temp_dir().join(format!("ninja_daemon_watch_{}", std::process::id()));
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
        // Files in the moved directory are reported at their new paths.
        rec.0.lock().unwrap().clear();
        std::fs::write(dir.join("inc/moved/a.h"), "zzz").unwrap();
        wait_for("the moved file", || rec.0.lock().unwrap().iter().any(|(p, _)| *p == dir.join("inc/moved/a.h")));
        // Still in sync.
        w.sync();
        assert_eq!(w.roots(), vec![(dir.clone(), RootState::Trusted)]);
        // Paths through symbolic links aren't vouched for.
        let outside = std::env::temp_dir().join(format!("ninja_daemon_watch_out_{}", std::process::id()));
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
}
