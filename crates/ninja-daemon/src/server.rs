//! The daemon: one per user and ninja-rs binary (`Location`), serving every
//! build, one thread per connection, until it has been idle for a while.

use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use cpp_deps::driver::Environment;
use cpp_deps::{Cache, Driver};

use crate::files::{self, Files};
use crate::ipc::{self, Location};
use crate::proto::{self, Begin, PROTOCOL, Request, Response, Status};
use crate::watch::{RootState, Watcher};

/// Builds whose environments are kept (a scan names its build's).
const KEPT_SESSIONS: usize = 64;

pub struct Daemon {
    pub driver: Arc<Driver>,
    pub files: Arc<Files>,
    pub watcher: Arc<Watcher>,
    sessions: Mutex<VecDeque<(u64, Arc<Environment>)>>,
    next_session: AtomicU64,
    /// Digest files read: (file, modification time).
    seeded: Mutex<HashSet<(PathBuf, Option<SystemTime>)>>,
    started: Instant,
    requests: AtomicU64,
    last_active: Mutex<Instant>,
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
            next_session: AtomicU64::new(1),
            seeded: Mutex::default(),
            started: Instant::now(),
            requests: AtomicU64::new(0),
            last_active: Mutex::new(Instant::now()),
        })
    }

    pub fn idle(&self) -> Duration {
        self.last_active.lock().unwrap().elapsed()
    }

    pub fn handle(&self, req: Request) -> Response {
        self.requests.fetch_add(1, Ordering::Relaxed);
        *self.last_active.lock().unwrap() = Instant::now();
        match req {
            Request::Hello { .. } => Response::Hello { protocol: PROTOCOL, pid: std::process::id(), version: env!("CARGO_PKG_VERSION").to_string() },
            Request::Begin(b) => self.begin(b),
            Request::Scan { session, argv, cwd } => {
                let env = self.sessions.lock().unwrap().iter().find(|(id, _)| *id == session).map(|(_, e)| e.clone());
                match env {
                    Some(env) => Response::Scanned(self.driver.scan_in(&argv, &cwd, &env).map(Into::into)),
                    None => Response::Error(format!("unknown build {session}")),
                }
            }
            Request::Files { paths } => Response::Files(paths.iter().map(|p| self.files.get(p)).collect()),
            Request::Tree { dir } => Response::Tree(self.files.tree(&dir).map(|t| (*t).clone())),
            Request::Status => Response::Status(self.status()),
            Request::Stop => Response::Stopping,
        }
    }

    /// A build starts: its directory is never trusted, entries the watcher
    /// doesn't vouch for are checked again, and roots are brought in sync.
    fn begin(&self, b: Begin) -> Response {
        self.driver.cache.add_volatile(&b.build_dir);
        self.files.add_volatile(&b.build_dir);
        self.watcher.ignore(&b.build_dir);
        self.driver.cache.new_session();
        self.files.new_session();
        if let Some(f) = &b.digests_file {
            let version = (f.clone(), std::fs::metadata(f).and_then(|m| m.modified()).ok());
            if self.seeded.lock().unwrap().insert(version) {
                self.files.seed(files::read_digests(f));
            }
        }
        self.watcher.sync();
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let env = Environment { vars: Some(Arc::new(b.env)), profile_dir: b.profile_dir };
        let mut sessions = self.sessions.lock().unwrap();
        sessions.push_back((id, Arc::new(env)));
        while sessions.len() > KEPT_SESSIONS {
            sessions.pop_front();
        }
        Response::Begun { session: id }
    }

    pub fn status(&self) -> Status {
        let (lexed, listings) = self.driver.cache.len();
        let (digests, trees) = self.files.len();
        let stats = self.driver.cache.stats();
        Status {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
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
            digests,
            trees,
            files_lexed: stats.files_lexed,
            files_reused: stats.files_reused,
        }
    }
}

/// A log file, restarted when it grows past 1 MiB.
fn logger(loc: &Location) -> Box<dyn Fn(&str) + Send + Sync> {
    let path = loc.log_file();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::remove_file(&path);
    }
    let file = std::fs::File::options().create(true).append(true).open(&path).ok().map(Mutex::new);
    Box::new(move |msg: &str| {
        if let Some(f) = &file {
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
            let _ = writeln!(f.lock().unwrap(), "{now} [{}] {msg}", std::process::id());
        }
    })
}

/// Runs the daemon until it has been idle for `idle` or is asked to stop.
/// Returns at once when another one is running.
pub fn serve(loc: &Location, idle: Duration) -> std::io::Result<()> {
    loc.create_dir()?;
    let lock = std::fs::File::options().create(true).truncate(false).write(true).open(loc.lock_file())?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    let log = Arc::new(logger(loc));
    let log2 = log.clone();
    let daemon = Daemon::new(Box::new(move |m| log2(m)));
    let mut listener = ipc::Listener::bind(loc)?;
    log(&format!("started (ninja-rs {}), exiting after {idle:?} idle", env!("CARGO_PKG_VERSION")));
    {
        let (daemon, log) = (daemon.clone(), log.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5).min(idle));
                if daemon.idle() >= idle {
                    log("idle: exiting");
                    std::process::exit(0);
                }
            }
        });
    }
    loop {
        let stream = listener.accept()?;
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
