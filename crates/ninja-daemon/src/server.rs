//! The daemon: one per user and ninja-rs binary (`Location`), serving every
//! build, one thread per connection, until it has been idle for a while.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use cpp_deps::driver::Environment;
use cpp_deps::{Cache, Driver};

use crate::files::{self, Files};
use crate::ipc::{self, Location};
use crate::proto::{self, Begin, PROTOCOL, Request, Response, Status};
use crate::watch::{self, RootState, Watcher};
use crate::{Locked, RwLocked};

/// Builds whose environments are kept (a scan names its build's): those
/// that used theirs last.
const KEPT_SESSIONS: usize = 64;

/// How long a build directory stays known after a build began in it
/// (CMake's `try_compile` makes hundreds a day): longer than a build. The
/// next build there names it again.
const BUILD_DIRS_KEPT: Duration = Duration::from_secs(24 * 3600);

/// The version the daemon reports: its program's (`set_version`), the
/// package's until then.
static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Sets the version the daemon reports, before it serves: ninja-rs gives
/// its own, with the commit it was built from, which this crate doesn't
/// know (it isn't built again for each commit).
pub fn set_version(version: &str) {
    let _ = VERSION.set(version.to_string());
}

pub fn version() -> &'static str {
    VERSION.get().map_or(env!("CARGO_PKG_VERSION"), String::as_str)
}

pub struct Daemon {
    preprocessor: Arc<Preprocessor>,
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

/// The daemon's preprocessor, which the watcher's events reach: another
/// once a request panicked (see `Daemon::answer`).
struct Preprocessor {
    driver: RwLock<Arc<Driver>>,
    case_insensitive: bool,
}

impl Preprocessor {
    /// A new one's driver.
    fn driver(watcher: &Arc<Watcher>, case_insensitive: bool) -> Arc<Driver> {
        let driver = Arc::new(Driver::new(Cache::new(vec![], case_insensitive)));
        driver.cache.set_watch(watcher.clone());
        driver
    }
}

impl watch::Listener for Preprocessor {
    fn changed(&self, path: &Path, change: watch::Change) {
        self.driver.reading().cache.changed(path, change);
    }
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
        let mut a = self.0.activity.locked();
        a.running -= 1;
        a.last = Instant::now();
    }
}

impl Daemon {
    pub fn new(log: Box<dyn Fn(&str) + Send + Sync>) -> Arc<Daemon> {
        let case_insensitive = cfg!(any(windows, target_os = "macos"));
        let watcher = Watcher::new(case_insensitive, log);
        let preprocessor = Arc::new(Preprocessor { driver: RwLock::new(Preprocessor::driver(&watcher, case_insensitive)), case_insensitive });
        watcher.listen(preprocessor.clone());
        let files = Arc::new(Files::new(watcher.clone(), case_insensitive));
        watcher.listen(files.clone());
        Arc::new(Daemon {
            preprocessor,
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
        let a = self.activity.locked();
        if a.running > 0 { Duration::ZERO } else { a.last.elapsed() }
    }

    /// Marks a request in progress, until the guard goes; None once
    /// exiting.
    fn busy(&self) -> Option<Busy<'_>> {
        let mut a = self.activity.locked();
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
        let mut a = self.activity.locked();
        if a.running == 0 && a.last.elapsed() >= idle {
            a.exiting = true;
        }
        a.exiting
    }

    /// The preprocessor.
    pub fn driver(&self) -> Arc<Driver> {
        self.preprocessor.driver.reading().clone()
    }

    /// Answers `req`; one that panicked (a bug), with an error. What it
    /// was doing may be half done (an event's entries dropped by some
    /// listeners, not others): nothing is vouched for until roots are in
    /// sync again. The locks it held are taken anyway (see `Locked`), but
    /// the preprocessor's (a compiler's queries are asked holding one): it
    /// starts again, as with a new daemon (every request after failed).
    pub fn answer(&self, req: Request) -> Response {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.handle(req))) {
            Ok(r) => r,
            Err(e) => {
                let why = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                *self.preprocessor.driver.writing() = Preprocessor::driver(&self.watcher, self.preprocessor.case_insensitive);
                self.watcher.distrust(&format!("a request panicked: {why}"));
                Response::Error(format!("the daemon failed: {why}"))
            }
        }
    }

    pub fn handle(&self, req: Request) -> Response {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let Some(_busy) = self.busy() else { return Response::Error("the daemon is exiting".into()) };
        match req {
            Request::Hello { .. } => Response::Hello { protocol: PROTOCOL, pid: std::process::id(), version: version().to_string() },
            Request::Begin(b) => self.begin(b),
            Request::Scan { session, argv, cwd, wrote } => {
                let env = self.sessions.locked().get_mut(&session).map(|(env, used)| {
                    *used = self.uses.fetch_add(1, Ordering::Relaxed);
                    env.clone()
                });
                match env {
                    Some(env) => {
                        self.watcher.fence(wrote);
                        Response::Scanned(self.driver().scan_in(&argv, &cwd, &env).map(Into::into))
                    }
                    None => Response::UnknownBuild,
                }
            }
            Request::Files { paths, wrote } => {
                self.watcher.fence(wrote);
                Response::Files(paths.iter().map(|p| self.files.get(p)).collect())
            }
            Request::Tree { dir, wrote } => {
                self.watcher.fence(wrote);
                Response::Tree(self.files.tree(&dir).map(|t| (*t).clone()))
            }
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
            self.seeded.locked().retain(|_, named| *named >= before);
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
        self.driver().cache.new_session();
        self.files.new_session();
        // Read once: every build saves it again, from what the daemon
        // knows (see `Files::seed`).
        if let Some(f) = &b.digests_file
            && self.seeded.locked().insert(f.clone(), Instant::now()).is_none()
        {
            self.files.seed(files::read_digests(f));
        }
        self.watcher.sync();
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let env = Environment { vars: Some(Arc::new(b.env)), profile_dir: b.profile_dir };
        let mut sessions = self.sessions.locked();
        sessions.insert(id, (Arc::new(env), self.uses.fetch_add(1, Ordering::Relaxed)));
        if sessions.len() > KEPT_SESSIONS
            && let Some(lru) = sessions.iter().min_by_key(|(_, (_, used))| *used).map(|(id, _)| *id)
        {
            sessions.remove(&lru);
        }
        Response::Begun { session: id }
    }

    pub fn status(&self) -> Status {
        let driver = self.driver();
        let (lexed, listings) = driver.cache.len();
        let (digests, trees) = self.files.len();
        let stats = driver.cache.stats();
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
/// (while the daemon runs too: it may run for days), what it held moved to
/// `old` (the one there before goes): what led to a problem stays.
fn logger(path: &Path, old: &Path, limit: u64) -> Box<dyn Fn(&str) + Send + Sync> {
    let (path, old) = (path.to_path_buf(), old.to_path_buf());
    let open = |p: &Path| std::fs::File::options().create(true).append(true).open(p);
    let file = open(&path).ok().map(|f| {
        let len = f.metadata().map_or(0, |m| m.len());
        Mutex::new((f, len))
    });
    Box::new(move |msg: &str| {
        let Some(f) = &file else { return };
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        let line = format!("{now} [{}] {msg}\n", std::process::id());
        let (f, len) = &mut *f.locked();
        if *len + line.len() as u64 > limit {
            // Moved aside (the handle follows it), then a new one; or, when
            // it can't be (open elsewhere on Windows), emptied through
            // another handle (one appending can't truncate there): appends
            // go to its new end.
            match std::fs::rename(&path, &old).and_then(|()| open(&path)) {
                Ok(new) => *f = new,
                Err(_) => drop(std::fs::File::create(&path)),
            }
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
/// than hours later. Returns at once when another one is running. Why it
/// couldn't start, and panics (a thread's, not a request's only), go to
/// its log: its standard streams go nowhere.
pub fn serve(loc: &Location, idle: Duration, exe: Option<PathBuf>) -> std::io::Result<()> {
    loc.create_dir()?;
    let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::from(logger(&loc.log_file(), &loc.old_log_file(), LOG_LIMIT));
    let failed = |what: &str, e: std::io::Error| {
        log(&format!("not started: {what}: {e}"));
        std::io::Error::new(e.kind(), format!("{what}: {e}"))
    };
    let lock = std::fs::File::options().create(true).truncate(false).write(true).open(loc.lock_file()).map_err(|e| failed("opening its lock", e))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => return Err(failed("locking", e)),
    }
    {
        let (log, before) = (log.clone(), std::panic::take_hook());
        std::panic::set_hook(Box::new(move |info| {
            log(&info.to_string().replace('\n', " "));
            before(info);
        }));
    }
    #[cfg(unix)]
    raise_file_limit();
    // Those of other binaries, rebuilt since (Windows: builds remove them
    // as they copy the binary for the daemon).
    #[cfg(unix)]
    if let Err(e) = loc.remove_others() {
        log(&format!("removing other daemons' files: {e}"));
    }
    let log2 = log.clone();
    let daemon = Daemon::new(Box::new(move |m| log2(m)));
    daemon.watcher.set_cookie_dir(&loc.cookie_dir());
    #[cfg(unix)]
    let at = loc.socket().display().to_string();
    #[cfg(windows)]
    let at = loc.pipe();
    let mut listener = ipc::Listener::bind(loc).map_err(|e| failed(&format!("listening at {at}"), e))?;
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
        let resp = daemon.answer(req);
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
    /// daemon runs (it may run for days), what it held kept beside it (it
    /// used to go, and with it what led to a problem).
    #[test]
    fn log_trimmed() {
        let dir = TestDir::new("ninja_daemon_log");
        let (path, old) = (dir.join("daemon.log"), dir.join("daemon.log.1"));
        let log = logger(&path, &old, 1000);
        for i in 0..200 {
            log(&format!("line {i}"));
            assert!(std::fs::metadata(&path).unwrap().len() <= 1000);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with(" line 199\n") && !text.contains(" line 0\n"), "{text}");
        let first: usize = text.lines().next().and_then(|l| l.rsplit(' ').next()).unwrap().parse().unwrap();
        let before = std::fs::read_to_string(&old).unwrap();
        assert!(before.ends_with(&format!(" line {}\n", first - 1)), "{before}");
    }

    /// A daemon that can't start says why in its log (its standard streams
    /// go nowhere), and so do panics outside requests.
    #[test]
    fn start_up_errors_logged() {
        let base = TestDir::new("ninja_daemon_cant_start");
        let loc = Location { dir: base.join("daemon"), id: "cant".into() };
        loc.create_dir().unwrap();
        // Something else where it would listen.
        #[cfg(unix)]
        std::fs::create_dir_all(loc.socket().join("x")).unwrap();
        #[cfg(windows)]
        let _taken = ipc::Listener::bind(&loc).unwrap();
        let e = serve(&loc, Duration::from_secs(3600), None).unwrap_err();
        let text = std::fs::read_to_string(loc.log_file()).unwrap();
        assert!(text.contains("not started: listening at") && text.contains(&e.to_string()), "{e}: {text}");
        std::thread::spawn(|| panic!("a reader's bug")).join().unwrap_err();
        let text = std::fs::read_to_string(loc.log_file()).unwrap();
        assert!(text.contains("a reader's bug"), "{text}");
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
        assert_eq!(daemon.seeded.locked().len(), 1);
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
                wrote: false,
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

    /// Under a directory builds write to whose own files only are theirs
    /// (one outside repositories, a repository's root), files are checked
    /// every time unless a root in sync watches them: an output the build
    /// doesn't name there (a generator's tree, a file copied after a link)
    /// used to keep the digest of its first look for the build, as builds
    /// without the daemon don't.
    #[test]
    fn unnamed_outputs() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        use cpp_deps::scan::Watch;
        let base = TestDir::new("ninja_daemon_unnamed");
        for d in ["build", "out/sub", "repo/.git", "repo/sub"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = watch::strip_verbatim(std::fs::canonicalize(&*base).unwrap());
        let (out, repo) = (dir.join("out"), dir.join("repo"));
        let files = [out.join("x.h"), out.join("sub/y.h"), repo.join("z.h"), repo.join("sub/w.h")];
        for f in &files {
            std::fs::write(f, "v1").unwrap();
        }
        let daemon = Daemon::new(Box::new(|_| {}));
        daemon.watcher.set_cookie_dir(&dir.join("cookies"));
        let begin = || {
            let b = Begin { build_dir: dir.join("build"), volatile: vec![out.clone(), repo.clone()], ..Default::default() };
            assert!(matches!(daemon.handle(Request::Begin(b)), Response::Begun { .. }));
        };
        let size = |p: &Path| match daemon.handle(Request::Files { paths: vec![p.to_path_buf()], wrote: false }) {
            Response::Files(v) => v[0].as_ref().unwrap().size,
            r => panic!("{r:?}"),
        };
        begin();
        assert!(files.iter().all(|f| size(f) == 2));
        // A generator rewrites them, during the build.
        for f in &files {
            std::fs::write(f, "version 2").unwrap();
        }
        for f in &files {
            assert_eq!(size(f), 9, "{}", f.display());
        }
        // Once the repository's root is in sync, its files under the
        // directory are vouched for, and their changes seen.
        let w = repo.join("sub/w.h");
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.watcher.token(&w).is_none() {
            assert!(Instant::now() < deadline, "{:?}", daemon.watcher.roots());
            size(&w);
            begin();
        }
        std::fs::write(&w, "version three").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while size(&w) != 13 {
            assert!(Instant::now() < deadline, "the change wasn't seen");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(daemon.watcher.token(&repo.join("z.h")), None);
    }

    /// The daemon reports the version its program gives it (ninja-rs's,
    /// with the commit it was built from), not its package's: unstamped,
    /// both are the same, and a check comparing them passed without it.
    #[test]
    fn versions_set() {
        set_version("1.2.3+given");
        let daemon = Daemon::new(Box::new(|_| {}));
        assert_eq!(daemon.status().version, "1.2.3+given");
        assert!(matches!(daemon.handle(Request::Hello { protocol: PROTOCOL }), Response::Hello { version, .. } if version == "1.2.3+given"));
    }

    /// A file vouched for, rewritten (an output a step doesn't name), then
    /// asked for at once: the answer is of the new version, though the
    /// thread reading events hasn't handled its event yet (it's busy). With
    /// inotify, the queue is read first; on Windows, when the build says a
    /// command ran since (a cookie comes back first). It used to be of the
    /// old one.
    #[test]
    fn answers_after_changes() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        use cpp_deps::scan::Watch;
        use std::sync::atomic::AtomicBool;
        /// Blocks the reader on `trigger`'s event while `stall` is set.
        struct Staller {
            trigger: PathBuf,
            stall: AtomicBool,
            blocked: AtomicBool,
        }
        impl watch::Listener for Staller {
            fn changed(&self, path: &Path, _: watch::Change) {
                while path == self.trigger && self.stall.load(Ordering::SeqCst) {
                    self.blocked.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        let base = TestDir::new("ninja_daemon_answers");
        for d in ["repo/.git", "repo/sub", "repo/build"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = watch::strip_verbatim(std::fs::canonicalize(&*base).unwrap());
        let (f, trigger) = (dir.join("repo/sub/gen.h"), dir.join("repo/sub/trigger"));
        std::fs::write(&f, "v1").unwrap();
        let daemon = Daemon::new(Box::new(|_| {}));
        daemon.watcher.set_cookie_dir(&dir.join("cookies"));
        let staller = Arc::new(Staller { trigger: trigger.clone(), stall: AtomicBool::new(false), blocked: AtomicBool::new(false) });
        daemon.watcher.listen(staller.clone());
        let begin = || {
            let b = Begin { build_dir: dir.join("repo/build"), ..Default::default() };
            assert!(matches!(daemon.handle(Request::Begin(b)), Response::Begun { .. }));
        };
        let size = {
            let f = f.clone();
            move |d: &Daemon, wrote: bool| match d.handle(Request::Files { paths: vec![f.clone()], wrote }) {
                Response::Files(v) => v[0].as_ref().unwrap().size,
                r => panic!("{r:?}"),
            }
        };
        begin();
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.watcher.token(&f).is_none() {
            assert!(Instant::now() < deadline, "{:?}", daemon.watcher.roots());
            size(&daemon, false);
            begin();
        }
        assert_eq!(size(&daemon, false), 2);
        // The reader is busy (with another event), the file rewritten.
        staller.stall.store(true, Ordering::SeqCst);
        std::fs::write(&trigger, "").unwrap();
        while !staller.blocked.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::fs::write(&f, "version 2").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (asking, ask) = (daemon.clone(), size.clone());
        std::thread::spawn(move || tx.send(ask(&asking, cfg!(windows))));
        // Answered at once, without the reader, or once it's done.
        let early = rx.recv_timeout(Duration::from_millis(500)).ok();
        staller.stall.store(false, Ordering::SeqCst);
        assert_eq!(early.unwrap_or_else(|| rx.recv().unwrap()), 9);
    }

    /// A request that panics while it holds a lock (a bug) is answered with
    /// an error, and those after it aren't: they failed on the lock it left
    /// poisoned, for the daemon's life. What was vouched for isn't any more
    /// (some listeners may have dropped an entry, others not), until roots
    /// are in sync again.
    #[test]
    fn panics_answered() {
        if cfg!(not(any(target_os = "linux", windows))) {
            return;
        }
        /// Panics once, dropping what's under `at`.
        struct Buggy(PathBuf, std::sync::atomic::AtomicBool);
        impl watch::Listener for Buggy {
            fn changed(&self, path: &Path, change: watch::Change) {
                if path == self.0 && change == watch::Change::Tree && !self.1.swap(true, Ordering::SeqCst) {
                    panic!("a listener's bug");
                }
            }
        }
        use cpp_deps::scan::Watch;
        let base = TestDir::new("ninja_daemon_panics");
        for d in ["repo/.git", "repo/src", "repo/build", "other"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        let dir = watch::strip_verbatim(std::fs::canonicalize(&*base).unwrap());
        let a = dir.join("repo/src/a.h");
        std::fs::write(&a, "a").unwrap();
        let (lines, log) = {
            let lines = Arc::new(Mutex::new(Vec::<String>::new()));
            let l2 = lines.clone();
            (lines, Box::new(move |m: &str| l2.locked().push(m.to_string())))
        };
        let daemon = Daemon::new(log);
        daemon.watcher.set_cookie_dir(&dir.join("cookies"));
        daemon.watcher.listen(Arc::new(Buggy(dir.join("other"), Default::default())));
        let preprocessor = daemon.driver();
        let begin = |build_dir: &Path| daemon.answer(Request::Begin(Begin { build_dir: build_dir.to_path_buf(), ..Default::default() }));
        let size = || match daemon.answer(Request::Files { paths: vec![a.clone()], wrote: false }) {
            Response::Files(v) => v[0].as_ref().unwrap().size,
            r => panic!("{r:?}"),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.watcher.token(&a).is_none() {
            assert!(Instant::now() < deadline, "{:?}", daemon.watcher.roots());
            size();
            begin(&dir.join("repo/build"));
        }
        // Its roots' lock held, as what's under a new build directory goes.
        let r = begin(&dir.join("other"));
        assert!(matches!(&r, Response::Error(e) if e.contains("a listener's bug")), "{r:?}");
        assert!(matches!(daemon.answer(Request::Status), Response::Status(_)));
        assert_eq!(size(), 1);
        // Its locks may be poisoned: another.
        assert!(!Arc::ptr_eq(&preprocessor, &daemon.driver()));
        assert_eq!(daemon.watcher.token(&a), None);
        assert!(lines.locked().iter().any(|l| l.contains("a request panicked: a listener's bug")), "{:?}", lines.locked());
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon.watcher.token(&a).is_none() {
            assert!(Instant::now() < deadline, "{:?}", daemon.watcher.roots());
            size();
            assert!(matches!(begin(&dir.join("repo/build")), Response::Begun { .. }));
        }
    }

    /// Unix: a daemon starting removes the files other daemons left (other
    /// binaries', rebuilt since: logs of 2 MiB each), unless they run. They
    /// stayed in the runtime directory (a tmpfs) until the next boot.
    #[cfg(unix)]
    #[test]
    fn others_removed_at_start() {
        let base = TestDir::new("ninja_daemon_others_at_start");
        let loc = Location { dir: base.join("daemon"), id: "now".into() };
        loc.create_dir().unwrap();
        let old = Location { dir: loc.dir.clone(), id: "before".into() };
        for f in [old.lock_file(), old.log_file(), old.old_log_file(), old.dir.join("before.sock")] {
            std::fs::write(f, "").unwrap();
        }
        let served = loc.clone();
        std::thread::spawn(move || serve(&served, Duration::from_secs(3600), None));
        crate::Client::connect_or_start(&loc, || Ok(None)).unwrap();
        let left: Vec<String> = std::fs::read_dir(&loc.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(!left.iter().any(|f| f.starts_with("before.")), "{left:?}");
    }
}
