//! Where a daemon listens, and local connections to it: Unix sockets in a
//! directory only the user can enter, Windows named pipes (which other
//! users can't write to, and remote clients can't open).

use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

/// A daemon's files (lock, log, socket) and its name.
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
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).recursive(true).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
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

    pub fn connect(loc: &Location) -> io::Result<Stream> {
        UnixStream::connect(loc.socket())
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

        pub fn accept(&mut self) -> io::Result<Stream> {
            self.0.accept().map(|(s, _)| s)
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
            security: *const c_void,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    pub type Stream = File;

    pub fn connect(loc: &Location) -> io::Result<Stream> {
        // All instances may be busy for a moment, between a connection and
        // the next instance.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(loc.pipe()) {
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                r => return r,
            }
        }
    }

    pub struct Listener {
        name: Vec<u16>,
        /// The instance the next client connects to.
        next: Handle,
    }

    // The handle is only used by the thread that accepts.
    unsafe impl Send for Listener {}

    fn instance(name: &[u16], first: bool) -> io::Result<Handle> {
        let mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
        // SAFETY: `name` is NUL-terminated; no security attributes.
        let h = unsafe { CreateNamedPipeW(name.as_ptr(), mode, PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, std::ptr::null()) };
        if h == INVALID_HANDLE_VALUE { Err(io::Error::last_os_error()) } else { Ok(h) }
    }

    impl Listener {
        /// Listens; fails when another daemon has the pipe.
        pub fn bind(loc: &Location) -> io::Result<Listener> {
            let name: Vec<u16> = std::ffi::OsStr::new(&loc.pipe()).encode_wide().chain([0]).collect();
            let next = instance(&name, true)?;
            Ok(Listener { name, next })
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
            let connected = std::mem::replace(&mut self.next, instance(&self.name, false)?);
            // SAFETY: the connected instance is handed over to the File.
            Ok(unsafe { File::from_raw_handle(connected) })
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // SAFETY: the pending instance is owned here.
            unsafe { CloseHandle(self.next) };
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
