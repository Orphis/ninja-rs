//! What builds use to talk to the daemon: connections kept in a pool, one
//! request at a time on each, so that concurrent actions ask in parallel.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::ipc::{self, Location, TimedStream};
use crate::proto::{self, Begin, FileInfo, PROTOCOL, Request, Response, ScanOutput, Status, TreeInfo};

/// How long the daemon may take to answer: listing a large toolchain the
/// first time, or a scan probing a compiler, takes seconds. A daemon stuck
/// longer is given up on (the build does the work itself).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

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
            Response::Hello { protocol, pid, .. } => Err(io::Error::other(format!("the daemon (pid {pid}) speaks protocol {protocol}, not {PROTOCOL}"))),
            r => Err(unexpected(r)),
        }
    }

    /// Connects, starting the daemon with `start` when none answers.
    pub fn connect_or_start(location: &Location, start: impl FnOnce() -> io::Result<()>) -> io::Result<Client> {
        if let Ok(c) = Client::connect(location) {
            return Ok(c);
        }
        start()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Client::connect(location) {
                Ok(c) => return Ok(c),
                Err(e) if Instant::now() >= deadline => return Err(io::Error::new(e.kind(), format!("the daemon didn't start: {e}"))),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
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

    /// Scans a compile command; the outer error is the daemon's.
    pub fn scan(&self, argv: &[String], cwd: &Path) -> io::Result<Result<cpp_deps::Output, String>> {
        let mut session = self.session.load(Ordering::Relaxed);
        for _ in 0..2 {
            let req = Request::Scan { session, argv: argv.to_vec(), cwd: cwd.to_path_buf() };
            match self.call(&req)? {
                Response::Scanned(r) => return Ok(r.map(ScanOutput::into)),
                Response::UnknownBuild => session = self.begin_again(session)?,
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

    fn location(name: &str) -> Location {
        let dir = std::env::temp_dir().join(format!("ninja_daemon_client_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let loc = Location { dir, id: name.into() };
        loc.create_dir().unwrap();
        loc
    }

    /// A build the daemon forgot (as many others began since as it keeps)
    /// begins again when it scans.
    #[test]
    fn begins_again() {
        let loc = location("forgot");
        let served = loc.clone();
        std::thread::spawn(move || crate::server::serve(&served, Duration::from_secs(3600), None));
        let client = Client::connect_or_start(&loc, || Ok(())).unwrap();
        client.begin(Begin { build_dir: loc.dir.join("long"), ..Default::default() }).unwrap();
        let other = Client::connect(&loc).unwrap();
        for i in 0..=64 {
            other.begin(Begin { build_dir: loc.dir.join(format!("other{i}")), ..Default::default() }).unwrap();
        }
        let argv = [loc.dir.join("no-such-cc").to_string_lossy().into_owned(), "-c".into(), "x.c".into()];
        assert!(client.scan(&argv, &loc.dir).is_ok());
        let _ = std::fs::remove_dir_all(&loc.dir);
    }

    /// A daemon that doesn't answer is given up on.
    #[test]
    fn stuck_daemons() {
        let loc = location("stuck");
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
        let r = rx.recv_timeout(Duration::from_secs(10)).expect("given up on in time");
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);
        let _ = std::fs::remove_dir_all(&loc.dir);
    }
}
