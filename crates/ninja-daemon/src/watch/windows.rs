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
    /// replaced since by a fresh clone, say): it's stopped once the
    /// directory there now is opened (by the reader that takes its
    /// place: of two set-ups at once, the one in the map last stops the
    /// other's, which would hold the directory for the daemon's life).
    /// Watched once the new reader's first read is issued: events from
    /// then on are kept between reads (in a buffer of the first read's
    /// size).
    fn watch(&self, root: &Path, _cookie_dir: &Path, _whole: bool) -> io::Result<()> {
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
        if let Some(old) = self.active.lock().unwrap().insert(root.clone(), stop.clone()) {
            old.stop();
        }
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

/// Checks that `dir`'s file system reports every change (see the
/// Linux one): not a network drive.
pub(super) fn local_file_system(dir: &Path) -> Result<(), String> {
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

    /// Two set-ups of a root at once (two syncs watching it again): the
    /// reader displaced is stopped (it was left holding the directory,
    /// and those above it, for the daemon's life).
    #[test]
    fn concurrent_watches_let_go() {
        let base = test_dirs::TestDir::new("ninja_daemon_concurrent");
        let w = Watcher::new(true, Box::new(|_| {}));
        let b: Arc<dyn Backend> = Arc::from(backend(Arc::downgrade(&w)).unwrap());
        for i in 0..20 {
            let (parent, moved) = (base.join(format!("p{i}")), base.join(format!("moved{i}")));
            let root = parent.join("root");
            std::fs::create_dir_all(&root).unwrap();
            let both = Arc::new(std::sync::Barrier::new(2));
            let set_ups: Vec<_> = (0..2)
                .map(|_| {
                    let (b, root, both) = (b.clone(), root.clone(), both.clone());
                    std::thread::spawn(move || {
                        both.wait();
                        b.watch(&root, &root, true).unwrap();
                    })
                })
                .collect();
            for s in set_ups {
                s.join().unwrap();
            }
            b.unwatch(&root);
            let deadline = Instant::now() + Duration::from_secs(60);
            while let Err(e) = std::fs::rename(&parent, &moved) {
                assert!(Instant::now() < deadline, "a reader of {} still holds it: {e}", root.display());
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
