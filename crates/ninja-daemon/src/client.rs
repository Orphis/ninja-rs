//! What builds use to talk to the daemon: connections kept in a pool, one
//! request at a time on each, so that concurrent actions ask in parallel.

use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::ipc::{self, Child, Location, START_FAILED_FOR, TimedStream};
use crate::proto::{self, Begin, FileInfo, PROTOCOL, Request, Response, ScanOutput, Status, TreeInfo};

/// How long the daemon may take to answer: listing a large toolchain the
/// first time, or a scan probing a compiler, takes seconds. A daemon stuck
/// longer is given up on (the build does the work itself).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a daemon just started may take to answer.
const START_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    location: Location,
    pool: Mutex<Vec<TimedStream>>,
    /// The build begun (0: none).
    session: AtomicU64,
    /// What began it: begun again when the daemon forgot it.
    begun: Mutex<Option<Begin>>,
    timeout: Duration,
}

fn unexpected(r: Response) -> io::Error {
    match r {
        Response::Error(e) => io::Error::other(e),
        other => io::Error::new(io::ErrorKind::InvalidData, format!("unexpected answer {other:?}")),
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "daemon {}", self.location.id)
    }
}

impl Client {
    /// Connects to the daemon at `location`, checking it speaks this
    /// protocol.
    pub fn connect(location: &Location) -> io::Result<Client> {
        Client::connect_timeout(location, ANSWER_TIMEOUT)
    }

    /// Connects, giving up on an answer after `timeout`.
    pub(crate) fn connect_timeout(location: &Location, timeout: Duration) -> io::Result<Client> {
        let client = Client { location: location.clone(), pool: Mutex::default(), session: AtomicU64::new(0), begun: Mutex::default(), timeout };
        match client.call(&Request::Hello { protocol: PROTOCOL })? {
            Response::Hello { protocol, .. } if protocol == PROTOCOL => Ok(client),
            Response::Hello { protocol, pid, .. } => {
                Err(io::Error::new(io::ErrorKind::Unsupported, format!("the daemon (pid {pid}) speaks protocol {protocol}, not {PROTOCOL}")))
            }
            r => Err(unexpected(r)),
        }
    }

    /// Connects, starting the daemon with `start` (which gives the process
    /// it started, if any) when none answers. One that answers in another
    /// protocol is given up on at once: another daemon can't start there
    /// while it runs. One that exits as it starts (not finding another
    /// running) is given up on at once, and isn't started again for a
    /// while (`START_FAILED_FOR`): builds don't each wait for it.
    pub fn connect_or_start(location: &Location, start: impl FnOnce() -> io::Result<Option<Child>>) -> io::Result<Client> {
        let mismatch = |e: &io::Error| e.kind() == io::ErrorKind::Unsupported;
        match Client::connect(location) {
            Ok(c) => return Ok(c),
            Err(e) if mismatch(&e) => return Err(e),
            Err(_) => {}
        }
        let log = location.log_file();
        if let Some(why) = location.start_failed(START_FAILED_FOR) {
            return Err(io::Error::other(format!("it failed to start: {why}; see {} (`ninja-rs -t daemon start` tries again)", log.display())));
        }
        let mut child = start()?;
        let failed = |e: io::Error, why: String| {
            location.note_start(Some(&why));
            io::Error::new(e.kind(), format!("{why}; see {}", log.display()))
        };
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match Client::connect(location) {
                Ok(c) => {
                    location.note_start(None);
                    return Ok(c);
                }
                Err(e) if mismatch(&e) => return Err(e),
                Err(e) => {
                    // One finding another running exits successfully: that
                    // one answers soon.
                    if let Some(Err(how)) = child.as_mut().and_then(Child::exited) {
                        return Err(failed(e, format!("the daemon exited as it started ({how})")));
                    }
                    if Instant::now() >= deadline {
                        return Err(failed(e, format!("the daemon didn't answer in {START_TIMEOUT:?}")));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// A connection answering late (or not at all) isn't used again.
    fn call(&self, req: &Request) -> io::Result<Response> {
        let pooled = self.pool.lock().unwrap().pop();
        let mut stream = match pooled {
            Some(s) => s,
            None => ipc::connect_timed(&self.location, self.timeout)?,
        };
        let late = |e: io::Error| match e.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                io::Error::new(io::ErrorKind::TimedOut, format!("no answer from the daemon in {:?}", self.timeout))
            }
            _ => e,
        };
        proto::write(&mut stream, req).map_err(late)?;
        let resp = proto::read(&mut stream).map_err(late)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the daemon closed the connection"))?;
        self.pool.lock().unwrap().push(stream);
        Ok(resp)
    }

    /// A build starts (see `Begin`).
    pub fn begin(&self, b: Begin) -> io::Result<()> {
        let mut begun = self.begun.lock().unwrap();
        self.begin_with(&b)?;
        *begun = Some(b);
        Ok(())
    }

    fn begin_with(&self, b: &Begin) -> io::Result<u64> {
        match self.call(&Request::Begin(b.clone()))? {
            Response::Begun { session } => {
                self.session.store(session, Ordering::Relaxed);
                Ok(session)
            }
            r => Err(unexpected(r)),
        }
    }

    /// The daemon forgot the build `forgotten` (many others began since):
    /// it begins again, once for every action finding out.
    fn begin_again(&self, forgotten: u64) -> io::Result<u64> {
        let begun = self.begun.lock().unwrap();
        match (self.session.load(Ordering::Relaxed), &*begun) {
            (current, _) if current != forgotten => Ok(current),
            (_, Some(b)) => self.begin_with(b),
            (_, None) => Err(io::Error::other("no build begun")),
        }
    }

    /// Scans a compile command; the outer error is the daemon's. A build
    /// the daemon forgot begins again, once.
    pub fn scan(&self, argv: &[String], cwd: &Path) -> io::Result<Result<cpp_deps::Output, String>> {
        let mut session = self.session.load(Ordering::Relaxed);
        for again in [true, false] {
            let req = Request::Scan { session, argv: argv.to_vec(), cwd: cwd.to_path_buf() };
            match self.call(&req)? {
                Response::Scanned(r) => return Ok(r.map(ScanOutput::into)),
                Response::UnknownBuild if again => session = self.begin_again(session)?,
                Response::UnknownBuild => break,
                r => return Err(unexpected(r)),
            }
        }
        Err(io::Error::other(format!("the daemon forgot build {session} again")))
    }

    pub fn files(&self, paths: &[PathBuf]) -> io::Result<Vec<Result<FileInfo, String>>> {
        match self.call(&Request::Files { paths: paths.to_vec() })? {
            Response::Files(f) if f.len() == paths.len() => Ok(f),
            r => Err(unexpected(r)),
        }
    }

    pub fn tree(&self, dir: &Path) -> io::Result<Result<TreeInfo, String>> {
        match self.call(&Request::Tree { dir: dir.to_path_buf() })? {
            Response::Tree(t) => Ok(t),
            r => Err(unexpected(r)),
        }
    }

    pub fn status(&self) -> io::Result<Status> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(s),
            r => Err(unexpected(r)),
        }
    }

    /// Asks the daemon to exit.
    pub fn stop(&self) -> io::Result<()> {
        match self.call(&Request::Stop)? {
            Response::Stopping => Ok(()),
            r => Err(unexpected(r)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_dirs::TestDir;

    /// A daemon's location in `base` (a directory of its own: it must be
    /// private).
    fn location(base: &TestDir, name: &str) -> Location {
        let loc = Location { dir: base.join("daemon"), id: name.into() };
        loc.create_dir().unwrap();
        loc
    }

    /// Answers requests at `loc` with `answer`, counting the builds begun.
    fn fake_daemon(loc: &Location, answer: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Arc<AtomicU64> {
        let begun = Arc::new(AtomicU64::new(0));
        let mut listener = ipc::Listener::bind(loc).unwrap();
        let (counter, answer) = (begun.clone(), Arc::new(answer));
        std::thread::spawn(move || {
            while let Ok(mut s) = listener.accept() {
                let (counter, answer) = (counter.clone(), answer.clone());
                std::thread::spawn(move || {
                    while let Ok(Some(req)) = proto::read::<Request>(&mut s) {
                        if matches!(req, Request::Begin(_)) {
                            counter.fetch_add(1, Ordering::SeqCst);
                        }
                        if proto::write(&mut s, &answer(&req)).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        begun
    }

    /// A build the daemon forgot (as many others began since as it keeps)
    /// begins again when it scans.
    #[test]
    fn begins_again() {
        let base = TestDir::new("ninja_daemon_client_forgot");
        let loc = location(&base, "forgot");
        let served = loc.clone();
        std::thread::spawn(move || crate::server::serve(&served, Duration::from_secs(3600), None));
        let client = Client::connect_or_start(&loc, || Ok(None)).unwrap();
        client.begin(Begin { build_dir: base.join("long"), ..Default::default() }).unwrap();
        let other = Client::connect(&loc).unwrap();
        for i in 0..=64 {
            other.begin(Begin { build_dir: base.join(format!("other{i}")), ..Default::default() }).unwrap();
        }
        let argv = [base.join("no-such-cc").to_string_lossy().into_owned(), "-c".into(), "x.c".into()];
        assert!(client.scan(&argv, &base).is_ok());
    }

    /// A daemon forgetting the build it just began again: the scan gives
    /// up, after beginning once more (not twice).
    #[test]
    fn begins_again_once() {
        let base = TestDir::new("ninja_daemon_client_forgets");
        let loc = location(&base, "forgets");
        let begun = fake_daemon(&loc, |req| match req {
            Request::Hello { .. } => Response::Hello { protocol: PROTOCOL, pid: 1, version: "fake".into() },
            Request::Begin(_) => Response::Begun { session: 1 },
            _ => Response::UnknownBuild,
        });
        let client = Client::connect(&loc).unwrap();
        client.begin(Begin::default()).unwrap();
        assert!(client.scan(&["cc".into()], &base).is_err());
        assert_eq!(begun.load(Ordering::SeqCst), 2);
    }

    /// A daemon speaking another protocol is given up on at once: none is
    /// started (none could, it holds the place), nor waited for.
    #[test]
    fn other_protocols() {
        let base = TestDir::new("ninja_daemon_client_protocol");
        let loc = location(&base, "protocol");
        fake_daemon(&loc, |_| Response::Hello { protocol: PROTOCOL + 1, pid: 1, version: "next".into() });
        let e = Client::connect_or_start(&loc, || panic!("started another daemon")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
    }

    /// A daemon that exits as it starts (it can't listen, it crashes) is
    /// given up on at once (builds used to wait 10 s for it), and not
    /// started again for a while, but when asked to.
    #[test]
    fn failed_starts() {
        let base = TestDir::new("ninja_daemon_client_failed");
        let loc = location(&base, "failed");
        let shell = |code: i32| {
            #[cfg(unix)]
            let c = ipc::spawn_detached(Path::new("/bin/sh"), &["-c", &format!("exit {code}")], &base, &[], &[]);
            // Bazel's tests don't get `ComSpec`; `SystemRoot` they do.
            #[cfg(windows)]
            let cmd = std::env::var_os("ComSpec")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join(r"System32\cmd.exe"));
            #[cfg(windows)]
            let c = ipc::spawn_detached(&cmd, &["/c", &format!("exit {code}")], &base, &[], &[]);
            c.map(Some)
        };
        let e = Client::connect_or_start(&loc, || shell(3)).unwrap_err();
        assert!(e.to_string().contains("exited as it started") && e.to_string().contains("3"), "{e}");
        let e = Client::connect_or_start(&loc, || panic!("started again")).unwrap_err();
        assert!(e.to_string().contains("failed to start: the daemon exited as it started"), "{e}");
        // Asked to start it: the failure is forgotten, and a daemon that
        // answers is used.
        loc.note_start(None);
        let served = loc.clone();
        std::thread::spawn(move || crate::server::serve(&served, Duration::from_secs(3600), None));
        Client::connect_or_start(&loc, || Ok(None)).unwrap();
        assert_eq!(loc.start_failed(START_FAILED_FOR), None);
    }

    /// A daemon that doesn't answer is given up on.
    #[test]
    fn stuck_daemons() {
        let base = TestDir::new("ninja_daemon_client_stuck");
        let loc = location(&base, "stuck");
        let mut listener = ipc::Listener::bind(&loc).unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok(s) = listener.accept() {
                held.push(s);
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let l = loc.clone();
        std::thread::spawn(move || tx.send(Client::connect_timeout(&l, Duration::from_millis(300)).map(drop)));
        let r = rx.recv_timeout(Duration::from_secs(60)).expect("given up on in time");
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
