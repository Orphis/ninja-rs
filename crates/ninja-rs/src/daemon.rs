//! `--daemon` and `-t daemon`: the daemon of this ninja-rs binary (see
//! `ninja_daemon`), started by the first build that wants it.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use ninja_core::graph::EdgeId;
use ninja_core::runner::{AbortFuture, CommandRunner, ExecKind, Task, WaitFuture};
use ninja_core::status::{print_err, print_out};
use ninja_daemon::{Client, Location, ipc};

/// How long the daemon waits for builds before exiting
/// (`NINJA_RS_DAEMON_IDLE`, in seconds).
const DEFAULT_IDLE: Duration = Duration::from_secs(3 * 3600);

/// The binary a daemon comes from (on Windows, it runs a copy): in its
/// environment, which has room for any path (its command line, read as
/// Unicode, doesn't).
const EXE_ENV: &str = "NINJA_RS_DAEMON_EXE";

fn location() -> io::Result<Location> {
    Location::for_exe(&std::env::current_exe()?)
}

/// Connects to the daemon, starting it when none runs (unless one failed
/// to start a moment ago).
pub fn connect() -> io::Result<Client> {
    let loc = location()?;
    Client::connect_or_start(&loc, || {
        loc.create_dir()?;
        // It exits once this binary is rebuilt (see `server::serve`). It
        // serves every build: nothing of this one's (its secret flags, its
        // logs' lock) goes to it.
        let exe = std::env::current_exe()?;
        let args = ["-t", "daemon", "run", "--id", &loc.id];
        let env_remove = [crate::nested::ENV, crate::nested::LOGS_LOCK_ENV];
        ipc::spawn_detached(&program(&loc)?, &args, &loc.dir, &env_remove, &[(EXE_ENV, exe.as_os_str())]).map(Some)
    })
}

/// Runs commands with `inner`, telling the daemon and the remote backend
/// when one ran on the host: what it wrote without naming it (a generator's
/// tree, a step writing into the source tree or the build directory),
/// which the next action may read, is seen by the next requests (see
/// `ninja_daemon::watch::Watcher::fence`) and scans (see
/// `cpp_deps::Cache::host_wrote`).
pub struct Telling {
    pub inner: Box<dyn CommandRunner>,
    pub client: Option<Arc<Client>>,
    pub remote: Option<Arc<dyn ninja_core::runner::RemoteBackend>>,
}

impl CommandRunner for Telling {
    fn can_run_more(&self) -> usize {
        self.inner.can_run_more()
    }

    fn start_command(&mut self, task: Task) -> Result<(), String> {
        self.inner.start_command(task)
    }

    fn wait_for_command(&mut self) -> WaitFuture<'_> {
        let (client, remote) = (self.client.clone(), self.remote.clone());
        let next = self.inner.wait_for_command();
        Box::pin(async move {
            let done = next.await;
            if done.as_ref().is_some_and(|r| r.kind == ExecKind::Local) {
                if let Some(c) = client {
                    c.ran_locally();
                }
                if let Some(r) = remote {
                    r.host_wrote();
                }
            }
            done
        })
    }

    fn active_edges(&self) -> Vec<EdgeId> {
        self.inner.active_edges()
    }

    fn abort(&mut self) -> AbortFuture<'_> {
        self.inner.abort()
    }

    fn wants_inputs(&self) -> bool {
        self.inner.wants_inputs()
    }

    fn slot_kinds(&self) -> usize {
        self.inner.slot_kinds()
    }

    fn slots_of(&self, task: &Task) -> usize {
        self.inner.slots_of(task)
    }

    fn has_free_slot(&self, kind: usize) -> bool {
        self.inner.has_free_slot(kind)
    }
}

/// What the daemon runs: this program, or on Windows, where a running
/// program can't be replaced, a copy in the daemon's directory (so that
/// ninja-rs can be rebuilt or upgraded while it runs).
fn program(loc: &Location) -> io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    if !cfg!(windows) {
        return Ok(exe);
    }
    let copy = loc.dir.join(format!("{}.exe", loc.id));
    if copy.is_file() {
        return Ok(copy);
    }
    // The files of other versions, unless running.
    loc.remove_others()?;
    let tmp = loc.dir.join(format!("{}.{}.tmp", loc.id, std::process::id()));
    std::fs::copy(&exe, &tmp)?;
    if std::fs::rename(&tmp, &copy).is_err() {
        // Another build made it meanwhile.
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(copy)
}

const USAGE: &str = "usage: ninja-rs -t daemon [status|start|stop|run]

The daemon keeps what builds with --daemon learn (scanned headers, compiler
profiles, file digests, toolchain trees) and watches the files they come
from, so that a build checks again only what changed. Builds start it (not
for 5 minutes after it failed to, or didn't answer); it exits after
NINJA_RS_DAEMON_IDLE seconds without builds [default=10800].

  status  what it keeps and watches (the default)
  start   start it
  stop    stop it
  run     run it in the foreground
";

/// The directories builds write to `status` lists (those named last).
const IGNORED_SHOWN: usize = 20;

fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// `-t daemon`. Its output, as the other tools', is lost when it can't be
/// written (see `print_out`).
pub fn tool(args: &[String]) -> i32 {
    // A daemon running a copy is told its name, and the binary it comes
    // from.
    let id = match args {
        [run, flag, id, ..] if run == "run" && flag == "--id" => Some(id),
        _ => None,
    };
    let exe = match (id, std::env::var_os(EXE_ENV)) {
        (Some(_), Some(exe)) => Some(std::path::PathBuf::from(exe)),
        _ => std::env::current_exe().ok(),
    };
    let error = |e: &str| print_err(&format!("ninja: error: {e}\n"));
    let loc = match id.map_or_else(location, |id| Location::with_id(id)) {
        Ok(l) => l,
        Err(e) => {
            error(&format!("the daemon's location: {e}"));
            return 1;
        }
    };
    match args.first().map(String::as_str).unwrap_or("status") {
        "run" => {
            let idle = std::env::var("NINJA_RS_DAEMON_IDLE").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(DEFAULT_IDLE);
            ninja_daemon::server::set_version(crate::version());
            match ninja_daemon::server::serve(&loc, idle, exe) {
                Ok(()) => 0,
                Err(e) => {
                    error(&format!("the daemon: {e}"));
                    1
                }
            }
        }
        "start" => {
            // Even if one failed to a moment ago.
            loc.note_start(None);
            match connect().and_then(|c| c.status()) {
                Ok(s) => {
                    print_out(&format!("ninja-rs daemon running (pid {})\n", s.pid));
                    0
                }
                Err(e) => {
                    error(&format!("starting the daemon: {e}"));
                    1
                }
            }
        }
        "stop" => match Client::connect(&loc) {
            Ok(c) => match c.stop() {
                Ok(()) => {
                    print_out("ninja-rs daemon stopped\n");
                    0
                }
                Err(e) => {
                    error(&format!("stopping the daemon: {e}"));
                    1
                }
            },
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                error(&format!("stopping the daemon: it doesn't answer ({e}); see {}", loc.log_file().display()));
                1
            }
            Err(_) => {
                print_out("ninja-rs daemon not running\n");
                0
            }
        },
        "status" => match Client::connect(&loc).and_then(|c| c.status()) {
            Ok(s) => {
                use std::fmt::Write as _;
                let mut text = format!("ninja-rs daemon: pid {}, ninja-rs {}, up {}\n", s.pid, s.version, duration(s.uptime_secs));
                let _ = writeln!(text, "  {} builds, {} requests, {} file events", s.builds, s.requests, s.events);
                let _ = writeln!(
                    text,
                    "  preprocessor: {} files and {} directory listings kept ({} files lexed, {} reused)",
                    s.lexed, s.listings, s.files_lexed, s.files_reused
                );
                let _ = writeln!(text, "  {} file digests, {} toolchain trees, {} directories watched", s.digests, s.trees, s.watched_dirs);
                if !s.roots.is_empty() {
                    text.push_str("  roots:\n");
                    for (p, state) in &s.roots {
                        let _ = writeln!(text, "    {}: {state}", p.display());
                    }
                }
                if !s.ignored.is_empty() {
                    let _ = writeln!(text, "  directories builds write to, checked every time ({}):", s.ignored.len());
                    for (p, whole) in s.ignored.iter().take(IGNORED_SHOWN) {
                        let _ = writeln!(text, "    {}{}", p.display(), if *whole { "" } else { " (but what roots in sync there watch)" });
                    }
                    if s.ignored.len() > IGNORED_SHOWN {
                        let _ = writeln!(text, "    and {} more", s.ignored.len() - IGNORED_SHOWN);
                    }
                }
                let _ = writeln!(text, "  log: {}", loc.log_file().display());
                print_out(&text);
                0
            }
            // Its place is held (it holds the lock): another can't start.
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                print_out(&format!("ninja-rs daemon running, but it doesn't answer ({e})\n  log: {}\n", loc.log_file().display()));
                1
            }
            Err(_) => {
                print_out("ninja-rs daemon not running\n");
                if let Some(why) = loc.start_failed(ipc::START_FAILED_FOR) {
                    print_out(&format!("  {why}\n  log: {}\n", loc.log_file().display()));
                }
                0
            }
        },
        "-h" | "--help" | "help" => {
            print_out(USAGE);
            0
        }
        other => {
            print_err(&format!("ninja: error: unknown daemon command '{other}'\n{USAGE}"));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninja_core::runner::{ExitStatus, TaskResult};
    use ninja_daemon::proto::{self, PROTOCOL, Request, Response};
    use std::sync::Mutex;

    /// Gives the results it's made with.
    struct Finishing(Vec<ExecKind>);

    impl CommandRunner for Finishing {
        fn can_run_more(&self) -> usize {
            1
        }
        fn start_command(&mut self, _: Task) -> Result<(), String> {
            Ok(())
        }
        fn wait_for_command(&mut self) -> WaitFuture<'_> {
            let kind = self.0.pop();
            Box::pin(async move {
                kind.map(|kind| TaskResult {
                    edge: EdgeId(0),
                    status: ExitStatus::Success,
                    output: Vec::new(),
                    kind,
                    cas_outputs: Vec::new(),
                    lost_inputs: Vec::new(),
                })
            })
        }
        fn active_edges(&self) -> Vec<EdgeId> {
            Vec::new()
        }
        fn abort(&mut self) -> AbortFuture<'_> {
            Box::pin(std::future::ready(()))
        }
    }

    /// A command run on the host is told to the daemon (the next request
    /// says so, and what it wrote is seen first); one run remotely isn't.
    #[test]
    fn local_commands_told() {
        let base = test_dirs::TestDir::new("ninja_rs_daemon_told");
        let loc = Location { dir: base.join("daemon"), id: "told".into() };
        loc.create_dir().unwrap();
        // A daemon noting what requests say.
        let said: Arc<Mutex<Vec<bool>>> = Arc::default();
        let mut listener = ipc::Listener::bind(&loc).unwrap();
        let noted = said.clone();
        std::thread::spawn(move || {
            while let Ok(mut s) = listener.accept() {
                let noted = noted.clone();
                std::thread::spawn(move || {
                    while let Ok(Some(req)) = proto::read::<Request>(&mut s) {
                        let answer = match req {
                            Request::Files { paths, wrote } => {
                                noted.lock().unwrap().push(wrote);
                                Response::Files(paths.iter().map(|_| Err("none".to_string())).collect())
                            }
                            _ => Response::Hello { protocol: PROTOCOL, pid: 1, version: "fake".into() },
                        };
                        if proto::write(&mut s, &answer).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let client = Arc::new(Client::connect(&loc).unwrap());
        let backend = Arc::new(Wrote::default());
        let mut runner =
            Telling { inner: Box::new(Finishing(vec![ExecKind::Local, ExecKind::Remote])), client: Some(client.clone()), remote: Some(backend.clone()) };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let ask = || client.files(&[base.join("a.h")]).unwrap();
        rt.block_on(runner.wait_for_command()).unwrap();
        ask();
        rt.block_on(runner.wait_for_command()).unwrap();
        ask();
        ask();
        assert_eq!(*said.lock().unwrap(), [false, true, false]);
        // The remote backend is told too, of the local command only.
        assert_eq!(backend.0.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// A remote backend counting what it's told of commands on the host.
    #[derive(Default)]
    struct Wrote(std::sync::atomic::AtomicUsize);

    impl ninja_core::runner::RemoteBackend for Wrote {
        fn wants(&self, _: &Task) -> bool {
            false
        }
        fn execute(&self, _: Task) -> ninja_core::runner::RemoteFuture {
            unreachable!()
        }
        fn max_jobs(&self) -> usize {
            1
        }
        fn host_wrote(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
