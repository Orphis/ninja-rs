//! `--daemon` and `-t daemon`: the daemon of this ninja-rs binary (see
//! `ninja_daemon`), started by the first build that wants it.

use std::io;
use std::time::Duration;

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
for 5 minutes after it failed to); it exits after NINJA_RS_DAEMON_IDLE
seconds without builds [default=10800].

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
                    print_out(&format!(
                        "ninja-rs daemon running (pid {})
",
                        s.pid
                    ));
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
            Err(_) => {
                print_out(
                    "ninja-rs daemon not running
",
                );
                if let Some(why) = loc.start_failed(ipc::START_FAILED_FOR) {
                    print_out(&format!(
                        "  it failed to start: {why}
  log: {}
",
                        loc.log_file().display()
                    ));
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
