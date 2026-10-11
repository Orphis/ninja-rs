//! `--daemon` and `-t daemon`: the daemon of this ninja-rs binary (see
//! `ninja_daemon`), started by the first build that wants it.

use std::io;
use std::time::Duration;

use ninja_daemon::{Client, Location, ipc};

/// How long the daemon waits for builds before exiting
/// (`NINJA_RS_DAEMON_IDLE`, in seconds).
const DEFAULT_IDLE: Duration = Duration::from_secs(3 * 3600);

fn location() -> io::Result<Location> {
    Location::for_exe(&std::env::current_exe()?)
}

/// Connects to the daemon, starting it when none runs.
pub fn connect() -> io::Result<Client> {
    let loc = location()?;
    Client::connect_or_start(&loc, || {
        loc.create_dir()?;
        ipc::spawn_detached(&program(&loc)?, &["-t", "daemon", "run", "--id", &loc.id], &loc.dir)
    })
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
    for e in std::fs::read_dir(&loc.dir)?.flatten() {
        if e.path().extension().is_some_and(|x| x == "exe" || x == "lock" || x == "log") {
            let _ = std::fs::remove_file(e.path());
        }
    }
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
from, so that a build checks again only what changed. Builds start it; it
exits after NINJA_RS_DAEMON_IDLE seconds without builds [default=10800].

  status  what it keeps and watches (the default)
  start   start it
  stop    stop it
  run     run it in the foreground
";

fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// `-t daemon`.
pub fn tool(args: &[String]) -> i32 {
    // A daemon running a copy is told its name.
    let id = match args {
        [run, flag, id, ..] if run == "run" && flag == "--id" => Some(id),
        _ => None,
    };
    let loc = match id.map_or_else(location, |id| Location::with_id(id)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ninja-rs: error: the daemon's location: {e}");
            return 1;
        }
    };
    match args.first().map(String::as_str).unwrap_or("status") {
        "run" => {
            let idle = std::env::var("NINJA_RS_DAEMON_IDLE").ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(DEFAULT_IDLE);
            match ninja_daemon::server::serve(&loc, idle) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("ninja-rs: error: the daemon: {e}");
                    1
                }
            }
        }
        "start" => match connect().and_then(|c| c.status()) {
            Ok(s) => {
                println!("ninja-rs daemon running (pid {})", s.pid);
                0
            }
            Err(e) => {
                eprintln!("ninja-rs: error: starting the daemon: {e}");
                1
            }
        },
        "stop" => match Client::connect(&loc) {
            Ok(c) => match c.stop() {
                Ok(()) => {
                    println!("ninja-rs daemon stopped");
                    0
                }
                Err(e) => {
                    eprintln!("ninja-rs: error: stopping the daemon: {e}");
                    1
                }
            },
            Err(_) => {
                println!("ninja-rs daemon not running");
                0
            }
        },
        "status" => match Client::connect(&loc).and_then(|c| c.status()) {
            Ok(s) => {
                println!("ninja-rs daemon: pid {}, ninja-rs {}, up {}", s.pid, s.version, duration(s.uptime_secs));
                println!("  {} builds, {} requests, {} file events", s.builds, s.requests, s.events);
                println!(
                    "  preprocessor: {} files and {} directory listings kept ({} files lexed, {} reused)",
                    s.lexed, s.listings, s.files_lexed, s.files_reused
                );
                println!("  {} file digests, {} toolchain trees", s.digests, s.trees);
                if !s.roots.is_empty() {
                    println!("  roots:");
                    for (p, state) in &s.roots {
                        println!("    {}: {state}", p.display());
                    }
                }
                println!("  log: {}", loc.log_file().display());
                0
            }
            Err(_) => {
                println!("ninja-rs daemon not running");
                0
            }
        },
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprint!("ninja-rs: error: unknown daemon command '{other}'\n{USAGE}");
            1
        }
    }
}
