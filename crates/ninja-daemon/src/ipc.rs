//! Where a daemon listens, and local connections to it: Unix sockets in a
//! directory only the user can enter, Windows named pipes (which other
//! users can't write to, and remote clients can't open).

use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

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

impl Location {
    /// The daemon of the ninja-rs binary `exe`, for this user: a rebuilt
    /// binary gets its own (the old one exits when idle). The directory is
    /// `NINJA_RS_DAEMON_DIR` when set.
    pub fn for_exe(exe: &Path) -> io::Result<Location> {
        let meta = std::fs::metadata(exe)?;
        let mtime = meta.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let id = short_hash(&[exe.to_string_lossy().as_bytes(), &meta.len().to_le_bytes(), &mtime.to_le_bytes()]);
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
        Ok(std::env::temp_dir().join(format!("ninja-rs-{uid}")))
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
    /// (so that Ctrl-C in the build doesn't stop it).
    pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        Command::new(exe).args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn().map(|_| ())
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
    /// when it allows that.
    pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let spawn = |flags: u32| {
            Command::new(exe).args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(flags).spawn()
        };
        spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
            .or_else(|_| spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP))
            .map(|_| ())
    }
}

pub use platform::{Listener, Stream, TimedStream, connect, connect_timed, spawn_detached};

#[cfg(test)]
mod tests {
    use super::*;

    /// Clients that go before they're accepted don't stop the accept loop
    /// (Windows: `ConnectNamedPipe` then fails with `ERROR_NO_DATA`).
    #[test]
    fn early_closes() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("ninja_daemon_ipc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let loc = Location { dir: dir.clone(), id: "early".into() };
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
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(10)).expect("accepted").unwrap(), b'x');
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A private file is a new one: never one someone else made first.
    #[test]
    fn private_files_are_new() {
        let dir = test_dirs::TestDir::new("ninja-daemon-private-file");
        let path = dir.join("secret");
        create_private_file(&path).unwrap();
        assert_eq!(create_private_file(&path).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }
}
