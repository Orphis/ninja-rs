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

    /// Creates the directories, only the user's on Unix.
    pub fn create_dir(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            private_dir(&self.dir)?;
            match self.socket().parent() {
                Some(d) if d != self.dir => private_dir(d),
                _ => Ok(()),
            }
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&self.dir)
    }
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
                let (s, _) = self.0.accept()?;
                if check_peer(&s).is_ok() {
                    return Ok(s);
                }
            }
        }
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
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::time::{Duration, Instant};

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const PIPE_ACCESS_DUPLEX: u32 = 0x3;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const ERROR_PIPE_CONNECTED: i32 = 535;
    const ERROR_PIPE_BUSY: i32 = 231;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const TOKEN_QUERY: u32 = 0x8;
    const TOKEN_USER: i32 = 1;
    const SDDL_REVISION_1: u32 = 1;

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            security: *const SecurityAttributes,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut u32) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetCurrentProcess() -> Handle;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(token: Handle, class: i32, info: *mut c_void, len: u32, returned: *mut u32) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl: *const u16, revision: u32, sd: *mut *mut c_void, size: *mut u32) -> i32;
    }

    /// The SID of the user `process` runs as, as a string (`S-1-5-21-...`).
    fn process_user(process: Handle) -> io::Result<String> {
        let mut token: Handle = std::ptr::null_mut();
        // SAFETY: valid out-parameter; the token is closed below.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // TOKEN_USER: a pointer to the SID (in the buffer), then attributes.
        let mut buf = vec![0u64; 64];
        let mut len = 0;
        // SAFETY: the buffer is large enough for a TOKEN_USER and its SID.
        let ok = unsafe { GetTokenInformation(token, TOKEN_USER, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len) };
        // SAFETY: opened above.
        unsafe { CloseHandle(token) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = buf[0] as usize as *mut c_void;
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` points into `buf`, alive here; the string is freed below.
        if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a NUL-terminated string from ConvertSidToStringSidW.
        let s = unsafe {
            let n = (0..).take_while(|&i| *text.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
        };
        // SAFETY: allocated by ConvertSidToStringSidW.
        unsafe { LocalFree(text.cast()) };
        Ok(s)
    }

    /// Checks that the pipe's server runs as this user: the pipe's name is
    /// predictable, so another user could have created it first.
    fn check_server(pipe: &File) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        let mut pid = 0;
        // SAFETY: a pipe handle and a valid out-parameter.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the pipe's server (process {pid}) isn't this user's")));
        }
        let theirs = process_user(process);
        // SAFETY: opened above.
        unsafe { CloseHandle(process) };
        // SAFETY: the pseudo-handle of this process.
        if theirs? != process_user(unsafe { GetCurrentProcess() })? {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the pipe's server (process {pid}) isn't this user's")));
        }
        Ok(())
    }

    pub type Stream = File;

    pub fn connect(loc: &Location) -> io::Result<Stream> {
        // All instances may be busy for a moment, between a connection and
        // the next instance.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(loc.pipe()) {
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Ok(pipe) => {
                    check_server(&pipe)?;
                    return Ok(pipe);
                }
                r => return r,
            }
        }
    }

    pub struct Listener {
        name: Vec<u16>,
        /// The instance the next client connects to.
        next: Handle,
        /// Only this user may open the pipe (its default DACL lets
        /// everyone read).
        descriptor: *mut c_void,
    }

    // The handles are only used by the thread that accepts.
    unsafe impl Send for Listener {}

    fn instance(name: &[u16], first: bool, descriptor: *mut c_void) -> io::Result<Handle> {
        let mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        let security = SecurityAttributes { length: std::mem::size_of::<SecurityAttributes>() as u32, descriptor, inherit: 0 };
        // SAFETY: `name` is NUL-terminated; `security` and its descriptor
        // are alive during the call.
        let h = unsafe { CreateNamedPipeW(name.as_ptr(), mode, PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, &security) };
        if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(h) }
    }

    /// A security descriptor giving this user (and the system) access, and
    /// nobody else.
    fn this_user_only() -> io::Result<*mut c_void> {
        // SAFETY: the pseudo-handle of this process.
        let user = process_user(unsafe { GetCurrentProcess() })?;
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{user})(A;;GA;;;SY)").encode_utf16().chain([0]).collect();
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
            let name: Vec<u16> = std::ffi::OsStr::new(&loc.pipe()).encode_wide().chain([0]).collect();
            let descriptor = this_user_only()?;
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
            // SAFETY: `next` is a pipe instance this listener owns.
            let ok = unsafe { ConnectNamedPipe(self.next, std::ptr::null_mut()) } != 0;
            if !ok {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(ERROR_PIPE_CONNECTED) {
                    return Err(e);
                }
            }
            let connected = std::mem::replace(&mut self.next, instance(&self.name, false, self.descriptor)?);
            // SAFETY: the connected instance is handed over to the File.
            Ok(unsafe { File::from_raw_handle(connected) })
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // SAFETY: the pending instance and the descriptor are owned here.
            unsafe {
                CloseHandle(self.next);
                LocalFree(self.descriptor);
            }
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

pub use platform::{Listener, Stream, connect, spawn_detached};
