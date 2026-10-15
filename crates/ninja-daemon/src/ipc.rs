//! Where a daemon listens, and local connections to it: Unix sockets in a
//! directory only the user can enter, Windows named pipes (which other
//! users can't write to, and remote clients can't open).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// A daemon's files (lock, log, socket, cookies) and its name.
#[derive(Clone, Debug)]
pub struct Location {
    pub dir: PathBuf,
    /// Tells apart the daemons of different ninja-rs binaries.
    pub id: String,
}

fn short_hash(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    hex::encode(&h.finalize()[..8])
}

/// The identity of the file at `path` (links followed): its device or
/// volume, and its inode or file ID, however the path spells it.
#[cfg(unix)]
fn file_identity(path: &Path) -> io::Result<(u64, u128)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok((meta.dev(), u128::from(meta.ino())))
}

#[cfg(windows)]
fn file_identity(path: &Path) -> io::Result<(u64, u128)> {
    ninja_os::identity(&ninja_os::windows::open_attributes(path)?)
}

#[cfg(not(any(unix, windows)))]
fn file_identity(path: &Path) -> io::Result<(u64, u128)> {
    let real = std::fs::canonicalize(path)?;
    Ok((0, u128::from_str_radix(&short_hash(&[real.to_string_lossy().as_bytes()]), 16).unwrap_or_default()))
}

/// How long builds don't start a daemon again after one failed to start
/// (one that dies at once would, every build, after waiting for it).
pub const START_FAILED_FOR: Duration = Duration::from_secs(5 * 60);

impl Location {
    /// The daemon of the ninja-rs binary `exe`, for this user: a rebuilt
    /// binary gets its own (the old one exits when idle). The binary is
    /// told by its identity, not its path: through another spelling (case,
    /// slashes, a junction, a `subst` drive, a link) it's the same daemon.
    /// The directory is `NINJA_RS_DAEMON_DIR` when set.
    pub fn for_exe(exe: &Path) -> io::Result<Location> {
        let meta = std::fs::metadata(exe)?;
        let mtime = meta.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let (volume, file) = file_identity(exe)?;
        let id = short_hash(&[&volume.to_le_bytes(), &file.to_le_bytes(), &meta.len().to_le_bytes(), &mtime.to_le_bytes()]);
        Ok(Location { dir: default_dir()?, id })
    }

    /// The daemon named `id` (in the default directory).
    pub fn with_id(id: &str) -> io::Result<Location> {
        Ok(Location { dir: default_dir()?, id: id.to_string() })
    }

    pub fn lock_file(&self) -> PathBuf {
        self.dir.join(format!("{}.lock", self.id))
    }

    pub fn log_file(&self) -> PathBuf {
        self.dir.join(format!("{}.log", self.id))
    }

    /// The log before the one being written (see `server::logger`).
    pub fn old_log_file(&self) -> PathBuf {
        self.dir.join(format!("{}.log.1", self.id))
    }

    /// Says why its daemon couldn't start, and since when (see
    /// `start_failed`).
    fn failed_file(&self) -> PathBuf {
        self.dir.join(format!("{}.failed", self.id))
    }

    /// Why its daemon failed to start, if it did less than `within` ago.
    pub fn start_failed(&self, within: Duration) -> Option<String> {
        let path = self.failed_file();
        let when = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        let ago = SystemTime::now().duration_since(when).unwrap_or_default();
        if ago >= within {
            return None;
        }
        let why = std::fs::read_to_string(&path).ok()?;
        Some(format!("{why} ({}s ago)", ago.as_secs()))
    }

    /// Notes that its daemon failed to start, and why (`None`: it started).
    pub fn note_start(&self, failed: Option<&str>) {
        let _ = match failed {
            Some(why) => std::fs::write(self.failed_file(), why),
            None => std::fs::remove_file(self.failed_file()),
        };
    }

    /// Where the watcher's cookies go for roots without a repository
    /// directory (see `Watcher::set_cookie_dir`).
    pub fn cookie_dir(&self) -> PathBuf {
        self.dir.join(format!("{}.cookies", self.id))
    }

    /// In the directory, or in a private one in `/tmp` when that path is
    /// too long for a socket's (about 100 bytes).
    #[cfg(unix)]
    pub fn socket(&self) -> PathBuf {
        let p = self.dir.join(format!("{}.sock", self.id));
        if p.as_os_str().len() < 100 {
            return p;
        }
        short_socket_dir().join(format!("{}-{}.sock", short_hash(&[self.dir.to_string_lossy().as_bytes()]), self.id))
    }

    /// The pipe's name: per directory (so per user) and binary.
    #[cfg(windows)]
    pub fn pipe(&self) -> String {
        format!(r"\\.\pipe\ninja-rs-{}-{}", short_hash(&[self.dir.to_string_lossy().as_bytes()]), self.id)
    }

    /// Whether its daemon runs: it holds its lock.
    pub fn running(&self) -> bool {
        std::fs::File::options().write(true).open(self.lock_file()).is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
    }

    /// Removes the files of other daemons in its directory (of other
    /// ninja-rs binaries: copies, locks, logs, cookies, failures) unless
    /// they run; and copies left half made (`{id}.{pid}.tmp`, a build that
    /// stopped while copying) a while ago.
    pub fn remove_others(&self) -> io::Result<()> {
        const TMP_KEPT: Duration = Duration::from_secs(10 * 60);
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            let path = e.path();
            // `{id}.{kind}`: ids have no dots.
            let Some((id, kind)) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.split_once('.')) else { continue };
            let tmp = kind.ends_with(".tmp");
            if !tmp && (id == self.id || !matches!(kind, "exe" | "lock" | "log" | "log.1" | "cookies" | "failed")) {
                continue;
            }
            if tmp && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < TMP_KEPT) {
                continue;
            }
            if !tmp && (Location { dir: self.dir.clone(), id: id.to_string() }).running() {
                continue;
            }
            let _ = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
        Ok(())
    }

    /// Creates the directories, only the user's (an existing one must be).
    pub fn create_dir(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            private_dir(&self.dir)?;
            match self.socket().parent() {
                Some(d) if d != self.dir => private_dir(d),
                _ => Ok(()),
            }
        }
        #[cfg(windows)]
        platform::private_dir(&self.dir)
    }
}

/// Creates `path`, which must not exist, for writing: a file only the user
/// can read (and the system, on Windows), whatever the directory's
/// permissions give its files.
pub fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
    }
    #[cfg(windows)]
    platform::create_private_file(path)
}

#[cfg(unix)]
fn short_socket_dir() -> PathBuf {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/ninja-rs-{uid}"))
}

/// Creates `dir`, checking that only the user can enter it.
#[cfg(unix)]
fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).recursive(true).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_private(dir)
}

/// Checks that only the user can enter `dir`: its paths are predictable
/// (in `/tmp` without `XDG_RUNTIME_DIR`), so another user could have made
/// it first, with a socket of theirs in it.
#[cfg(unix)]
fn check_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(dir)?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not private to this user", dir.display())));
    }
    Ok(())
}

fn default_dir() -> io::Result<PathBuf> {
    if let Some(d) = std::env::var_os("NINJA_RS_DAEMON_DIR") {
        return Ok(PathBuf::from(d));
    }
    #[cfg(windows)]
    {
        #[allow(clippy::disallowed_methods)] // The daemon's own directory, not a test's.
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        Ok(base.join("ninja-rs").join("daemon"))
    }
    #[cfg(unix)]
    {
        if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(d).join("ninja-rs"));
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        #[allow(clippy::disallowed_methods)] // The daemon's own directory, not a test's.
        let base = std::env::temp_dir();
        Ok(base.join(format!("ninja-rs-{uid}")))
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    pub type Stream = UnixStream;

    /// Connects to the socket, if it's in a directory only the user can
    /// enter and served by a process of the user's: a daemon's answers go
    /// into action keys and uploads.
    pub fn connect(loc: &Location) -> io::Result<Stream> {
        let path = loc.socket();
        if let Some(dir) = path.parent() {
            check_private(dir)?;
        }
        let s = UnixStream::connect(&path)?;
        check_peer(&s)?;
        Ok(s)
    }

    /// The user of the process at the other end.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn peer_uid(s: &UnixStream) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        // SAFETY: `ucred` is plain data, filled by getsockopt.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the buffer and its length are valid.
        let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
        if r != 0 { Err(io::Error::last_os_error()) } else { Ok(cred.uid) }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn peer_uid(s: &UnixStream) -> io::Result<u32> {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: the out-parameters are valid.
        let r = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
        if r != 0 { Err(io::Error::last_os_error()) } else { Ok(uid) }
    }

    fn check_peer(s: &UnixStream) -> io::Result<()> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        match peer_uid(s)? {
            p if p == uid => Ok(()),
            p => Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the other end of the socket is user {p}'s"))),
        }
    }

    pub struct Listener(UnixListener);

    impl Listener {
        /// Listens (the caller holds the lock: a socket file left by a
        /// daemon that died is replaced).
        pub fn bind(loc: &Location) -> io::Result<Listener> {
            let path = loc.socket();
            let _ = std::fs::remove_file(&path);
            Ok(Listener(UnixListener::bind(&path)?))
        }

        /// The next connection of a process of the user's (others are
        /// closed).
        pub fn accept(&mut self) -> io::Result<Stream> {
            loop {
                let s = match self.0.accept() {
                    Ok((s, _)) => s,
                    // A client gone before it was accepted.
                    Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted) => continue,
                    Err(e) => return Err(e),
                };
                if check_peer(&s).is_ok() {
                    return Ok(s);
                }
            }
        }
    }

    /// A connection whose reads and writes give up after `timeout`.
    pub type TimedStream = UnixStream;

    pub fn connect_timed(loc: &Location, timeout: std::time::Duration) -> io::Result<TimedStream> {
        let s = connect(loc)?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        Ok(s)
    }

    /// Starts `exe` in the background, out of the terminal's process group
    /// (so that Ctrl-C in the build doesn't stop it), without the variables
    /// `env_remove` and with those of `env_set`, nor any file of the
    /// build's: descriptors it inherited without close-on-exec (a
    /// jobserver's, a pipe its caller reads) would stay open as long as the
    /// daemon runs.
    pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path, env_remove: &[&str], env_set: &[(&str, &std::ffi::OsStr)]) -> io::Result<Child> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut c = Command::new(exe);
        c.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
        for v in env_remove {
            c.env_remove(v);
        }
        c.envs(env_set.iter().copied());
        // SAFETY: the closure only makes system calls, which are
        // async-signal-safe.
        unsafe {
            c.pre_exec(|| {
                close_on_exec_from(3);
                Ok(())
            })
        };
        c.spawn().map(Child)
    }

    /// A process `spawn_detached` started.
    pub struct Child(std::process::Child);

    impl Child {
        /// None while it runs; Ok once it exited successfully, else how it
        /// ended.
        pub fn exited(&mut self) -> Option<Result<(), String>> {
            let status = self.0.try_wait().ok()??;
            Some(if status.success() { Ok(()) } else { Err(status.to_string()) })
        }
    }

    /// Marks the descriptors from `first` on close-on-exec (std's own,
    /// which reports a failed exec, is used until then).
    fn close_on_exec_from(first: i32) {
        #[cfg(target_os = "linux")]
        {
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            // SAFETY: plain system call.
            if unsafe { libc::syscall(libc::SYS_close_range, first as libc::c_uint, libc::c_uint::MAX, CLOSE_RANGE_CLOEXEC) } == 0 {
                return;
            }
        }
        // Before Linux 5.11, and elsewhere: one by one, up to the limit (or
        // a sane bound).
        let mut l = libc::rlimit { rlim_cur: 1024, rlim_max: 1024 };
        // SAFETY: a valid out-parameter (left as is on failure).
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) };
        for fd in first..l.rlim_cur.min(1 << 16) as i32 {
            // SAFETY: as above; a descriptor not open fails with EBADF.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
    use std::time::{Duration, Instant};

    use ninja_os::windows::{current_user, file_security, process_user, wide};
    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE, LocalFree, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile,
        WriteFile,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    /// SYSTEM, Administrators and TrustedInstaller, who can use any file
    /// anyway; CREATOR OWNER and OWNER RIGHTS, the owner.
    const TRUSTED: &[&str] = &["S-1-5-18", "S-1-5-32-544", "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464", "S-1-3-0", "S-1-3-4"];

    /// Security attributes with `descriptor`.
    fn attributes(descriptor: *mut c_void) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor, bInheritHandle: 0 }
    }

    /// Creates `dir`, which only this user and the system may use, or
    /// checks that an existing one is private (as a Unix mode of 0700):
    /// its path may be predictable (TEMP), so another user could have made
    /// it first, or it may be in a shared directory whose permissions it
    /// inherited.
    pub fn private_dir(dir: &Path) -> io::Result<()> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let wide = wide(dir);
        let descriptor = this_user_only("OICI")?;
        let security = attributes(descriptor);
        // SAFETY: `wide` is NUL-terminated; `security` and its descriptor
        // are alive during the call.
        let created = unsafe { CreateDirectoryW(wide.as_ptr(), &security) } != 0;
        let error = io::Error::last_os_error();
        // SAFETY: allocated by this_user_only.
        unsafe { LocalFree(descriptor) };
        if !created && error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error);
        }
        let user = current_user()?;
        let trusted = |sid: &str| sid == user || TRUSTED.contains(&sid);
        // The entry itself (a link someone else made), and what it links to:
        // whom its DACL lets in, whatever the rights.
        for follow in [false, true] {
            let (owner, allowed) = file_security(dir, follow, |_| true)?;
            if !trusted(&owner) || !allowed.iter().all(|s| trusted(s)) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not private to this user", dir.display())));
            }
        }
        Ok(())
    }

    /// See `super::create_private_file`.
    pub fn create_private_file(path: &Path) -> io::Result<File> {
        let wide = wide(path);
        let descriptor = this_user_only("")?;
        let security = attributes(descriptor);
        // SAFETY: `wide` is NUL-terminated; `security` and its descriptor
        // are alive during the call.
        let h = unsafe { CreateFileW(wide.as_ptr(), GENERIC_WRITE, 0, &security, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
        let error = io::Error::last_os_error();
        // SAFETY: allocated by this_user_only.
        unsafe { LocalFree(descriptor) };
        if h == INVALID_HANDLE_VALUE {
            return Err(error);
        }
        // SAFETY: a new handle, handed over to the File.
        Ok(unsafe { File::from_raw_handle(h) })
    }

    /// Checks that the pipe's server runs as this user: the pipe's name is
    /// predictable, so another user could have created it first.
    fn check_server(pipe: &File) -> io::Result<()> {
        let mut pid = 0;
        // SAFETY: a pipe handle and a valid out-parameter.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: plain call.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the pipe's server (process {pid}) isn't this user's")));
        }
        // SAFETY: a new handle, owned from now on.
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        if process_user(process.as_handle())? != current_user()? {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the pipe's server (process {pid}) isn't this user's")));
        }
        Ok(())
    }

    pub type Stream = File;

    pub fn connect(loc: &Location) -> io::Result<Stream> {
        open(loc, 0)
    }

    fn open(loc: &Location, flags: u32) -> io::Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        // All instances may be busy for a moment, between a connection and
        // the next instance.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).custom_flags(flags).open(loc.pipe()) {
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Ok(pipe) => {
                    check_server(&pipe)?;
                    return Ok(pipe);
                }
                r => return r,
            }
        }
    }

    /// A connection whose reads and writes give up after a while: the pipe
    /// opened for overlapped I/O, waited for with a timeout.
    pub struct TimedStream {
        pipe: File,
        event: OwnedHandle,
        timeout: Duration,
    }

    pub fn connect_timed(loc: &Location, timeout: Duration) -> io::Result<TimedStream> {
        let pipe = open(loc, FILE_FLAG_OVERLAPPED)?;
        // SAFETY: plain call; a manual-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new handle, owned from now on.
        Ok(TimedStream { pipe, event: unsafe { OwnedHandle::from_raw_handle(event) }, timeout })
    }

    impl TimedStream {
        /// Runs a read or write `start`ed with an OVERLAPPED, and waits for
        /// it, `timeout` at most (then cancelled).
        fn io(&mut self, reading: bool, start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32) -> io::Result<usize> {
            let h = self.pipe.as_raw_handle();
            let mut ov = OVERLAPPED { hEvent: self.event.as_raw_handle(), ..Default::default() };
            // The other end closed: the end of the stream, as for a File.
            let closed = |e: &io::Error| reading && e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32);
            if start(h, &mut ov) == 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(c) if c == ERROR_IO_PENDING as i32 => {}
                    _ if closed(&e) => return Ok(0),
                    _ => return Err(e),
                }
            }
            let mut n = 0;
            let millis = self.timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
            // SAFETY: `ov` stays alive (and the buffer borrowed) until the
            // I/O is over: done, or cancelled and waited for.
            if unsafe { GetOverlappedResultEx(h, &ov, &mut n, millis, 0) } == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                    // SAFETY: the I/O of `ov`, cancelled, then waited for.
                    unsafe {
                        CancelIoEx(h, &ov);
                        GetOverlappedResult(h, &ov, &mut n, 1);
                    }
                    return Err(io::Error::new(io::ErrorKind::TimedOut, format!("no answer in {:?}", self.timeout)));
                }
                return if closed(&e) { Ok(0) } else { Err(e) };
            }
            Ok(n as usize)
        }
    }

    impl io::Read for TimedStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            // SAFETY: `buf` is writable for `len` bytes until the I/O is over.
            self.io(true, |h, ov| unsafe { ReadFile(h, buf.as_mut_ptr(), len, std::ptr::null_mut(), ov) })
        }
    }

    impl io::Write for TimedStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            // SAFETY: `buf` is readable for `len` bytes until the I/O is over.
            self.io(false, |h, ov| unsafe { WriteFile(h, buf.as_ptr(), len, std::ptr::null_mut(), ov) })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct Listener {
        name: Vec<u16>,
        /// The instance the next client connects to.
        next: OwnedHandle,
        /// Only this user may open the pipe (its default DACL lets
        /// everyone read).
        descriptor: *mut c_void,
    }

    // SAFETY: the descriptor belongs to the listener (freed when it's
    // dropped), and only the thread that accepts uses it: moving it to that
    // thread is sound.
    unsafe impl Send for Listener {}

    fn instance(name: &[u16], first: bool, descriptor: *mut c_void) -> io::Result<OwnedHandle> {
        let mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let security = attributes(descriptor);
        // SAFETY: `name` is NUL-terminated; `security` and its descriptor
        // are alive during the call.
        let h = unsafe { CreateNamedPipeW(name.as_ptr(), mode, PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, &security) };
        // SAFETY: a new handle, owned from now on.
        if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedHandle::from_raw_handle(h) }) }
    }

    /// A security descriptor giving this user (and the system) access, and
    /// nobody else; with the ACE flags `inherit` (`OICI`: for what a
    /// directory will hold too).
    fn this_user_only(inherit: &str) -> io::Result<*mut c_void> {
        let user = current_user()?;
        let sddl: Vec<u16> = format!("D:P(A;{inherit};GA;;;{user})(A;{inherit};GA;;;SY)").encode_utf16().chain([0]).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string; freed by the Listener.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor)
    }

    impl Listener {
        /// Listens; fails when another daemon has the pipe.
        pub fn bind(loc: &Location) -> io::Result<Listener> {
            let name = wide(loc.pipe());
            let descriptor = this_user_only("")?;
            match instance(&name, true, descriptor) {
                Ok(next) => Ok(Listener { name, next, descriptor }),
                Err(e) => {
                    // SAFETY: allocated by this_user_only.
                    unsafe { LocalFree(descriptor) };
                    Err(e)
                }
            }
        }

        pub fn accept(&mut self) -> io::Result<Stream> {
            loop {
                // SAFETY: `next` is a pipe instance this listener owns.
                let ok = unsafe { ConnectNamedPipe(self.next.as_raw_handle(), std::ptr::null_mut()) } != 0;
                if !ok {
                    let e = io::Error::last_os_error();
                    match e.raw_os_error() {
                        Some(c) if c == ERROR_PIPE_CONNECTED as i32 => {}
                        // A client gone before it was accepted: the
                        // instance is made ready for the next one.
                        Some(c) if c == ERROR_NO_DATA as i32 => {
                            // SAFETY: as above.
                            unsafe { DisconnectNamedPipe(self.next.as_raw_handle()) };
                            continue;
                        }
                        _ => return Err(e),
                    }
                }
                // The connected instance, handed over to the File.
                let connected = std::mem::replace(&mut self.next, instance(&self.name, false, self.descriptor)?);
                return Ok(File::from(connected));
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // SAFETY: the descriptor is owned here.
            unsafe { LocalFree(self.descriptor) };
        }
    }

    /// Starts `exe` in the background, detached from the console (so that
    /// Ctrl-C in the build doesn't stop it), out of the build's job object
    /// when it allows that, without the variables `env_remove` and with
    /// those of `env_set`. It inherits no handle: std's spawn passes on
    /// every inheritable one, the pipes the build's output goes to
    /// included, which their reader (an IDE, `| tee`) would then wait on
    /// until the daemon exits, hours later.
    pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path, env_remove: &[&str], env_set: &[(&str, &std::ffi::OsStr)]) -> io::Result<Child> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
            STARTF_USESTDHANDLES, STARTUPINFOW,
        };
        let mut line = Vec::new();
        push_arg(&mut line, exe.as_os_str());
        for a in args {
            push_arg(&mut line, std::ffi::OsStr::new(a));
        }
        line.push(0);
        // Sorted by name, ignoring case, as Windows wants it.
        let mut vars: Vec<_> = std::env::vars_os()
            .filter(|(k, _)| !env_remove.iter().chain(env_set.iter().map(|(k, _)| k)).any(|r| k.eq_ignore_ascii_case(r)))
            .chain(env_set.iter().map(|(k, v)| (k.into(), v.into())))
            .collect();
        vars.sort_by_cached_key(|(k, _)| k.to_string_lossy().to_uppercase());
        let mut env = Vec::new();
        for (k, v) in vars {
            env.extend(k.encode_wide().chain(['=' as u16]).chain(v.encode_wide()).chain([0]));
        }
        env.extend([0, 0]);
        let (app, dir) = (wide(exe), wide(cwd));
        // No standard handles: the daemon writes to its log.
        let startup = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, dwFlags: STARTF_USESTDHANDLES, ..Default::default() };
        let spawn = |flags: u32| {
            let mut info = PROCESS_INFORMATION::default();
            // The call may change the command line it's given.
            let mut line = line.clone();
            // SAFETY: NUL-terminated strings, an environment block ending
            // with two NULs, valid structures; no handle inherited.
            let ok = unsafe {
                CreateProcessW(
                    app.as_ptr(),
                    line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    flags | CREATE_UNICODE_ENVIRONMENT,
                    env.as_ptr().cast(),
                    dir.as_ptr(),
                    &startup,
                    &mut info,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: handles the call returned: the thread's not used
            // again, the process' owned from now on.
            unsafe {
                CloseHandle(info.hThread);
                Ok(Child(OwnedHandle::from_raw_handle(info.hProcess)))
            }
        };
        spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP))
    }

    /// A process `spawn_detached` started.
    pub struct Child(OwnedHandle);

    impl Child {
        /// None while it runs; Ok once it exited successfully, else how it
        /// ended.
        pub fn exited(&mut self) -> Option<Result<(), String>> {
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
            let mut code = 0u32;
            // SAFETY: a process handle this owns, and a valid out-parameter.
            let ended =
                unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_OBJECT_0 && GetExitCodeProcess(self.0.as_raw_handle(), &mut code) != 0 };
            ended.then(|| match code {
                0 => Ok(()),
                c if c > 0xffff => Err(format!("exit code {c:#x}")),
                c => Err(format!("exit code {c}")),
            })
        }
    }

    /// Appends `arg` to a command line, as `CommandLineToArgvW` (and the C
    /// runtime) read it back: quoted when it has blanks or quotes, the
    /// backslashes before a quote doubled.
    pub(super) fn push_arg(line: &mut Vec<u16>, arg: &std::ffi::OsStr) {
        use std::os::windows::ffi::OsStrExt;
        let (quote, backslash, space, tab) = ('"' as u16, '\\' as u16, ' ' as u16, '\t' as u16);
        let arg: Vec<u16> = arg.encode_wide().collect();
        if !line.is_empty() {
            line.push(space);
        }
        if !arg.is_empty() && !arg.iter().any(|&c| c == space || c == tab || c == quote) {
            line.extend(arg);
            return;
        }
        line.push(quote);
        let mut backslashes = 0;
        for c in arg {
            if c == backslash {
                backslashes += 1;
            } else {
                if c == quote {
                    line.extend(std::iter::repeat_n(backslash, backslashes + 1));
                }
                backslashes = 0;
            }
            line.push(c);
        }
        line.extend(std::iter::repeat_n(backslash, backslashes));
        line.push(quote);
    }
}

pub use platform::{Child, Listener, Stream, TimedStream, connect, connect_timed, spawn_detached};

#[cfg(test)]
mod tests {
    use super::*;

    /// Clients that go before they're accepted don't stop the accept loop
    /// (Windows: `ConnectNamedPipe` then fails with `ERROR_NO_DATA`).
    #[test]
    fn early_closes() {
        use std::io::{Read, Write};
        let base = test_dirs::TestDir::new("ninja_daemon_ipc");
        let loc = Location { dir: base.join("daemon"), id: "early".into() };
        loc.create_dir().unwrap();
        let mut listener = Listener::bind(&loc).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    // The clients that went send nothing.
                    Ok(mut s) => {
                        let mut b = [0];
                        if s.read(&mut b).is_ok_and(|n| n == 1) && tx.send(Ok(b[0])).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
        });
        for _ in 0..2000 {
            drop(connect(&loc));
        }
        // Still accepting.
        let mut s = connect(&loc).unwrap();
        s.write_all(b"x").unwrap();
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(60)).expect("accepted").unwrap(), b'x');
    }

    /// The files of other daemons go, unless they run (they used to go
    /// all, those of running ones included); so do copies left half made a
    /// while ago (they never went, 20 MB each), not those being made.
    #[test]
    fn others_removed() {
        let base = test_dirs::TestDir::new("ninja_daemon_others");
        let at = |id: &str| Location { dir: base.join("daemon"), id: id.into() };
        let (me, running, gone) = (at("me"), at("running"), at("gone"));
        me.create_dir().unwrap();
        for loc in [&me, &running, &gone] {
            for f in [loc.lock_file(), loc.log_file(), loc.old_log_file(), loc.dir.join(format!("{}.exe", loc.id))] {
                std::fs::write(f, "").unwrap();
            }
            std::fs::create_dir(loc.cookie_dir()).unwrap();
            loc.note_start(Some("it failed"));
        }
        let (old, new) = (me.dir.join("gone.1234.tmp"), me.dir.join("gone.5678.tmp"));
        for f in [&old, &new] {
            std::fs::write(f, "a copy").unwrap();
        }
        let long_ago = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(long_ago).unwrap();
        let lock = std::fs::File::options().write(true).open(running.lock_file()).unwrap();
        lock.try_lock().unwrap();
        assert!(running.running() && !gone.running());
        me.remove_others().unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&me.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        let mine = ["me.cookies", "me.exe", "me.failed", "me.lock", "me.log", "me.log.1"];
        let running = ["running.cookies", "running.exe", "running.failed", "running.lock", "running.log", "running.log.1"];
        let expected: Vec<&str> = std::iter::once("gone.5678.tmp").chain(mine).chain(running).collect();
        assert_eq!(left, expected);
    }

    /// A binary has one daemon, however its path is spelled (`.`, case,
    /// slashes, a link to its directory): each spelling used to get its
    /// own, which `-t daemon status` and `stop` didn't see. Another binary
    /// (a copy) has another.
    #[test]
    fn one_daemon_per_binary() {
        let base = test_dirs::TestDir::new("ninja_daemon_spellings");
        std::fs::create_dir_all(base.join("bin")).unwrap();
        let exe = base.join("bin").join("ninja-rs");
        std::fs::write(&exe, "the binary").unwrap();
        let id = |p: &Path| Location::for_exe(p).unwrap().id;
        let mut spellings = vec![base.join("bin").join(".").join("ninja-rs")];
        let linked = base.join("linked");
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("bin"), &linked).unwrap();
        #[cfg(windows)]
        {
            let backslashes = |p: &Path| p.to_string_lossy().replace('/', "\\");
            let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J", &backslashes(&linked), &backslashes(&base.join("bin"))]).output().unwrap();
            assert!(made.status.success(), "{made:?}");
            spellings.push(PathBuf::from(exe.to_string_lossy().to_uppercase().replace('\\', "/")));
        }
        spellings.push(linked.join("ninja-rs"));
        for p in &spellings {
            assert_eq!(id(p), id(&exe), "{}", p.display());
        }
        let copy = base.join("copy");
        std::fs::copy(&exe, &copy).unwrap();
        std::fs::File::options().write(true).open(&copy).unwrap().set_modified(std::fs::metadata(&exe).unwrap().modified().unwrap()).unwrap();
        assert_ne!(id(&copy), id(&exe));
    }

    /// Lets everyone change `dir`.
    fn share(dir: &Path) {
        #[cfg(unix)]
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
        #[cfg(windows)]
        assert!(std::process::Command::new("icacls").arg(dir).args(["/grant", "*S-1-1-0:(OI)(CI)M"]).output().unwrap().status.success());
    }

    fn private(dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        return private_dir(dir);
        #[cfg(windows)]
        return platform::private_dir(dir);
    }

    /// A directory others may change is refused (another user's files, a
    /// shared directory); one made in it is private.
    #[test]
    fn private_directories() {
        let base = test_dirs::TestDir::new("ninja-daemon-private");
        let shared = base.join("shared");
        std::fs::create_dir(&shared).unwrap();
        share(&shared);
        assert_eq!(private(&shared).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let mine = shared.join("mine");
        private(&mine).unwrap();
        private(&mine).unwrap();
    }

    /// Unix: the daemon keeps none of the build's descriptors (a pipe its
    /// caller reads, inherited without close-on-exec), whose reader would
    /// wait for it.
    #[cfg(unix)]
    #[test]
    fn spawned_without_descriptors() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let dir = test_dirs::TestDir::new("ninja_daemon_spawn");
        let mut fds = [0; 2];
        // SAFETY: a valid array for the two descriptors, owned from now on;
        // only the end written to is inherited.
        let (read, write) = unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1]))
        };
        let pid_file = dir.join("pid");
        let script = format!("echo $$ > '{}'.tmp && mv '{0}'.tmp '{0}' && exec sleep 600", pid_file.display());
        spawn_detached(Path::new("/bin/sh"), &["-c", &script], &dir, &[], &[]).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse().ok()) {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "the process didn't start");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        drop(write);
        // The end of the pipe, though the process runs.
        let mut pfd = libc::pollfd { fd: read.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, then plain calls.
        let ended = unsafe { libc::poll(&mut pfd, 1, 60_000) == 1 && pfd.revents & libc::POLLHUP != 0 };
        // SAFETY: plain call.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(ended, "the spawned process holds the pipe");
    }

    /// Windows: arguments are read back as they were (the daemon's binary
    /// and its directory may have blanks in their paths).
    #[cfg(windows)]
    #[test]
    fn command_lines() {
        let args = [r"C:\Program Files\ninja-rs.exe", "-t", "", "a b", r#"say "hi""#, r"dir\", r"dir with space\", r#"x\"y"#];
        let mut line = Vec::new();
        for a in args {
            platform::push_arg(&mut line, std::ffi::OsStr::new(a));
        }
        assert_eq!(cpp_deps::external::split_windows(&String::from_utf16(&line).unwrap(), true), args);
    }

    /// A private file is a new one: never one someone else made first.
    #[test]
    fn private_files_are_new() {
        let dir = test_dirs::TestDir::new("ninja-daemon-private-file");
        let path = dir.join("secret");
        create_private_file(&path).unwrap();
        assert_eq!(create_private_file(&path).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }
}
