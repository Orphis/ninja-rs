//! GNU make jobserver, like ninja 1.13: a client that takes a job slot from
//! the pool of the `make` (or `cargo`, or ninja) that started ninja-rs for
//! each local command, and the pool that `--jobserver-pool` creates for the
//! commands ninja-rs runs.
//!
//! Protocols: the named FIFO (`--jobserver-auth=fifo:PATH`, GNU make 4.4)
//! and the inherited pipe (`--jobserver-auth=R,W` or `--jobserver-fds=R,W`,
//! GNU make 4.3 and older; upstream ninja only has it in Kitware's fork) on
//! Unix, the named semaphore (`--jobserver-auth=NAME`) on Windows. Pools are
//! FIFOs on Unix and semaphores on Windows.
//!
//! Every client owns one implicit slot that isn't in the pool; each other
//! job takes a token from the pool and writes the same byte back when done.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ninja_core::status::{Status, Verbosity};

/// The pool `MAKEFLAGS` names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// No pool, or a disabled one.
    #[default]
    None,
    /// Pipe descriptors inherited from the parent.
    Pipe {
        read: i32,
        write: i32,
    },
    Fifo(String),
    /// Name of a Windows semaphore.
    Semaphore(String),
}

/// Parses `MAKEFLAGS` like ninja's `Jobserver::ParseMakeFlagsValue`: the
/// last jobserver argument wins.
pub fn parse_makeflags(flags: &str) -> Result<Mode, String> {
    let args: Vec<&str> = flags.split([' ', '\t']).filter(|a| !a.is_empty()).collect();
    // The first word holds make's single-letter flags, unless it starts with
    // a dash: an `n` there is `make -n`, which runs nothing.
    if let Some(first) = args.first()
        && !first.starts_with('-')
        && first.contains('n')
    {
        return Ok(Mode::None);
    }
    let mut mode = Mode::None;
    for arg in args {
        if let Some(v) = arg.strip_prefix("--jobserver-auth=") {
            mode = match fd_pair(v) {
                Some(m) => m,
                None => match v.strip_prefix("fifo:") {
                    Some(path) => Mode::Fifo(path.to_string()),
                    None => Mode::Semaphore(v.to_string()),
                },
            };
        } else if let Some(v) = arg.strip_prefix("--jobserver-fds=") {
            // GNU make's name for `--jobserver-auth=R,W` before 4.2.
            mode = fd_pair(v).ok_or_else(|| format!("Invalid file descriptor pair [{v}]"))?;
        }
    }
    Ok(mode)
}

fn fd_pair(v: &str) -> Option<Mode> {
    let (r, w) = v.split_once(',')?;
    let (read, write) = (r.parse::<i32>().ok()?, w.parse::<i32>().ok()?);
    // A negative descriptor disables the jobserver.
    Some(if read < 0 || write < 0 { Mode::None } else { Mode::Pipe { read, write } })
}

/// [`parse_makeflags`], rejecting the protocols this platform doesn't have.
pub fn parse_native_makeflags(flags: &str) -> Result<Mode, String> {
    let mode = parse_makeflags(flags)?;
    match mode {
        #[cfg(windows)]
        Mode::Fifo(_) => Err("FIFO mode is not supported on Windows!".into()),
        #[cfg(windows)]
        Mode::Pipe { .. } => Err("Pipe mode is not supported on Windows!".into()),
        #[cfg(not(windows))]
        Mode::Semaphore(_) => Err("Semaphore mode is not supported on Posix!".into()),
        m => Ok(m),
    }
}

/// The `-jN` in `MAKEFLAGS` (GNU make 4.2 and later, and ninja's pools, put
/// it next to the jobserver arguments).
pub fn makeflags_jobs(flags: &str) -> Option<usize> {
    flags.split([' ', '\t']).filter_map(|a| a.strip_prefix("-j")?.parse::<usize>().ok()).rfind(|&n| n > 0)
}

/// Tokens other processes give back aren't signalled: waiting polls up to
/// this often.
const MAX_POLL: Duration = Duration::from_millis(10);

pub struct Client {
    sys: sys::Client,
    implicit_free: AtomicBool,
    /// Waiters take turns in the order they asked, which is the order the
    /// engine started their commands in (by priority).
    turn: tokio::sync::Mutex<()>,
    released: tokio::sync::Notify,
}

/// A job slot; dropping it gives it back.
pub struct Slot {
    client: Arc<Client>,
    /// `None` for the implicit slot.
    token: Option<u8>,
}

impl Client {
    pub fn new(mode: &Mode) -> Result<Arc<Self>, String> {
        let sys = sys::Client::new(mode)?;
        Ok(Arc::new(Self { sys, implicit_free: AtomicBool::new(true), turn: tokio::sync::Mutex::new(()), released: tokio::sync::Notify::new() }))
    }

    /// Waits for a job slot: the implicit one, or a token from the pool.
    pub async fn acquire(self: &Arc<Self>) -> Slot {
        let _turn = self.turn.lock().await;
        let mut delay = Duration::from_millis(1);
        loop {
            if let Some(slot) = self.try_acquire() {
                return slot;
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.released.notified() => {}
            }
            delay = (delay * 2).min(MAX_POLL);
        }
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<Slot> {
        let token = if self.implicit_free.swap(false, Ordering::AcqRel) { None } else { Some(self.sys.try_acquire()?) };
        Some(Slot { client: self.clone(), token })
    }

    /// A slot if one is free now and nobody waits for one: those waiting
    /// (`acquire`) take theirs first.
    pub fn try_acquire_in_turn(self: &Arc<Self>) -> Option<Slot> {
        let _turn = self.turn.try_lock().ok()?;
        self.try_acquire()
    }
}

impl Slot {
    pub fn is_implicit(&self) -> bool {
        self.token.is_none()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        match self.token {
            None => self.client.implicit_free.store(true, Ordering::Release),
            Some(t) => self.client.sys.release(t),
        }
        self.client.released.notify_one();
    }
}

/// The pool `--jobserver-pool` creates: `jobs - 1` tokens, ninja-rs holding
/// the implicit slot. Removed when dropped.
pub struct Pool {
    sys: sys::Pool,
    jobs: usize,
}

impl Pool {
    pub fn new(jobs: usize) -> Result<Self, String> {
        if jobs < 2 {
            return Err("At least 2 job slots needed".into());
        }
        Ok(Self { sys: sys::Pool::new(jobs)?, jobs })
    }

    /// The `MAKEFLAGS` value that hands the pool to commands.
    pub fn makeflags(&self) -> String {
        format!(" -j{} --jobserver-auth={}", self.jobs, self.sys.auth())
    }
}

/// What sets up a build's jobserver.
pub struct Settings {
    pub parallelism: usize,
    /// `-j` was given: an external pool is ignored, unless `pool`.
    pub explicit_parallelism: bool,
    /// `--jobserver-pool`, or `enable_jobserver_pool = 1` in the manifest.
    pub pool: bool,
    pub dry_run: bool,
    pub verbosity: Verbosity,
}

/// A build's jobserver.
#[derive(Default)]
pub struct Jobserver {
    /// The pool ninja-rs made; removed when dropped.
    pub pool: Option<Pool>,
    /// Local tasks take their slot from this.
    pub client: Option<Arc<Client>>,
    /// `MAKEFLAGS` for local commands, naming ninja-rs's pool.
    pub makeflags: Option<String>,
    /// How many jobs the pool is for, when known (its `-jN`).
    pub jobs: Option<usize>,
}

/// ninja's `HasExternalJobserverPool`: the pool `makeflags` names, or why
/// there's none: an empty reason when there's no `MAKEFLAGS`.
fn external_pool(makeflags: Option<&str>) -> Result<Mode, String> {
    let Some(flags) = makeflags else { return Err(String::new()) };
    match parse_native_makeflags(flags)? {
        Mode::None => Err("external pool is disabled".into()),
        mode => Ok(mode),
    }
}

/// Sets up the pool and the client like ninja's `JobserverState`, with the
/// same messages. `makeflags` is the `MAKEFLAGS` environment variable.
pub fn setup(s: &Settings, makeflags: Option<String>, status: &mut dyn Status) -> Jobserver {
    let verbose = s.verbosity == Verbosity::Verbose;
    let quiet = s.verbosity == Verbosity::Quiet;
    let mut js = Jobserver::default();
    let mut makeflags = makeflags;

    let skip_pool = if s.parallelism == 1 {
        Some("no parallelism (-j1) specified".to_string())
    } else if s.dry_run {
        Some("dry-run mode".to_string())
    } else {
        match external_pool(makeflags.as_deref()) {
            Ok(_) => Some("external pool detected".to_string()),
            Err(e) if !e.is_empty() => Some(e),
            Err(_) => (!s.pool).then(String::new),
        }
    };
    match skip_pool {
        Some(reason) => {
            if !reason.is_empty() && verbose {
                status.info(&format!("not creating a jobserver pool: {reason}"));
            }
        }
        None => {
            if verbose {
                status.info(&format!("Creating jobserver pool for {} parallel jobs", s.parallelism));
            }
            match Pool::new(s.parallelism) {
                Ok(pool) => {
                    let flags = pool.makeflags();
                    makeflags = Some(flags.clone());
                    js.makeflags = Some(flags);
                    js.pool = Some(pool);
                }
                Err(e) => {
                    if !quiet {
                        status.warning(&format!("Jobserver pool creation failed: {e}"));
                    }
                }
            }
        }
    }

    let mode = if s.dry_run {
        Err("Dry-run mode".to_string())
    } else if s.explicit_parallelism && !s.pool {
        Err("Explicit parallelism specified".to_string())
    } else {
        external_pool(makeflags.as_deref())
    };
    let flags = makeflags.as_deref().unwrap_or("");
    match mode {
        Err(reason) => {
            if !reason.is_empty() && verbose {
                status.warning(&format!("ignoring jobserver: {reason} [{flags}]"));
            }
        }
        Ok(mode) => {
            if !quiet && s.verbosity != Verbosity::NoStatusUpdate {
                status.info(&format!("Jobserver mode detected: {flags}"));
                if matches!(mode, Mode::Pipe { .. }) {
                    status.warning("Jobserver 'pipe' mode detected, a pool that implements 'fifo' mode would be more reliable!");
                }
            }
            match Client::new(&mode) {
                Ok(client) => {
                    js.client = Some(client);
                    js.jobs = makeflags_jobs(flags);
                }
                Err(e) => {
                    if !quiet {
                        status.error(&format!("Could not initialize jobserver: {e}"));
                    }
                }
            }
        }
    }
    js
}

#[cfg(unix)]
mod sys {
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Mode;

    pub struct Client {
        read: File,
        write: File,
        /// The read end is a description of its own, non-blocking (see
        /// `pipe`); else it is the pool's, read only when ready.
        own: bool,
    }

    impl Client {
        pub fn new(mode: &Mode) -> Result<Self, String> {
            match mode {
                Mode::Fifo(path) => Self::fifo(path),
                &Mode::Pipe { read, write } => Self::pipe(read, write),
                _ => Err("Unsupported jobserver mode".into()),
            }
        }

        fn fifo(path: &str) -> Result<Self, String> {
            if path.is_empty() {
                return Err("Empty fifo path".into());
            }
            let open = |write: bool| OpenOptions::new().read(!write).write(write).custom_flags(libc::O_NONBLOCK).open(path);
            let read = open(false).map_err(|e| format!("Error opening fifo for reading: {e}"))?;
            let is_pool = read.metadata().is_ok_and(|m| m.file_type().is_fifo() || m.file_type().is_char_device());
            if !is_pool {
                return Err(format!("Not a fifo path: {path}"));
            }
            let write = open(true).map_err(|e| format!("Error opening fifo for writing: {e}"))?;
            Ok(Self { read, write, own: true })
        }

        fn pipe(read: i32, write: i32) -> Result<Self, String> {
            // make 4.3 and older close the descriptors for commands that
            // aren't recursive make invocations, so they may be closed or be
            // other files.
            if !is_pool_descriptor(read) || !is_pool_descriptor(write) {
                return Err("Invalid file descriptors".into());
            }
            // Closed on exec: commands inherit the originals. Their open
            // file descriptions are make's and its other clients': made
            // non-blocking, they would be for them too (make 3.81 fails
            // reading a non-blocking pipe). Where Linux gives the pipe a
            // description of its own (`/proc/self/fd`), that one is.
            let write = duplicate(write).ok_or("Could not duplicate write descriptor")?;
            if let Some(read) = reopen(read) {
                return Ok(Self { read, write, own: true });
            }
            let read = duplicate(read).ok_or("Could not duplicate read descriptor")?;
            Ok(Self { read, write, own: false })
        }

        pub fn try_acquire(&self) -> Option<u8> {
            let mut b = [0u8];
            loop {
                let read = if self.own { (&self.read).read(&mut b) } else { read_now(&self.read, &mut b) };
                match read {
                    Ok(1) => return Some(b[0]),
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    _ => return None,
                }
            }
        }

        pub fn release(&self, token: u8) {
            // Nothing can be done when this fails.
            let _ = (&self.write).write_all(&[token]);
        }
    }

    /// A pipe, or a socket (pools over a socket pair work the same).
    fn is_pool_descriptor(fd: i32) -> bool {
        // SAFETY: F_GETFD only checks that the descriptor is open.
        if fd < 0 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return false;
        }
        // SAFETY: the descriptor is open, and only borrowed for the call.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        fd.try_clone_to_owned().map(File::from).and_then(|f| f.metadata()).is_ok_and(|m| m.file_type().is_fifo() || m.file_type().is_socket())
    }

    /// A duplicate, closed on exec, sharing the original's description.
    fn duplicate(fd: i32) -> Option<File> {
        // SAFETY: `is_pool_descriptor` checked that the descriptor is open.
        Some(File::from(unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned().ok()?))
    }

    /// The pipe `fd` reads from, opened again for reading: a description of
    /// its own, non-blocking (Linux; not for sockets).
    fn reopen(fd: i32) -> Option<File> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(format!("/proc/self/fd/{fd}")).ok()?;
        file.metadata().is_ok_and(|m| m.file_type().is_fifo()).then_some(file)
    }

    /// Reads from the pool's description without waiting: from a socket,
    /// with `MSG_DONTWAIT`; from a pipe, once it is readable (a token
    /// another process takes meanwhile makes it wait for the next one).
    fn read_now(file: &File, buf: &mut [u8]) -> std::io::Result<usize> {
        let fd = file.as_raw_fd();
        // SAFETY: a buffer of the length given.
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOTSOCK) {
            return Err(e);
        }
        let mut ready = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, not waiting.
        if unsafe { libc::poll(&mut ready, 1, 0) } != 1 {
            return Err(ErrorKind::WouldBlock.into());
        }
        let mut file = file;
        file.read(buf)
    }

    pub struct Pool {
        path: String,
        /// Keeps the FIFO's tokens alive while no command has it open.
        _keep: Option<File>,
    }

    impl Pool {
        pub fn new(jobs: usize) -> Result<Self, String> {
            // Unique per process, and per pool within the process (tests).
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
            let suffix = if n == 0 { String::new() } else { format!("_{n}") };
            let path = format!("{dir}/NinjaFIFO{}{suffix}", std::process::id());
            let c_path = std::ffi::CString::new(path.clone()).map_err(|_| "Cannot create fifo path!".to_string())?;
            // Owner only, like GNU make: anyone who can open the FIFO can take
            // tokens or add some.
            // SAFETY: a valid NUL-terminated path.
            if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } < 0 {
                return Err(format!("Cannot create fifo: {}", std::io::Error::last_os_error()));
            }
            // From here, dropping the pool removes the FIFO.
            let mut pool = Self { path, _keep: None };
            // Non-blocking: more tokens than the FIFO holds (`-j0`) fail.
            let file =
                OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&pool.path).map_err(|e| format!("Could not open fifo: {e}"))?;
            // '+' like GNU make.
            (&file).write_all(&vec![b'+'; jobs - 1]).map_err(|e| format!("Could not fill job slots pool: {e}"))?;
            pool._keep = Some(file);
            Ok(pool)
        }

        pub fn auth(&self) -> String {
            format!("fifo:{}", self.path)
        }
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::CString;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{CreateSemaphoreA, ReleaseSemaphore, SEMAPHORE_MODIFY_STATE, WaitForSingleObject};
    use windows_sys::Win32::System::WindowsProgramming::OpenSemaphoreA;

    use super::Mode;

    /// An owned semaphore handle.
    struct Semaphore(OwnedHandle);

    impl Semaphore {
        /// Owns `h`, a new handle (None: the call that made it failed).
        fn new(h: HANDLE) -> Option<Semaphore> {
            // SAFETY: a new handle, owned from now on.
            (!h.is_null()).then(|| Semaphore(unsafe { OwnedHandle::from_raw_handle(h) }))
        }
    }

    fn c_name(name: &str) -> Result<CString, String> {
        CString::new(name).map_err(|_| format!("Invalid semaphore name: {name}"))
    }

    pub struct Client {
        semaphore: Semaphore,
    }

    impl Client {
        pub fn new(mode: &Mode) -> Result<Self, String> {
            let Mode::Semaphore(name) = mode else { return Err("Unsupported jobserver mode".into()) };
            let c = c_name(name)?;
            // SAFETY: a valid NUL-terminated name.
            let h = unsafe { OpenSemaphoreA(SYNCHRONIZE | SEMAPHORE_MODIFY_STATE, 0, c.as_ptr().cast()) };
            let semaphore = Semaphore::new(h).ok_or_else(|| format!("Error opening semaphore: {}", std::io::Error::last_os_error()))?;
            Ok(Self { semaphore })
        }

        pub fn try_acquire(&self) -> Option<u8> {
            // SAFETY: the handle is open. A zero timeout doesn't block.
            // Semaphore tokens have no value: always 1, like ninja.
            (unsafe { WaitForSingleObject(self.semaphore.0.as_raw_handle(), 0) } == WAIT_OBJECT_0).then_some(1)
        }

        pub fn release(&self, _token: u8) {
            // SAFETY: the handle is open. Nothing can be done when this fails.
            unsafe { ReleaseSemaphore(self.semaphore.0.as_raw_handle(), 1, std::ptr::null_mut()) };
        }
    }

    pub struct Pool {
        name: String,
        _semaphore: Semaphore,
    }

    impl Pool {
        pub fn new(jobs: usize) -> Result<Self, String> {
            // Unique per process, and per pool within the process (tests).
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
            let name = format!("ninja_jobserver_pool_{}_{n}", std::process::id());
            let c = c_name(&name)?;
            let count = i32::try_from(jobs - 1).unwrap_or(i32::MAX);
            // SAFETY: a valid NUL-terminated name, default security.
            let h = unsafe { CreateSemaphoreA(std::ptr::null(), count, count, c.as_ptr().cast()) };
            let semaphore = Semaphore::new(h).ok_or_else(|| format!("Could not create semaphore: {}", std::io::Error::last_os_error()))?;
            Ok(Self { name, _semaphore: semaphore })
        }

        pub fn auth(&self) -> String {
            self.name.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_makeflags() {
        let p = |s: &str| parse_makeflags(s).unwrap();
        assert_eq!(p(""), Mode::None);
        assert_eq!(p("ks"), Mode::None);
        assert_eq!(p(" -j"), Mode::None);
        assert_eq!(p(" -j3 --jobserver-auth=3,4"), Mode::Pipe { read: 3, write: 4 });
        assert_eq!(p("ks -j3 --jobserver-auth=3,4"), Mode::Pipe { read: 3, write: 4 });
        assert_eq!(p("--jobserver-fds=5,6"), Mode::Pipe { read: 5, write: 6 });
        assert_eq!(p("-j --jobserver-fds=5,6 --jobserver-auth=7,8"), Mode::Pipe { read: 7, write: 8 });
        assert_eq!(p("\t-j4\t--jobserver-auth=fifo:/tmp/GMfifo1"), Mode::Fifo("/tmp/GMfifo1".into()));
        assert_eq!(p(" -j4 --jobserver-auth=gmake_semaphore_1"), Mode::Semaphore("gmake_semaphore_1".into()));
        // The last one wins.
        assert_eq!(p("--jobserver-auth=fifo:/a --jobserver-auth=fifo:/b"), Mode::Fifo("/b".into()));
        // Negative descriptors disable it.
        assert_eq!(p("--jobserver-auth=3,4 --jobserver-auth=-1,-1"), Mode::None);
        // `make -n`.
        assert_eq!(p("kns --jobserver-auth=fifo:/a"), Mode::None);
        assert_eq!(p("-n --jobserver-auth=fifo:/a"), Mode::Fifo("/a".into()));
        assert!(parse_makeflags("--jobserver-fds=x").is_err());
    }

    #[test]
    fn native_protocols() {
        #[cfg(windows)]
        {
            assert!(parse_native_makeflags("--jobserver-auth=fifo:/a").is_err());
            assert!(parse_native_makeflags("--jobserver-auth=3,4").is_err());
            assert!(parse_native_makeflags("--jobserver-auth=sem").is_ok());
        }
        #[cfg(not(windows))]
        {
            assert!(parse_native_makeflags("--jobserver-auth=sem").is_err());
            assert!(parse_native_makeflags("--jobserver-auth=3,4").is_ok());
            assert!(parse_native_makeflags("--jobserver-auth=fifo:/a").is_ok());
        }
    }

    #[test]
    fn jobs_from_makeflags() {
        assert_eq!(makeflags_jobs(" -j8 --jobserver-auth=fifo:/a"), Some(8));
        assert_eq!(makeflags_jobs("-j --jobserver-auth=3,4"), None);
        assert_eq!(makeflags_jobs("ks -j3"), Some(3));
        assert_eq!(makeflags_jobs("--jobserver-auth=3,4"), None);
    }

    #[test]
    fn pool_and_client() {
        let pool = Pool::new(3).unwrap();
        let flags = pool.makeflags();
        assert!(flags.starts_with(" -j3 --jobserver-auth="), "{flags}");
        let client = Client::new(&parse_native_makeflags(&flags).unwrap()).unwrap();
        // The implicit slot, then the pool's two tokens.
        let a = client.try_acquire().unwrap();
        assert!(a.is_implicit());
        let b = client.try_acquire().unwrap();
        let c = client.try_acquire().unwrap();
        assert!(!b.is_implicit() && !c.is_implicit());
        assert!(client.try_acquire().is_none());
        // Tokens go back to the pool, the implicit slot to the client.
        drop(b);
        let d = client.try_acquire().unwrap();
        assert!(!d.is_implicit());
        drop(a);
        assert!(client.try_acquire().unwrap().is_implicit());
        drop((c, d));
        // A second client shares the pool, with its own implicit slot.
        let other = Client::new(&parse_native_makeflags(&flags).unwrap()).unwrap();
        let held: Vec<Slot> = std::iter::from_fn(|| other.try_acquire()).collect();
        assert_eq!(held.len(), 3);
        let implicit = client.try_acquire().unwrap();
        assert!(implicit.is_implicit());
        assert!(client.try_acquire().is_none());
    }

    #[test]
    fn acquire_waits_for_release() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        rt.block_on(async {
            let pool = Pool::new(2).unwrap();
            let client = Client::new(&parse_native_makeflags(&pool.makeflags()).unwrap()).unwrap();
            let a = client.acquire().await;
            let b = client.acquire().await;
            let waiter = {
                let client = client.clone();
                tokio::spawn(async move { client.acquire().await.is_implicit() })
            };
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(!waiter.is_finished());
            drop(b);
            // The token is the waiter's, not one asking without waiting.
            assert!(client.try_acquire_in_turn().is_none(), "a slot taken out of turn");
            assert!(!waiter.await.unwrap());
            drop(a);
            assert!(client.try_acquire_in_turn().is_some());
        });
    }

    #[cfg(unix)]
    #[test]
    fn pool_fifo_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let pool = Pool::new(2).unwrap();
        let flags = pool.makeflags();
        let path = flags.split("fifo:").nth(1).unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn socket_client() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let client = Client::new(&Mode::Pipe { read: a.as_raw_fd(), write: a.as_raw_fd() }).unwrap();
        let _implicit = client.try_acquire().unwrap();
        assert!(client.try_acquire().is_none());
        (&b).write_all(b"+").unwrap();
        let token = client.try_acquire().unwrap();
        drop(token);
        let mut back = [0u8];
        (&b).read_exact(&mut back).unwrap();
        assert_eq!(&back, b"+");
    }

    #[cfg(unix)]
    #[test]
    fn pipe_client() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let (r, mut w) = std::io::pipe().unwrap();
        w.write_all(b"xy").unwrap();
        let client = Client::new(&Mode::Pipe { read: r.as_raw_fd(), write: w.as_raw_fd() }).unwrap();
        let _implicit = client.try_acquire().unwrap();
        let x = client.try_acquire().unwrap();
        let y = client.try_acquire().unwrap();
        assert_eq!((x.token, y.token), (Some(b'x'), Some(b'y')));
        assert!(client.try_acquire().is_none());
        // The exact byte goes back.
        drop(y);
        assert_eq!(client.try_acquire().unwrap().token, Some(b'y'));
        // Not a pipe: refused.
        let f = std::fs::File::open("/dev/null").unwrap();
        assert!(Client::new(&Mode::Pipe { read: f.as_raw_fd(), write: f.as_raw_fd() }).is_err());
        assert!(Client::new(&Mode::Pipe { read: 1000, write: 1001 }).is_err());
    }

    /// The pool's descriptions stay as make has them, blocking: it shares
    /// them with its other clients, and make 3.81 fails reading a
    /// non-blocking pipe.
    #[cfg(unix)]
    #[test]
    fn pipe_pool_stays_blocking() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let (r, mut w) = std::io::pipe().unwrap();
        w.write_all(b"+").unwrap();
        let client = Client::new(&Mode::Pipe { read: r.as_raw_fd(), write: w.as_raw_fd() }).unwrap();
        let _implicit = client.try_acquire().unwrap();
        let token = client.try_acquire();
        assert!(token.is_some());
        assert!(client.try_acquire().is_none(), "waited for a token");
        for fd in [r.as_raw_fd(), w.as_raw_fd()] {
            // SAFETY: plain fcntl call on an open descriptor.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK, 0, "{fd}");
        }
    }
}
