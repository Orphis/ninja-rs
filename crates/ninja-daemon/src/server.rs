//! The daemon: one per user and ninja-rs binary (`Location`), serving every
//! build, one thread per connection, until it has been idle for a while.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cpp_deps::driver::Environment;
use cpp_deps::{Cache, Driver};

use crate::files::{self, Files};
use crate::ipc::{self, Location};
use crate::proto::{self, Begin, PROTOCOL, Request, Response, Status};
use crate::watch::{self, RootState, Watcher};

/// Builds whose environments are kept (a scan names its build's): those
/// that used theirs last.
const KEPT_SESSIONS: usize = 64;

/// How long a build directory stays known after a build began in it
/// (CMake's `try_compile` makes hundreds a day): longer than a build. The
/// next build there names it again.
const BUILD_DIRS_KEPT: Duration = Duration::from_secs(24 * 3600);

/// The ninja-rs version, which the daemon reports and ninja-rs too (this
/// is the lowest crate reporting it): the package's, with the commit it
/// was built from when the build knew it (`build.rs` asks git; Bazel
/// stamps it with `--stamp`), as `0.1.0+1a2b3c4d5e6f`.
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        // An unstamped Bazel build leaves the placeholder.
        match option_env!("NINJA_RS_COMMIT").filter(|c| !c.is_empty() && !c.starts_with('{')) {
            Some(commit) => format!("{}+{commit}", env!("CARGO_PKG_VERSION")),
            None => env!("CARGO_PKG_VERSION").to_string(),
        }
    })
}

pub struct Daemon {
    pub driver: Arc<Driver>,
    pub files: Arc<Files>,
    pub watcher: Arc<Watcher>,
    /// Builds' environments, and when each was last used (`uses`).
    sessions: Mutex<HashMap<u64, (Arc<Environment>, u64)>>,
    uses: AtomicU64,
    next_session: AtomicU64,
    /// Digest files read, and when a build last named each.
    seeded: Mutex<HashMap<PathBuf, Instant>>,
    started: Instant,
    requests: AtomicU64,
    activity: Mutex<Activity>,
}

/// Whether the daemon is busy: it exits when idle only between requests.
struct Activity {
    /// Requests in progress.
    running: usize,
    /// When the last one ended.
    last: Instant,
    /// Exiting: no request starts any more.
    exiting: bool,
}

/// A request in progress (see `Daemon::busy`).
struct Busy<'a>(&'a Daemon);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut a = self.0.activity.lock().unwrap();
        a.running -= 1;
        a.last = Instant::now();
    }
}

impl Daemon {
    pub fn new(log: Box<dyn Fn(&str) + Send + Sync>) -> Arc<Daemon> {
        let case_insensitive = cfg!(any(windows, target_os = "macos"));
        let watcher = Watcher::new(case_insensitive, log);
        let driver = Arc::new(Driver::new(Cache::new(vec![], case_insensitive)));
        driver.cache.set_watch(watcher.clone());
        watcher.listen(driver.clone());
        let files = Arc::new(Files::new(watcher.clone(), case_insensitive));
        watcher.listen(files.clone());
        Arc::new(Daemon {
            driver,
            files,
            watcher,
            sessions: Mutex::default(),
            uses: AtomicU64::new(0),
            next_session: AtomicU64::new(1),
            seeded: Mutex::default(),
            started: Instant::now(),
            requests: AtomicU64::new(0),
            activity: Mutex::new(Activity { running: 0, last: Instant::now(), exiting: false }),
        })
    }

    /// How long since the last request ended (none while one runs).
    pub fn idle(&self) -> Duration {
        let a = self.activity.lock().unwrap();
        if a.running > 0 { Duration::ZERO } else { a.last.elapsed() }
    }

    /// Marks a request in progress, until the guard goes; None once
    /// exiting.
    fn busy(&self) -> Option<Busy<'_>> {
        let mut a = self.activity.lock().unwrap();
        if a.exiting {
            return None;
        }
        a.running += 1;
        Some(Busy(self))
    }

    /// Whether it has been idle for `idle`, between requests: it's exiting
    /// then, and none starts any more (one in progress, a long toolchain
    /// walk, used to be cut).
    pub fn exit_if_idle(&self, idle: Duration) -> bool {
        let mut a = self.activity.lock().unwrap();
        if a.running == 0 && a.last.elapsed() >= idle {
            a.exiting = true;
        }
        a.exiting
    }

    pub fn handle(&self, req: Request) -> Response {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let Some(_busy) = self.busy() else { return Response::Error("the daemon is exiting".into()) };
        match req {
            Request::Hello { .. } => Response::Hello { protocol: PROTOCOL, pid: std::process::id(), version: version().to_string() },
            Request::Begin(b) => self.begin(b),
            Request::Scan { session, argv, cwd } => {
                let env = self.sessions.lock().unwrap().get_mut(&session).map(|(env, used)| {
                    *used = self.uses.fetch_add(1, Ordering::Relaxed);
                    env.clone()
                });
                match env {
                    Some(env) => Response::Scanned(self.driver.scan_in(&argv, &cwd, &env).map(Into::into)),
                    None => Response::UnknownBuild,
                }
            }
            Request::Files { paths } => Response::Files(paths.iter().map(|p| self.files.get(p)).collect()),
            Request::Tree { dir } => Response::Tree(self.files.tree(&dir).map(|t| (*t).clone())),
            Request::Status => Response::Status(self.status()),
            Request::Stop => Response::Stopping,
        }
    }

    /// A build starts: its directory, and the others it writes to, are
    /// never trusted, entries the watcher doesn't vouch for are checked
    /// again, and roots are brought in sync.
    fn begin(&self, b: Begin) -> Response {
        if let Some(before) = Instant::now().checked_sub(BUILD_DIRS_KEPT) {
            self.watcher.expire(before);
            self.seeded.lock().unwrap().retain(|_, named| *named >= before);
        }
        // The one set the digests and the preprocessor's cache ask: the
        // build directory, under any of its spellings, whole; the others
        // (outputs outside it) whole unless they hold roots.
        let key = |p: &Path| watch::key(p, self.watcher.case_insensitive());
        let real = std::fs::canonicalize(&b.build_dir).ok().map(|r| key(&watch::strip_verbatim(r)));
        self.watcher.ignore(&b.build_dir, true);
        for d in &b.volatile {
            if d.starts_with(&b.build_dir) || real.as_ref().is_some_and(|r| key(d).starts_with(r)) {
                self.watcher.ignore(d, true);
            } else {
                self.watcher.ignore_outputs(d);
            }
        }
        self.driver.cache.new_session();
        self.files.new_session();
        // Read once: every build saves it again, from what the daemon
        // knows (see `Files::seed`).
        if let Some(f) = &b.digests_file
            && self.seeded.lock().unwrap().insert(f.clone(), Instant::now()).is_none()
        {
            self.files.seed(files::read_digests(f));
        }
        self.watcher.sync();
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let env = Environment { vars: Some(Arc::new(b.env)), profile_dir: b.profile_dir };
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(id, (Arc::new(env), self.uses.fetch_add(1, Ordering::Relaxed)));
        if sessions.len() > KEPT_SESSIONS
            && let Some(lru) = sessions.iter().min_by_key(|(_, (_, used))| *used).map(|(id, _)| *id)
        {
            sessions.remove(&lru);
        }
        Response::Begun { session: id }
    }

    pub fn status(&self) -> Status {
        let (lexed, listings) = self.driver.cache.len();
        let (digests, trees) = self.files.len();
        let stats = self.driver.cache.stats();
        Status {
            pid: std::process::id(),
            version: version().to_string(),
            uptime_secs: self.started.elapsed().as_secs(),
            builds: self.next_session.load(Ordering::Relaxed) - 1,
            requests: self.requests.load(Ordering::Relaxed),
            events: self.watcher.events(),
            roots: self
                .watcher
                .roots()
                .into_iter()
                .map(|(p, s)| {
                    let s = match s {
                        RootState::Starting => "starting".to_string(),
                        RootState::Unsynced => "not in sync yet".to_string(),
                        RootState::Trusted => "watched".to_string(),
                        RootState::Failed(e) => format!("not watched: {e}"),
                    };
                    (p, s)
                })
                .collect(),
            lexed,
            listings,
            watched_dirs: self.watcher.watched_dirs(),
            ignored: self.watcher.ignored_dirs(),
            digests,
            trees,
            files_lexed: stats.files_lexed,
            files_reused: stats.files_reused,
        }
    }
}

/// How large the log grows before it starts again.
const LOG_LIMIT: u64 = 1 << 20;

/// The log file `path`, started again when it would grow past `limit`
/// (while the daemon runs too: it may run for days).
fn logger(path: &Path, limit: u64) -> Box<dyn Fn(&str) + Send + Sync> {
    let path = path.to_path_buf();
    let file = std::fs::File::options().create(true).append(true).open(&path).ok().map(|f| {
        let len = f.metadata().map_or(0, |m| m.len());
        Mutex::new((f, len))
    });
    Box::new(move |msg: &str| {
        let Some(f) = &file else { return };
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        let line = format!("{now} [{}] {msg}\n", std::process::id());
        let (f, len) = &mut *f.lock().unwrap();
        // Emptied through another handle (one appending can't truncate on
        // Windows): appends go to its new end.
        if *len + line.len() as u64 > limit && std::fs::File::create(&path).is_ok() {
            *len = 0;
        }
        if f.write_all(line.as_bytes()).is_ok() {
            *len += line.len() as u64;
        }
    })
}

/// Unix: lets the daemon have as many files open as it may, up to a sane
/// bound: out of descriptors, watches and tree walks fail, and a shell's
/// default (1024) is soon reached by a daemon serving many builds. (The
/// compilers it probes inherit it.)
#[cfg(unix)]
fn raise_file_limit() {
    let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: a valid out-parameter, then plain calls.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) == 0 {
            let wanted = l.rlim_max.min(1 << 16);
            if l.rlim_cur < wanted {
                l.rlim_cur = wanted;
                libc::setrlimit(libc::RLIMIT_NOFILE, &l);
            }
        }
    }
}

/// Whether the daemon of `exe` is no longer the one at `loc`: the binary
/// was rebuilt (or removed), and builds start another daemon.
pub fn superseded(exe: &Path, loc: &Location) -> bool {
    Location::for_exe(exe).map_or(true, |l| l.id != loc.id)
}

/// How long a superseded daemon waits for builds still using it.
const SUPERSEDED_GRACE: Duration = Duration::from_secs(10);

/// How long the daemon waits after a connection failed, at first and at
/// most.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
const ACCEPT_RETRY_MAX: Duration = Duration::from_secs(5);

/// Runs the daemon until it has been idle for `idle`, is asked to stop, or
/// the binary it comes from (`exe`) is rebuilt: its watches go then rather
/// than hours later. Returns at once when another one is running.
pub fn serve(loc: &Location, idle: Duration, exe: Option<PathBuf>) -> std::io::Result<()> {
    loc.create_dir()?;
    let lock = std::fs::File::options().create(true).truncate(false).write(true).open(loc.lock_file())?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    #[cfg(unix)]
    raise_file_limit();
    let log = Arc::new(logger(&loc.log_file(), LOG_LIMIT));
    let log2 = log.clone();
    let daemon = Daemon::new(Box::new(move |m| log2(m)));
    daemon.watcher.set_cookie_dir(&loc.cookie_dir());
    let mut listener = ipc::Listener::bind(loc)?;
    log(&format!("started (ninja-rs {}), exiting after {idle:?} idle", version()));
    {
        let (daemon, log, loc) = (daemon.clone(), log.clone(), loc.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5).min(idle));
                if daemon.exit_if_idle(idle) {
                    log("idle: exiting");
                    std::process::exit(0);
                }
                if let Some(exe) = &exe
                    && daemon.idle() >= SUPERSEDED_GRACE
                    && superseded(exe, &loc)
                    && daemon.exit_if_idle(SUPERSEDED_GRACE)
                {
                    log(&format!("{} was rebuilt or removed: exiting", exe.display()));
                    std::process::exit(0);
                }
            }
        });
    }
    // A connection that failed (a client gone, out of file descriptors)
    // isn't the daemon's end: said once while it lasts, and tried again
    // less and less often.
    let (mut delay, mut failing) = (ACCEPT_RETRY, None);
    loop {
        let stream = match listener.accept() {
            Ok(s) => {
                (delay, failing) = (ACCEPT_RETRY, None);
                s
            }
            Err(e) => {
                let e = e.to_string();
                if failing.as_ref() != Some(&e) {
                    log(&format!("accepting a connection: {e}"));
                    failing = Some(e);
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(ACCEPT_RETRY_MAX);
                continue;
            }
        };
        let (daemon, log) = (daemon.clone(), log.clone());
        std::thread::spawn(move || connection(&daemon, stream, &*log));
    }
    // The lock is held until the process exits.
}

fn connection(daemon: &Daemon, mut stream: ipc::Stream, log: &(dyn Fn(&str) + Send + Sync)) {
    while let Ok(Some(req)) = proto::read::<Request>(&mut stream) {
        let stop = matches!(req, Request::Stop);
        let resp = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| daemon.handle(req))) {
            Ok(r) => r,
            Err(e) => {
                let why = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                log(&format!("a request panicked: {why}"));
                Response::Error(format!("the daemon failed: {why}"))
            }
        };
        if proto::write(&mut stream, &resp).is_err() {
            return;
        }
        if stop {
            log("asked to stop: exiting");
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_dirs::TestDir;

    /// A rebuilt (or removed) binary supersedes its daemon.
    #[test]
    fn superseded_by_a_rebuilt_binary() {
        let dir = TestDir::new("ninja_daemon_exe");
        let exe = dir.join("ninja-rs");
        std::fs::write(&exe, "version 1").unwrap();
        let loc = Location::for_exe(&exe).unwrap();
        assert!(!superseded(&exe, &loc));
        std::fs::write(&exe, "version 2, longer").unwrap();
        assert!(superseded(&exe, &loc));
        std::fs::remove_file(&exe).unwrap();
        assert!(superseded(&exe, &loc));
    }

    /// The daemon exits when idle only between requests: one in progress
    /// (a long toolchain walk) isn't cut, and none starts once it's
    /// exiting.
    #[test]
    fn idle_between_requests() {
        let daemon = Daemon::new(Box::new(|_| {}));
        let busy = daemon.busy().unwrap();
        assert_eq!(daemon.idle(), Duration::ZERO);
        assert!(!daemon.exit_if_idle(Duration::ZERO));
        drop(busy);
        assert!(daemon.exit_if_idle(Duration::ZERO));
        assert!(matches!(daemon.handle(Request::Status), Response::Error(_)));
    }

    /// The log starts again when it would grow past its limit, while the
    /// daemon runs (it may run for days).
    #[test]
    fn log_trimmed() {
        let dir = TestDir::new("ninja_daemon_log");
        let path = dir.join("daemon.log");
        let log = logger(&path, 1000);
        for i in 0..200 {
            log(&format!("line {i}"));
            assert!(std::fs::metadata(&path).unwrap().len() <= 1000);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with(" line 199\n") && !text.contains(" line 0\n"), "{text}");
    }

    /// A build's digests file, saved again by every build, is read once:
    /// what the daemon keeps per build directory doesn't grow per build.
    #[test]
    fn digests_read_once() {
        let dir = TestDir::new("ninja_daemon_begin");
        let digests = dir.join(".ninja_rs_digests");
        let daemon = Daemon::new(Box::new(|_| {}));
        for i in 0..3 {
            std::fs::write(&digests, format!("{}\n", files::DIGESTS_SIGNATURE)).unwrap();
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + i);
            std::fs::File::options().write(true).open(&digests).unwrap().set_modified(t).unwrap();
            let b = Begin { build_dir: dir.to_path_buf(), digests_file: Some(digests.clone()), ..Default::default() };
            assert!(matches!(daemon.handle(Request::Begin(b)), Response::Begun { .. }));
        }
        assert_eq!(daemon.seeded.lock().unwrap().len(), 1);
    }

    /// A build's environment is kept while it scans, however many builds
    /// begin meanwhile (a configure's `try_compile` builds): those used
    /// least recently go.
    #[test]
    fn sessions_by_last_use() {
        let dir = TestDir::new("ninja_daemon_sessions");
        let daemon = Daemon::new(Box::new(|_| {}));
        let begin = |name: &str| match daemon.handle(Request::Begin(Begin { build_dir: dir.join(name), ..Default::default() })) {
            Response::Begun { session } => session,
            r => panic!("{r:?}"),
        };
        let scan = |session| {
            daemon.handle(Request::Scan {
                session,
                argv: vec![dir.join("no-such-cc").to_string_lossy().into_owned(), "-c".into(), "x.c".into()],
                cwd: dir.to_path_buf(),
            })
        };
        let long = begin("long");
        let first = begin("TryCompile-first");
        for i in 0..2 * KEPT_SESSIONS {
            begin(&format!("TryCompile-{i}"));
            if i % 8 == 0 {
                assert!(matches!(scan(long), Response::Scanned(_)), "{i}");
            }
        }
        assert!(matches!(scan(long), Response::Scanned(_)));
        assert!(!matches!(scan(first), Response::Scanned(_)));
    }

    /// Directories a build writes to besides its build directory aren't
    /// watched: their files are never vouched for, and their changes don't
    /// count against others'. One that holds the repository (an output in
    /// its root) only for its own files: the rest stays watched.
    #[test]
    fn volatile_dirs_unwatched() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        use cpp_deps::scan::Watch;
        let base = TestDir::new("ninja_daemon_volatile");
        for d in [".git", "src", "gen", "build"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = watch::strip_verbatim(std::fs::canonicalize(&*base).unwrap());
        let a = dir.join("src/a.h");
        std::fs::write(&a, "a").unwrap();
        let daemon = Daemon::new(Box::new(|_| {}));
        let begin =
            |volatile: &[PathBuf]| daemon.handle(Request::Begin(Begin { build_dir: dir.join("build"), volatile: volatile.to_vec(), ..Default::default() }));
        let gen_and_root = [dir.join("gen"), dir.clone()];
        begin(&gen_and_root);
        assert_eq!(daemon.watcher.token(&a), None);
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.watcher.roots().iter().all(|(_, s)| *s != RootState::Unsynced) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        daemon.watcher.token(&a);
        begin(&gen_and_root);
        let t = daemon.watcher.token(&a).expect("vouched for");
        assert_eq!(daemon.watcher.token(&dir.join("gen/out.h")), None);
        assert_eq!(daemon.watcher.token(&dir.join("compile_commands.json")), None);
        std::fs::write(dir.join("gen/out.h"), "generated").unwrap();
        // Its event, if any, came before the sync's.
        begin(&gen_and_root);
        assert!(daemon.watcher.unchanged_since(t));
        let status = daemon.status();
        assert_eq!(status.ignored.len(), 3, "{:?}", status.ignored);
        assert!(status.ignored.contains(&(dir.clone(), false)) && status.ignored.contains(&(dir.join("gen"), true)), "{:?}", status.ignored);
    }
}
