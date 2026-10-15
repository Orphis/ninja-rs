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
        self.0.whole.write().unwrap().add(dir);
        self.0.add_tree(dir).inspect_err(|_| {
            self.0.whole.write().unwrap().remove(dir);
        })
    }

    fn unwatch(&self, dir: &Path) {
        self.0.whole.write().unwrap().remove_under(dir);
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
fn under(dirs: &RwLock<PathSet>, dir: &Path) -> bool {
    let dirs = dirs.read().unwrap();
    !dirs.is_empty() && dir.ancestors().any(|a| dirs.contains(a))
}

/// Watch descriptors and their directories, and the other way round
/// (those under a directory go together).
#[derive(Default)]
struct Wds {
    dirs: HashMap<i32, PathBuf>,
    by_dir: PathMap<i32>,
}

impl Wds {
    /// Whether it's new (not a directory watched already).
    fn insert(&mut self, wd: i32, dir: &Path) -> bool {
        // Another directory's at that path before: its events, if any
        // are left, still name it, until it's gone.
        let new = self.by_dir.insert(dir, wd) != Some(wd);
        self.dirs.insert(wd, dir.to_path_buf());
        new
    }

    /// Forgets the watches of `dir` and under it: theirs.
    fn take_under(&mut self, dir: &Path) -> Vec<i32> {
        let mut under = Vec::new();
        self.by_dir.take_under(dir, |_, wd| under.push(wd));
        for wd in &under {
            self.dirs.remove(wd);
        }
        under
    }
}

struct Inotify {
    fd: i32,
    wds: Mutex<Wds>,
    /// Directories watched with everything under them: directories
    /// created there are watched as they appear.
    whole: RwLock<PathSet>,
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
            // nothing then); before Linux 5.3 (RHEL 8's 4.18), its own
            // events may come first: what's under it then goes here, and
            // the parent's event finds little left. One replaced since:
            // its path is watched again, with another descriptor.
            // Otherwise its file system was unmounted, which nothing
            // reported.
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
            self.whole.write().unwrap().remove_under(&path);
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

/// Checks that `dir`'s file system reports every change: a network or
/// virtual one (NFS, SMB, WSL's view of Windows drives, a VM's shared
/// folders) only reports this machine's, so the daemon would vouch for
/// files others changed. Such roots are checked once per build instead.
pub(super) fn local_file_system(dir: &Path) -> Result<(), String> {
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
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
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
