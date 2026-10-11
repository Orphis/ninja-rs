//! What builds use to talk to the daemon: connections kept in a pool, one
//! request at a time on each, so that concurrent actions ask in parallel.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::ipc::{self, Location, Stream};
use crate::proto::{self, Begin, FileInfo, PROTOCOL, Request, Response, ScanOutput, Status, TreeInfo};

pub struct Client {
    location: Location,
    pool: Mutex<Vec<Stream>>,
    /// The build begun (0: none).
    session: AtomicU64,
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
        let client = Client { location: location.clone(), pool: Mutex::default(), session: AtomicU64::new(0) };
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

    fn call(&self, req: &Request) -> io::Result<Response> {
        let pooled = self.pool.lock().unwrap().pop();
        let mut stream = match pooled {
            Some(s) => s,
            None => ipc::connect(&self.location)?,
        };
        proto::write(&mut stream, req)?;
        let resp = proto::read(&mut stream)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the daemon closed the connection"))?;
        self.pool.lock().unwrap().push(stream);
        Ok(resp)
    }

    /// A build starts (see `Begin`).
    pub fn begin(&self, b: Begin) -> io::Result<()> {
        match self.call(&Request::Begin(b))? {
            Response::Begun { session } => {
                self.session.store(session, Ordering::Relaxed);
                Ok(())
            }
            r => Err(unexpected(r)),
        }
    }

    /// Scans a compile command; the outer error is the daemon's.
    pub fn scan(&self, argv: &[String], cwd: &Path) -> io::Result<Result<cpp_deps::Output, String>> {
        let req = Request::Scan { session: self.session.load(Ordering::Relaxed), argv: argv.to_vec(), cwd: cwd.to_path_buf() };
        match self.call(&req)? {
            Response::Scanned(r) => Ok(r.map(ScanOutput::into)),
            r => Err(unexpected(r)),
        }
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
