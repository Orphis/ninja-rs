//! What ninja-rs' crates share from the operating system: file stamps and
//! digests (what the digest caches of builds and of the daemon key on),
//! executable bits, random numbers, file identities and locks (the build
//! logs'), processes' identities and parents (who holds a lock), programs
//! run with a time limit ([`process`]), and on Windows
//! ([`windows`]) who owns files and processes.

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

pub mod process;
#[cfg(windows)]
pub mod windows;

/// What a file's digest is valid for, besides its path: its size and
/// modification time, and what tools don't set back: its change time and
/// identity. Files extracted in place from an archive with normalized
/// times can have new contents, the same size and an old modification
/// time; not an old change time. A file created again under its name gets
/// a new identity (on Windows, where NTFS gives it the old one's creation
/// time within 15 s, a new file ID).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileStamp {
    pub len: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: u64,
    /// Unix: in nanoseconds; Windows: in 100 ns.
    pub change: u64,
    /// Windows: the creation time, in 100 ns (FAT has no change time); 0
    /// elsewhere.
    pub created: u64,
    /// Windows: the volume's serial number (file IDs are per volume); 0
    /// elsewhere.
    pub volume: u64,
    /// Unix: the inode; Windows: the file ID.
    pub id: u128,
}

impl FileStamp {
    /// Whether the file was modified within its file system's time
    /// granularity of `now`: changed again since, it could have kept this
    /// stamp, so its digest isn't kept across builds (git's "racy"
    /// entries). Whole seconds are 2 s (FAT counts in 2 s, ext3 and HFS+ in
    /// seconds), finer times 100 ms (kernels stamp files with a clock that
    /// ticks every few milliseconds).
    pub fn is_racy(&self, now: SystemTime) -> bool {
        let now_ns = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        let granularity = if self.mtime_ns.is_multiple_of(1_000_000_000) { 2_000_000_000 } else { 100_000_000 };
        self.mtime_ns.saturating_add(granularity) > now_ns
    }
}

fn mtime_ns(meta: &fs::Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64)
}

/// A file's metadata (symlinks followed) and stamp.
#[cfg(unix)]
pub fn stat(path: &Path) -> io::Result<(fs::Metadata, FileStamp)> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path)?;
    let change = (meta.ctime() as u64).wrapping_mul(1_000_000_000).wrapping_add(meta.ctime_nsec() as u64);
    let stamp = FileStamp { len: meta.len(), mtime_ns: mtime_ns(&meta), change, created: 0, volume: 0, id: u128::from(meta.ino()) };
    Ok((meta, stamp))
}

/// A file's metadata (symlinks followed) and stamp, from a handle to it:
/// its change time and file ID aren't in `std::fs::Metadata`.
#[cfg(windows)]
pub fn stat(path: &Path) -> io::Result<(fs::Metadata, FileStamp)> {
    let f = windows::open_attributes(path)?;
    let meta = f.metadata()?;
    // A file system without them (FAT has no change time) leaves zeros.
    let (change, created) = windows::times(&f).unwrap_or_default();
    let (volume, id) = windows::file_id(&f).unwrap_or_default();
    let stamp = FileStamp { len: meta.len(), mtime_ns: mtime_ns(&meta), change, created, volume, id };
    Ok((meta, stamp))
}

/// A file's metadata (symlinks followed) and stamp.
#[cfg(not(any(unix, windows)))]
pub fn stat(path: &Path) -> io::Result<(fs::Metadata, FileStamp)> {
    let meta = fs::metadata(path)?;
    let stamp = FileStamp { len: meta.len(), mtime_ns: mtime_ns(&meta), ..FileStamp::default() };
    Ok((meta, stamp))
}

/// The identity of the file `f` is open on: on Unix its device and inode,
/// on Windows its volume's serial number and file ID.
#[cfg(unix)]
pub fn identity(f: &fs::File) -> io::Result<(u64, u128)> {
    use std::os::unix::fs::MetadataExt;
    let meta = f.metadata()?;
    Ok((meta.dev(), u128::from(meta.ino())))
}

/// The identity of the file `f` is open on: on Unix its device and inode,
/// on Windows its volume's serial number and file ID.
#[cfg(windows)]
pub fn identity(f: &fs::File) -> io::Result<(u64, u128)> {
    // FAT has no 128-bit file IDs.
    windows::file_id(f).map_or_else(|| windows::file_index(f), Ok)
}

/// Whether `f`, opened as `path`, isn't the file there any more: another
/// process renamed a file over it, or removed it. Unix compares
/// identities; Windows asks whether the handle's file is deleted, which a
/// rename over it does (a rename can't replace a file open elsewhere
/// otherwise).
#[cfg(unix)]
pub fn replaced(f: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (f.metadata(), fs::metadata(path)) {
        (Ok(open), Ok(named)) => (open.dev(), open.ino()) != (named.dev(), named.ino()),
        (_, Err(e)) => e.kind() == io::ErrorKind::NotFound,
        _ => false,
    }
}

/// Whether `f`, opened as `path`, isn't the file there any more: another
/// process renamed a file over it, or removed it. Unix compares
/// identities; Windows asks whether the handle's file is deleted, which a
/// rename over it does (a rename can't replace a file open elsewhere
/// otherwise).
#[cfg(windows)]
pub fn replaced(f: &fs::File, _path: &Path) -> bool {
    windows::deleted(f)
}

/// Takes an exclusive lock on the file `f` is open on, for this handle:
/// another handle's lock excludes it, even in this process. Waits for it
/// (`wait`), or returns false while another holds it. The lock is
/// advisory: reads and writes through other handles go on. On Unix a
/// `flock`; on Windows, where a lock on a file's data makes other
/// handles' reads and writes of it fail, one on a byte far past any data
/// (which excludes a lock on the whole file, std's). Released by
/// [`unlock`], or when the handle is closed.
#[cfg(unix)]
pub fn lock(f: &fs::File, wait: bool) -> io::Result<bool> {
    if wait {
        return f.lock().map(|()| true);
    }
    match f.try_lock() {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Takes an exclusive lock on the file `f` is open on, for this handle:
/// another handle's lock excludes it, even in this process. Waits for it
/// (`wait`), or returns false while another holds it. The lock is
/// advisory: reads and writes through other handles go on. On Unix a
/// `flock`; on Windows, where a lock on a file's data makes other
/// handles' reads and writes of it fail, one on a byte far past any data
/// (which excludes a lock on the whole file, std's). Released by
/// [`unlock`], or when the handle is closed.
#[cfg(windows)]
pub fn lock(f: &fs::File, wait: bool) -> io::Result<bool> {
    windows::lock(f, wait)
}

/// Releases [`lock`].
#[cfg(unix)]
pub fn unlock(f: &fs::File) -> io::Result<()> {
    f.unlock()
}

/// Releases [`lock`].
#[cfg(windows)]
pub fn unlock(f: &fs::File) -> io::Result<()> {
    windows::unlock(f)
}

/// What tells a running process from others with its id: those that had
/// it before (a lock file naming a process gone), and those that have it
/// in another pid namespace (two containers sharing a directory).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    /// When it started: on Linux in clock ticks since boot, on macOS in µs
    /// since the Unix epoch, on Windows in 100 ns since 1601.
    pub start: u64,
    /// Linux: its pid namespace's inode (0 when that can't be read, as
    /// another user's); 0 elsewhere.
    pub namespace: u64,
}

/// A running process, as [`process_info`] tells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub identity: ProcessIdentity,
    /// Its parent's id (0: none).
    pub parent: u32,
}

/// The running process `pid` (`None`: this one), if it can be asked.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn process_info(pid: Option<u32>) -> Option<ProcessInfo> {
    let dir = format!("/proc/{}", pid.map_or("self".to_string(), |p| p.to_string()));
    let stat = fs::read_to_string(format!("{dir}/stat")).ok()?;
    // `pid (name) state ppid ...`: the name may hold spaces and
    // parentheses. `starttime` is the 22nd field.
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let (parent, start) = (fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?);
    // `pid:[4026531836]`.
    let link = fs::read_link(format!("{dir}/ns/pid")).ok();
    let namespace = link.as_ref().and_then(|l| l.to_str()?.strip_prefix("pid:[")?.strip_suffix(']')?.parse().ok()).unwrap_or(0);
    Some(ProcessInfo { identity: ProcessIdentity { start, namespace }, parent })
}

/// The running process `pid` (`None`: this one), if it can be asked.
#[cfg(target_os = "macos")]
pub fn process_info(pid: Option<u32>) -> Option<ProcessInfo> {
    let pid = libc::c_int::try_from(pid.unwrap_or_else(std::process::id)).ok()?;
    // SAFETY: fills `info`, of the size given, nothing else.
    let info = unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        (libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) == size).then_some(info)?
    };
    let start = info.pbi_start_tvsec.saturating_mul(1_000_000).saturating_add(info.pbi_start_tvusec);
    Some(ProcessInfo { identity: ProcessIdentity { start, namespace: 0 }, parent: info.pbi_ppid })
}

/// The running process `pid` (`None`: this one), if it can be asked.
#[cfg(windows)]
pub fn process_info(pid: Option<u32>) -> Option<ProcessInfo> {
    windows::process_info(pid.unwrap_or_else(std::process::id))
}

/// The running process `pid`: not known here.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
pub fn process_info(_pid: Option<u32>) -> Option<ProcessInfo> {
    None
}

/// Whether process `pid` runs (a zombie, ended but not waited for,
/// doesn't); `None` when that can't be asked (another user's process, on
/// Windows).
#[cfg(unix)]
pub fn process_running(pid: u32) -> Option<bool> {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return Some(false) };
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Some(false),
            // Another user's.
            Some(libc::EPERM) => Some(!zombie(pid)),
            _ => None,
        };
    }
    Some(!zombie(pid))
}

/// Whether process `pid` is a zombie, where `/proc` tells.
#[cfg(unix)]
fn zombie(pid: libc::pid_t) -> bool {
    // "pid (name) S ...": the state follows the name.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
}

/// Whether process `pid` runs; `None` when that can't be asked (another
/// user's process).
#[cfg(windows)]
pub fn process_running(pid: u32) -> Option<bool> {
    windows::process_running(pid)
}

/// Whether process `pid` runs: not known here.
#[cfg(not(any(unix, windows)))]
pub fn process_running(_pid: u32) -> Option<bool> {
    None
}

/// A file's digest, as REAPI has it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Digest {
    /// SHA-256, hex.
    pub hash: String,
    pub size: u64,
}

/// The digest of a file's contents.
pub fn hash_file(path: &Path) -> io::Result<Digest> {
    let mut f = fs::File::open(path).map_err(|e| io::Error::new(e.kind(), format!("open {}: {e}", path.display())))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        h.update(&buf[..n]);
    }
    Ok(Digest { hash: hex::encode(h.finalize()), size })
}

/// Whether a file's mode lets it run (never on Windows, where that isn't
/// part of the file).
#[cfg(unix)]
pub fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// Whether a file's mode lets it run (never on Windows, where that isn't
/// part of the file).
#[cfg(not(unix))]
pub fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

/// A random number, from std's hasher keys: random per thread (the system
/// seeds them), and different for each hasher. Enough for unique names
/// and jitter.
pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    /// Waits for the file system's clock to move past `change` (a file's):
    /// changes within one of its ticks get the same change time (Windows
    /// counts in 100 ns but ticks every half millisecond or so, Linux every
    /// few milliseconds).
    fn tick_past(dir: &Path, change: u64) {
        let probe = dir.join("tick");
        loop {
            std::fs::write(&probe, "").unwrap();
            if stat(&probe).unwrap().1.change > change {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A file rewritten in place with its size and modification time
    /// restored, or created again under its name: a new stamp (on Windows,
    /// the creation time NTFS gives back is the same).
    #[test]
    fn rewrites_change_the_stamp() {
        let dir = test_dirs::TestDir::new("ninja_os_stamps");
        let f = dir.join("f.h");
        let old = SystemTime::now() - Duration::from_secs(60);
        let write = |text: &str| {
            std::fs::write(&f, text).unwrap();
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(old).unwrap();
            stat(&f).unwrap().1
        };
        let first = write("version 1");
        tick_past(&dir, first.change);
        let rewritten = write("version 2");
        assert_eq!((rewritten.len, rewritten.mtime_ns), (first.len, first.mtime_ns));
        assert_ne!(rewritten, first, "rewritten in place");
        // Unix may give the next file the same inode.
        tick_past(&dir, rewritten.change);
        std::fs::remove_file(&f).unwrap();
        let recreated = write("version 3");
        assert_eq!((recreated.len, recreated.mtime_ns), (first.len, first.mtime_ns));
        assert_ne!(recreated, rewritten, "created again");
        assert_eq!(stat(&f).unwrap().1, recreated);
    }

    #[test]
    fn digests() {
        let dir = test_dirs::TestDir::new("ninja_os_digests");
        std::fs::write(dir.join("a"), "a").unwrap();
        let d = hash_file(&dir.join("a")).unwrap();
        assert_eq!((d.hash.as_str(), d.size), ("ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb", 1));
        let e = hash_file(&dir.join("b")).unwrap_err();
        assert!(e.to_string().starts_with(&format!("open {}: ", dir.join("b").display())), "{e}");
    }

    /// Locks exclude each other between handles, not reads and writes.
    #[test]
    fn locks() {
        let dir = test_dirs::TestDir::new("ninja_os_locks");
        let path = dir.join("lock");
        let open = || fs::File::options().read(true).append(true).create(true).open(&path).unwrap();
        let (a, b) = (open(), open());
        assert!(lock(&a, false).unwrap());
        assert!(!lock(&b, false).unwrap());
        (&b).write_all(b"written").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"written");
        unlock(&a).unwrap();
        assert!(lock(&b, true).unwrap());
        assert!(!lock(&a, false).unwrap());
        drop(b);
        // Waited for: a child another test forks meanwhile shares the
        // closed handle's lock until it starts its program (Unix).
        assert!(lock(&a, true).unwrap());
        assert_eq!(identity(&a).unwrap(), identity(&open()).unwrap());
    }

    /// A file renamed over, or removed: the handle open on it sees it.
    #[test]
    fn replaced_files() {
        let dir = test_dirs::TestDir::new("ninja_os_replaced");
        let path = dir.join("log");
        fs::write(&path, "old").unwrap();
        let f = fs::File::options().read(true).append(true).open(&path).unwrap();
        assert!(!replaced(&f, &path));
        fs::write(dir.join("new"), "new").unwrap();
        fs::rename(dir.join("new"), &path).unwrap();
        assert!(replaced(&f, &path));
        let g = fs::File::options().read(true).append(true).open(&path).unwrap();
        assert!(!replaced(&g, &path));
        assert_ne!(identity(&f).unwrap(), identity(&g).unwrap());
        fs::remove_file(&path).unwrap();
        assert!(replaced(&g, &path));
    }

    /// A process tells its identity and its parent; a child started after
    /// it tells it for parent, and a start no earlier (on Linux, in clock
    /// ticks: maybe the same).
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", windows))]
    #[test]
    fn processes_and_their_parents() {
        let this = process_info(None).unwrap();
        assert_eq!(process_info(Some(std::process::id())), Some(this));
        let parent = process_info(Some(this.parent)).unwrap();
        assert!(parent.identity.start <= this.identity.start);
        assert_eq!(parent.identity.namespace, this.identity.namespace);
        if cfg!(any(target_os = "linux", target_os = "android")) {
            assert_ne!(this.identity.namespace, 0);
        }
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        let args: &[&str] = if cfg!(windows) { &["/c", "pause"] } else { &["-c", "read x"] };
        let mut child = std::process::Command::new(program).args(args).stdin(std::process::Stdio::piped()).spawn().unwrap();
        let info = process_info(Some(child.id()));
        drop(child.stdin.take());
        child.wait().unwrap();
        let info = info.unwrap();
        assert_eq!(info.parent, std::process::id());
        assert!(info.identity.start >= this.identity.start);
    }

    /// A process runs until it exits; a zombie (exited, not waited for)
    /// doesn't.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn running_processes() {
        assert_eq!(process_running(std::process::id()), Some(true));
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        let args: &[&str] = if cfg!(windows) { &["/c", "pause"] } else { &["-c", "read x"] };
        let mut child = std::process::Command::new(program).args(args).stdin(std::process::Stdio::piped()).spawn().unwrap();
        let pid = child.id();
        assert_eq!(process_running(pid), Some(true));
        drop(child.stdin.take());
        let start = std::time::Instant::now();
        while process_running(pid) == Some(true) {
            assert!(start.elapsed() < Duration::from_secs(60), "the child didn't end");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(process_running(pid), Some(false));
        child.wait().unwrap();
    }

    #[test]
    fn random_numbers_differ() {
        assert_ne!(random_u64(), random_u64());
    }
}
