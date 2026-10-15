//! Local command execution (ninja's `Subprocess`/`SubprocessSet`).
//!
//! As in ninja, commands run through `/bin/sh -c` on POSIX and are passed
//! to `CreateProcess` verbatim on Windows. On POSIX, a command without
//! shell syntax runs directly instead (see `simple_argv`). stdout and stderr share one pipe
//! so their interleaving is preserved; `console` pool commands inherit the
//! terminal instead.
//!
//! As in ninja, every other command runs in a process group of its own on
//! POSIX, and in a job object on Windows: the terminal's Ctrl-C reaches
//! ninja-rs only, and stopping a command (an interrupted or failed build)
//! reaches every process it started. They get the interrupt's signal
//! (Ctrl-Break on Windows, as ninja sends; without a console, the job is
//! terminated), and the command's own process is waited for, as ninja waits
//! for it, until another interrupt kills them all. Processes it leaves
//! behind once it exited get a grace (`STOP_GRACE`), then are killed. A
//! raced command losing is killed at once. `console` commands stay in the
//! foreground and get the terminal's signals directly.

use std::future::Future;
use std::io::Read;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::runner::ExitStatus;

#[cfg(unix)]
use libc::{SIGINT, SIGKILL, SIGTERM};
// Windows has no signals: stopping a command terminates its job.
#[cfg(not(unix))]
const SIGINT: i32 = 2;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;

/// Set when the user interrupts the build: SIGINT, SIGTERM or SIGHUP on
/// POSIX, Ctrl-C or Ctrl-Break on Windows.
///
/// A build's loop acts on interrupts (see `handle`): the first one stops
/// the build, the next one kills the commands still running. Anywhere else
/// (loading the manifest, connecting, a tool, a build with nothing to do),
/// one ends the invocation (see `unhandled`), as ninja dies of the signal
/// there, once its build events end.
#[derive(Default)]
pub struct Interrupt {
    /// How many interrupts arrived.
    count: AtomicUsize,
    /// The first interrupt's signal, which commands get too.
    signal: AtomicI32,
    /// The last interrupt's signal.
    last: AtomicI32,
    /// Who sent the last interrupt (see `SENT_BY_PROCESS`).
    sender: AtomicI32,
    /// How many holders of `handle` act on interrupts, and the signal of an
    /// interrupt that came while there was none.
    handling: Mutex<(usize, Option<i32>)>,
    notify: Notify,
}

/// Who sent a signal, as its `si_code` tells (Linux): a terminal (the
/// kernel, to its whole foreground process group) or a process (`kill(2)`,
/// to ninja-rs alone); 0 when that can't be told.
const SENT_BY_TERMINAL: i32 = 1;
const SENT_BY_PROCESS: i32 = 2;

/// The sender of the last SIGINT and SIGHUP, noted by `note_sender`.
#[cfg(any(target_os = "linux", target_os = "android"))]
static SENDERS: [AtomicI32; 2] = [AtomicI32::new(0), AtomicI32::new(0)];

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sender_slot(signal: i32) -> Option<&'static AtomicI32> {
    match signal {
        libc::SIGINT => Some(&SENDERS[0]),
        libc::SIGHUP => Some(&SENDERS[1]),
        _ => None,
    }
}

/// Notes who sent a signal: runs before tokio's handler, which calls the
/// one it replaced first. Async-signal-safe: an atomic store.
#[cfg(any(target_os = "linux", target_os = "android"))]
extern "C" fn note_sender(signal: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: with SA_SIGINFO, the kernel passes a valid siginfo.
    if let (Some(slot), Some(info)) = (sender_slot(signal), unsafe { info.as_ref() }) {
        // `SI_FROMUSER`: kill(2), sigqueue(3) and the like.
        slot.store(if info.si_code <= 0 { SENT_BY_PROCESS } else { SENT_BY_TERMINAL }, Ordering::SeqCst);
    }
}

/// Installs `note_sender` for `signal`, unless it has a handler already
/// (tokio's: replacing it would lose the signal). Also when it is ignored
/// (a background job of a shell without job control inherits SIGINT so):
/// tokio's handler replaces that anyway, as ninja's does.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn note_senders_of(signal: i32) {
    // SAFETY: plain sigaction calls with initialized structures; the
    // handler only stores to an atomic.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signal, std::ptr::null(), &mut current) != 0 || ![libc::SIG_DFL, libc::SIG_IGN].contains(&current.sa_sigaction) {
            return;
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = note_sender as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
}

/// Who sent the last `signal` (see `note_sender`).
#[cfg(unix)]
fn sender_of(signal: i32) -> i32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(slot) = sender_slot(signal) {
        return slot.load(Ordering::SeqCst);
    }
    let _ = signal;
    0
}

/// See [`Interrupt::handle`].
pub struct Handling(Arc<Interrupt>);

impl Drop for Handling {
    fn drop(&mut self) {
        self.0.handling.lock().unwrap_or_else(PoisonError::into_inner).0 -= 1;
    }
}

impl Interrupt {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Interrupts as SIGINT does.
    pub fn trigger(&self) {
        self.trigger_with(SIGINT, 0);
    }

    fn trigger_with(&self, signal: i32, sender: i32) {
        let _ = self.signal.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
        self.last.store(signal, Ordering::SeqCst);
        self.sender.store(sender, Ordering::SeqCst);
        {
            // Counted under the lock: a holder of `handle` sees it, or it is
            // unhandled.
            let mut handling = self.handling.lock().unwrap_or_else(PoisonError::into_inner);
            if handling.0 == 0 {
                handling.1.get_or_insert(signal);
            }
            self.count.fetch_add(1, Ordering::SeqCst);
        }
        self.notify.notify_waiters();
    }

    /// The last interrupt's signal (`SIGINT` on Windows); 0 before any.
    pub fn last_signal(&self) -> i32 {
        self.last.load(Ordering::SeqCst)
    }

    /// Whether a process sent the last interrupt (`kill(2)`), rather than a
    /// terminal (to its whole foreground process group); None where that
    /// can't be told (Linux tells).
    pub fn last_sent_by_process(&self) -> Option<bool> {
        match self.sender.load(Ordering::SeqCst) {
            SENT_BY_TERMINAL => Some(false),
            SENT_BY_PROCESS => Some(true),
            _ => None,
        }
    }

    /// Until the guard returned is dropped, its holder acts on interrupts
    /// (a build stopping its commands, `-t exec` passing them on): they
    /// don't end the invocation at once (see `unhandled`).
    pub fn handle(self: &Arc<Self>) -> Handling {
        self.handling.lock().unwrap_or_else(PoisonError::into_inner).0 += 1;
        Handling(self.clone())
    }

    /// Completes once an interrupt came while nothing handled it, with its
    /// signal: the invocation ends.
    pub async fn unhandled(&self) -> i32 {
        loop {
            let n = self.notify.notified();
            if let Some(signal) = self.handling.lock().unwrap_or_else(PoisonError::into_inner).1 {
                return signal;
            }
            n.await;
        }
    }

    pub fn is_set(&self) -> bool {
        self.count() > 0
    }

    /// How many interrupts arrived.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        self.wait_beyond(0).await;
    }

    /// Waits until more than `count` interrupts arrived.
    pub async fn wait_beyond(&self, count: usize) {
        loop {
            let n = self.notify.notified();
            if self.count() > count {
                return;
            }
            n.await;
        }
    }

    /// Listens for interrupts for the rest of the process lifetime. As in
    /// ninja, SIGTERM and SIGHUP interrupt a build as SIGINT does, and
    /// Ctrl-Break as Ctrl-C does. Call it before anything else listens for
    /// signals: who sends them is noted first (see `last_sent_by_process`).
    pub fn install_handlers(self: &Arc<Self>) {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            #[cfg(any(target_os = "linux", target_os = "android"))]
            for number in [SIGINT, libc::SIGHUP] {
                note_senders_of(number);
            }
            for (kind, number) in [(SignalKind::interrupt(), SIGINT), (SignalKind::terminate(), SIGTERM), (SignalKind::hangup(), libc::SIGHUP)] {
                let Ok(mut events) = signal(kind) else { continue };
                let me = self.clone();
                tokio::spawn(async move {
                    while events.recv().await.is_some() {
                        me.trigger_with(number, sender_of(number));
                    }
                });
            }
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows::{ctrl_break, ctrl_c};
            if let Ok(mut events) = ctrl_c() {
                let me = self.clone();
                tokio::spawn(async move {
                    while events.recv().await.is_some() {
                        me.trigger();
                    }
                });
            }
            if let Ok(mut events) = ctrl_break() {
                let me = self.clone();
                tokio::spawn(async move {
                    while events.recv().await.is_some() {
                        me.trigger();
                    }
                });
            }
        }
    }
}

/// Stops the commands a runner started, and tells when they have all
/// exited: a build interrupted or failing waits for them before deleting
/// the outputs they were writing (see `CommandRunner::abort`).
#[derive(Default)]
pub struct Stop {
    /// 0 while commands may run, then the signal they get.
    signal: AtomicI32,
    /// What still runs is killed.
    killing: AtomicBool,
    /// Commands started and not yet gone.
    running: AtomicUsize,
    notify: Notify,
}

impl Stop {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Counts a command about to start; `None` once stopping (it must not
    /// start). Counted first, so that `stop` waits for a command starting
    /// as it begins.
    fn enter(&self) -> Option<Running<'_>> {
        self.running.fetch_add(1, Ordering::SeqCst);
        let running = Running(self);
        (self.signal() == 0).then_some(running)
    }

    fn signal(&self) -> i32 {
        self.signal.load(Ordering::SeqCst)
    }

    fn killing(&self) -> bool {
        self.killing.load(Ordering::SeqCst)
    }

    async fn until(&self, done: impl Fn(&Self) -> bool) {
        loop {
            let n = self.notify.notified();
            if done(self) {
                return;
            }
            n.await;
        }
    }

    /// Stops every command and waits until all have exited. Those in a
    /// process group of their own get the interrupt's signal (SIGTERM
    /// without one; Ctrl-Break on Windows, or their jobs terminated without
    /// a console), console ones only the terminal's (as in ninja). Another
    /// interrupt meanwhile kills what still runs.
    pub async fn stop(&self, interrupt: Option<&Interrupt>) {
        let seen = interrupt.map_or(0, Interrupt::count);
        let signal = interrupt.map(|i| i.signal.load(Ordering::SeqCst)).filter(|&s| s != 0).unwrap_or(SIGTERM);
        let _ = self.signal.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
        self.notify.notify_waiters();
        let gone = self.until(|s| s.running.load(Ordering::SeqCst) == 0);
        tokio::pin!(gone);
        let again = async {
            match interrupt {
                Some(i) => i.wait_beyond(seen).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = &mut gone => return,
            _ = again => {}
        }
        self.killing.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        gone.await;
    }
}

/// A command counted by [`Stop::enter`].
struct Running<'a>(&'a Stop);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
}

/// The result of a command stopped before or while it ran.
fn stopped() -> (ExitStatus, Vec<u8>) {
    (ExitStatus::Interrupted, Vec::new())
}

/// Splits a Windows command line into the program and the rest, following
/// `CommandLineToArgvW` rules for argv[0].
pub fn split_windows_program(command: &str) -> (String, &str) {
    let s = command.trim_start_matches([' ', '\t']);
    if let Some(rest) = s.strip_prefix('"') {
        match rest.find('"') {
            Some(i) => (rest[..i].to_string(), rest[i + 1..].trim_start_matches([' ', '\t'])),
            None => (rest.to_string(), ""),
        }
    } else {
        match s.find([' ', '\t']) {
            Some(i) => (s[..i].to_string(), s[i..].trim_start_matches([' ', '\t'])),
            None => (s.to_string(), ""),
        }
    }
}

/// Shell builtins and keywords: a command starting with one needs the
/// shell, even when a program of that name exists (`pwd`, `time`).
const SHELL_WORDS: &[&str] = &[
    ".",
    ":",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "case",
    "cd",
    "command",
    "compgen",
    "complete",
    "continue",
    "declare",
    "dirs",
    "disown",
    "do",
    "done",
    "elif",
    "else",
    "enable",
    "esac",
    "eval",
    "exec",
    "exit",
    "export",
    "fc",
    "fg",
    "fi",
    "for",
    "function",
    "getopts",
    "hash",
    "help",
    "history",
    "if",
    "in",
    "jobs",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "select",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "then",
    "time",
    "times",
    "trap",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "until",
    "wait",
    "while",
];

/// The words of `command`, when `/bin/sh -c` would just run a program with
/// those arguments: no quoting, expansion, redirection, globbing or other
/// shell syntax, no variable assignment, and not a shell builtin or
/// keyword. Such commands run directly on POSIX, saving a shell per
/// command (ninja issue 2665).
pub fn simple_argv(command: &str) -> Option<Vec<&str>> {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_-+=/.,:@%".contains(c) || c == ' ' || c == '\t';
    if !command.chars().all(plain) {
        return None;
    }
    let words: Vec<&str> = command.split([' ', '\t']).filter(|w| !w.is_empty()).collect();
    let first = *words.first()?;
    // `NAME=value cmd` assigns a variable.
    if first.contains('=') || SHELL_WORDS.contains(&first) {
        return None;
    }
    Some(words)
}

fn build_command(command: &str, direct: bool) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let _ = direct;
        let (program, rest) = split_windows_program(command);
        // CreateProcess also searches the current directory; std does not.
        let program = if !program.contains(['/', '\\']) && std::path::Path::new(&program).is_file() { format!(".\\{program}") } else { program };
        let mut c = tokio::process::Command::new(program);
        if !rest.is_empty() {
            c.raw_arg(rest);
        }
        c
    }
    #[cfg(not(windows))]
    {
        if direct && let Some(argv) = simple_argv(command) {
            let mut c = tokio::process::Command::new(argv[0]);
            c.args(&argv[1..]);
            return c;
        }
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Runs one command. Returns the exit status and combined stdout/stderr.
pub async fn run_local_command(command: &str, use_console: bool) -> (ExitStatus, Vec<u8>) {
    run_local_command_env(command, use_console, &[], &Stop::default()).await
}

/// Builds the command to run `command` locally, with `env` added.
fn local_command(command: &str, direct: bool, env: &[(String, String)]) -> tokio::process::Command {
    let mut cmd = build_command(command, direct);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.kill_on_drop(true);
    cmd
}

/// [`run_local_command`] with extra environment variables, stopped by
/// `stop` (see [`Stop::stop`]).
pub async fn run_local_command_env(command: &str, use_console: bool, env: &[(String, String)], stop: &Stop) -> (ExitStatus, Vec<u8>) {
    if !use_console {
        return run_piped(Spec::Shell(command), env, stop, None::<std::future::Pending<()>>).await.expect("never cancelled");
    }
    let Some(_running) = stop.enter() else { return stopped() };
    let direct = cfg!(not(windows)) && simple_argv(command).is_some();
    // A program run directly that can't start (missing, not executable)
    // runs through the shell instead, for the shell's message and status.
    let spawn = |direct: bool| local_command(command, direct, env).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn();
    let spawned = match spawn(direct) {
        Err(_) if direct => spawn(false),
        r => r,
    };
    match spawned {
        Ok(mut child) => {
            // In the foreground, it gets the terminal's signals as ninja-rs
            // does: a stop waits for it, unless it kills.
            let status = tokio::select! {
                st = child.wait() => st,
                _ = stop.until(Stop::killing) => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            match status {
                Ok(st) => (exit_status(st), Vec::new()),
                Err(e) => (ExitStatus::Failure(1), format!("waiting for command: {e}\n").into_bytes()),
            }
        }
        Err(e) => (ExitStatus::Failure(1), format!("{e}\n").into_bytes()),
    }
}

/// Runs one command (not a console one) like [`run_local_command_env`],
/// until `cancel` resolves: then the command and every process it started
/// are killed, and `None` is returned once they have exited. Racing uses
/// it: a remote result replaces the outputs only once nothing local writes
/// them.
pub async fn run_local_command_cancellable(
    command: &str,
    env: &[(String, String)],
    stop: &Stop,
    cancel: impl Future<Output = ()>,
) -> Option<(ExitStatus, Vec<u8>)> {
    run_piped(Spec::Shell(command), env, stop, Some(cancel)).await
}

/// Runs an argv in `dir`, like [`run_local_command_env`].
pub async fn run_argv(argv: &[String], dir: &std::path::Path, env: &[(String, String)], stop: &Stop) -> (ExitStatus, Vec<u8>) {
    run_piped(Spec::Argv { argv, dir }, env, stop, None::<std::future::Pending<()>>).await.expect("never cancelled")
}

/// [`run_local_command_cancellable`] for an argv, run in `dir`.
pub async fn run_argv_cancellable(
    argv: &[String],
    dir: &std::path::Path,
    env: &[(String, String)],
    stop: &Stop,
    cancel: impl Future<Output = ()>,
) -> Option<(ExitStatus, Vec<u8>)> {
    run_piped(Spec::Argv { argv, dir }, env, stop, Some(cancel)).await
}

/// What to run: a build file's command, or an argv in a directory.
#[derive(Clone, Copy)]
enum Spec<'a> {
    Shell(&'a str),
    Argv { argv: &'a [String], dir: &'a std::path::Path },
}

/// Runs a command with stdout and stderr on one pipe, its processes as a
/// tree that `stop` stops, and `cancel` kills.
async fn run_piped<F: Future<Output = ()>>(spec: Spec<'_>, env: &[(String, String)], stop: &Stop, cancel: Option<F>) -> Option<(ExitStatus, Vec<u8>)> {
    let Some(_running) = stop.enter() else { return Some(stopped()) };
    let direct = match spec {
        Spec::Shell(command) => cfg!(not(windows)) && simple_argv(command).is_some(),
        Spec::Argv { .. } => false,
    };
    let (mut reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return Some((ExitStatus::Failure(1), format!("pipe: {e}\n").into_bytes())),
    };
    // The command's copies of the write end close when it is dropped, at the
    // end of `spawn`, so the reader sees EOF.
    let spawn = |direct: bool| -> std::io::Result<tokio::process::Child> {
        let mut cmd = match spec {
            Spec::Shell(command) => local_command(command, direct, env),
            Spec::Argv { argv, dir } => {
                // Relative programs are relative to `dir`; a bare name that
                // isn't there is looked up in `PATH` (a host step's `cmake`).
                let local = dir.join(&argv[0]);
                let is_path = argv[0].contains('/') || (cfg!(windows) && argv[0].contains('\\'));
                let program = if is_path || local.is_file() { local } else { std::path::PathBuf::from(&argv[0]) };
                let mut c = tokio::process::Command::new(program);
                c.args(&argv[1..]).current_dir(dir).envs(env.iter().map(|(k, v)| (k, v))).kill_on_drop(true);
                c
            }
        };
        ProcessTree::prepare(&mut cmd);
        cmd.stdin(Stdio::null()).stdout(writer.try_clone()?).stderr(writer.try_clone()?).spawn()
    };
    // A program run directly that can't start (missing, not executable)
    // runs through the shell instead, for the shell's message and status.
    let spawned = match spawn(direct) {
        Err(_) if direct => spawn(false),
        r => r,
    };
    drop(writer);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            // As ninja says it, on each system.
            let what = if cfg!(windows) { "CreateProcess failed" } else { "posix_spawn" };
            return Some((ExitStatus::Failure(1), format!("{what}: {e}\n").into_bytes()));
        }
    };
    let mut tree = match ProcessTree::attach(&child) {
        Ok(t) => t,
        Err(e) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Some((ExitStatus::Failure(1), format!("starting the command: {e}\n").into_bytes()));
        }
    };
    let mut read = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let cancelled = async {
        match cancel {
            Some(cancel) => cancel.await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(cancelled);
    let status = tokio::select! {
        st = child.wait() => st,
        _ = &mut cancelled => {
            tree.stop(&mut child, SIGKILL, std::future::pending()).await;
            let _ = read.await;
            return None;
        }
        _ = stop.until(|s| s.signal() != 0) => {
            tree.stop(&mut child, stop.signal(), stop.until(Stop::killing)).await;
            // Not waiting for the output: a process outside the tree (a
            // server it started) may hold the pipe.
            return Some(stopped());
        }
    };
    // As in ninja, the command is done once its output ends: when the
    // processes it left holding the pipe (one in the background) exit too.
    // Meanwhile they are still in its tree, which a stop stops.
    let output = tokio::select! {
        out = &mut read => out.unwrap_or_default(),
        _ = &mut cancelled => {
            tree.stop(&mut child, SIGKILL, std::future::pending()).await;
            return None;
        }
        _ = stop.until(|s| s.signal() != 0) => {
            tree.stop(&mut child, stop.signal(), stop.until(Stop::killing)).await;
            return Some(stopped());
        }
    };
    tree.finished();
    Some(match status {
        Ok(st) => (exit_status(st), output),
        Err(e) => (ExitStatus::Failure(1), format!("waiting for command: {e}\n").into_bytes()),
    })
}

/// How long processes a stopped command leaves behind get to act on its
/// signal before they are killed.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long killed processes get to be gone (Windows terminates a job's
/// processes asynchronously).
const KILL_WAIT: Duration = Duration::from_secs(5);

/// Every process a command starts, so that all can be stopped: its own
/// process group on POSIX, a job object on Windows. Dropped before the
/// command finished (its task aborted), it kills them too.
struct ProcessTree {
    #[cfg(unix)]
    group: Option<i32>,
    #[cfg(windows)]
    job: Option<job::Job>,
    /// The command's process group (its id), which Ctrl-Break reaches.
    #[cfg(windows)]
    group: Option<u32>,
}

impl ProcessTree {
    fn prepare(cmd: &mut tokio::process::Command) {
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(job::CREATION_FLAGS);
        #[cfg(not(any(unix, windows)))]
        let _ = cmd;
    }

    /// Once `child` is spawned. On Windows, where it starts suspended, it
    /// joins its job before it runs, so that no process it starts escapes.
    fn attach(child: &tokio::process::Child) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self { group: child.id().and_then(|p| i32::try_from(p).ok()) })
        }
        #[cfg(windows)]
        {
            let process = child.raw_handle().ok_or_else(|| std::io::Error::other("no process handle"))?;
            Ok(Self { job: Some(job::Job::start(process)?), group: child.id() })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    /// Sends `signal` to every process. On Windows, Ctrl-Break, which ninja
    /// sends, reaches the command's process group, and `SIGKILL` terminates
    /// its job. Returns whether that killed them: `SIGKILL`, or Windows
    /// without a console to send Ctrl-Break through.
    fn signal(&self, signal: i32) -> bool {
        #[cfg(unix)]
        {
            if let Some(g) = self.group {
                // SAFETY: plain system call; the group is the command's own.
                unsafe { libc::killpg(g, signal) };
            }
            signal == SIGKILL
        }
        #[cfg(windows)]
        {
            if signal != SIGKILL
                && let Some(group) = self.group
                && job::ctrl_break(group)
            {
                return false;
            }
            if let Some(job) = &self.job {
                job.terminate();
            }
            true
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = signal;
            true
        }
    }

    /// Whether processes of the tree may still run.
    fn alive(&self) -> bool {
        #[cfg(unix)]
        if let Some(g) = self.group {
            // Orphans become this process's children when it reaps them (PID 1
            // in a container, a subreaper): reaped here, or the group never
            // empties (zombies stay in it).
            if reaps_orphans() {
                let mut status = 0;
                // SAFETY: plain system call; the command's own process was
                // waited for already, the others are orphans.
                while unsafe { libc::waitpid(-g, &mut status, libc::WNOHANG) } > 0 {}
            }
            // SAFETY: plain system call; signal 0 only checks the group.
            return unsafe { libc::killpg(g, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        }
        // One that can't be asked may run: the stop waits for it as long
        // as for one running.
        #[cfg(windows)]
        if let Some(job) = &self.job {
            return job.active_processes().is_none_or(|n| n > 0);
        }
        false
    }

    /// Stops every process with `signal` (on Windows, terminates them) and
    /// waits until they have exited; once `kill` completes, they are killed.
    /// Processes left once the command exited get `STOP_GRACE`.
    async fn stop(&mut self, child: &mut tokio::process::Child, signal: i32, kill: impl Future<Output = ()>) {
        tokio::pin!(kill);
        let mut killed = self.signal(signal);
        if killed {
            let _ = child.start_kill();
        }
        tokio::select! {
            _ = child.wait() => {}
            _ = &mut kill, if !killed => {
                killed = true;
                self.signal(SIGKILL);
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        let mut deadline = Instant::now() + if killed { KILL_WAIT } else { STOP_GRACE };
        while self.alive() {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                _ = &mut kill, if !killed => deadline = Instant::now(),
            }
            if Instant::now() >= deadline {
                if killed {
                    break;
                }
                killed = true;
                self.signal(SIGKILL);
                deadline = Instant::now() + KILL_WAIT;
            }
        }
        self.finished();
    }

    /// The command ended: nothing left to stop (its group id may be reused).
    /// Processes it left running are reaped as they exit, when this process
    /// reaps orphans (see `reap_orphans`).
    fn finished(&mut self) {
        #[cfg(unix)]
        {
            reap_orphans(self.group.take());
        }
        #[cfg(windows)]
        {
            self.job = None;
            self.group = None;
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.signal(SIGKILL);
    }
}

/// The process groups of commands that ended leaving processes running,
/// which become this process's children once orphaned (see
/// `reaps_orphans`).
#[cfg(unix)]
static ORPHAN_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// As a reaper of orphans, reaps those of the commands that ended (and of
/// `ended`, a command's group whose own process was waited for) as they
/// exit, whenever a command ends: else they stay zombies for the rest of
/// the build. A group is forgotten once it has no children left.
#[cfg(unix)]
fn reap_orphans(ended: Option<i32>) {
    if !reaps_orphans() {
        return;
    }
    let mut groups = ORPHAN_GROUPS.lock().unwrap_or_else(PoisonError::into_inner);
    groups.extend(ended);
    groups.retain(|&g| {
        let mut status = 0;
        loop {
            // SAFETY: plain system call; the group's own process was waited
            // for already, the others are orphans. While one is left (even a
            // zombie), the group's id isn't reused.
            match unsafe { libc::waitpid(-g, &mut status, libc::WNOHANG) } {
                n if n > 0 => continue,
                0 => return true,
                _ => return false,
            }
        }
    });
}

/// Whether orphaned processes become this process's children: as PID 1 (a
/// container's init) or a subreaper (Linux).
#[cfg(unix)]
fn reaps_orphans() -> bool {
    if std::process::id() == 1 {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        let mut set: libc::c_int = 0;
        // SAFETY: plain system call writing an int.
        if unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut set as *mut libc::c_int) } == 0 && set != 0 {
            return true;
        }
    }
    false
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED};

    /// A command joins its job before it runs, and Ctrl-C reaches ninja-rs
    /// only, as in ninja.
    pub const CREATION_FLAGS: u32 = CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP;

    /// Sends Ctrl-Break to the process group `group`, as ninja does to stop
    /// a command. False without a console to send it through.
    pub fn ctrl_break(group: u32) -> bool {
        // SAFETY: plain system call.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) != 0 }
    }

    /// A job object holding a command's processes.
    pub struct Job(OwnedHandle);

    impl Job {
        /// Puts `process`, created suspended, in a new job, then resumes
        /// it. One that can't join a job isn't resumed: stopping it
        /// couldn't reach what it starts, nor tell when they're gone.
        pub fn start(process: RawHandle) -> std::io::Result<Job> {
            let job = Self::with_process(process).map_err(|e| std::io::Error::new(e.kind(), format!("putting the command in a job: {e}")))?;
            // SAFETY: a valid process handle, with all access rights.
            unsafe { ninja_os::process::resume(process) }?;
            Ok(job)
        }

        fn with_process(process: RawHandle) -> std::io::Result<Job> {
            // SAFETY: a new anonymous job, owned from then on, and a valid
            // process handle.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = OwnedHandle::from_raw_handle(job);
                // A process asking to leave the job (a server, as with
                // setsid on POSIX) may: its creation would fail otherwise,
                // where it works under ninja, which has no jobs.
                let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                    BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION { LimitFlags: JOB_OBJECT_LIMIT_BREAKAWAY_OK, ..Default::default() },
                    ..Default::default()
                };
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if AssignProcessToJobObject(job.as_raw_handle(), process) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(Job(job))
            }
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is open.
            unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
        }

        /// How many of its processes run, if it can be asked.
        pub fn active_processes(&self) -> Option<u32> {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: the handle is open; the buffer has the class's size.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            (ok != 0).then_some(info.ActiveProcesses)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;

        #[test]
        fn suspended_command_joins_a_job_that_allows_breakaway() {
            let mut child = std::process::Command::new("cmd").args(["/c", "exit 3"]).creation_flags(CREATION_FLAGS).spawn().unwrap();
            let job = Job::start(child.as_raw_handle()).unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(3));
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // SAFETY: the handle is open; the buffer has the class's size.
            let ok = unsafe {
                QueryInformationJobObject(
                    job.0.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw mut limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
            assert_eq!(limits.BasicLimitInformation.LimitFlags, JOB_OBJECT_LIMIT_BREAKAWAY_OK);
        }
    }
}

fn exit_status(st: std::process::ExitStatus) -> ExitStatus {
    if st.success() {
        return ExitStatus::Success;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            // SIGINT, SIGTERM, SIGHUP
            if sig == 2 || sig == 15 || sig == 1 {
                return ExitStatus::Interrupted;
            }
            return ExitStatus::Failure(128 + sig);
        }
    }
    #[cfg(windows)]
    {
        const CONTROL_C_EXIT: i32 = 0xC000013Au32 as i32;
        if st.code() == Some(CONTROL_C_EXIT) {
            return ExitStatus::Interrupted;
        }
    }
    ExitStatus::Failure(st.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_program() {
        assert_eq!(split_windows_program("cmd.exe /C \"a && b\""), ("cmd.exe".to_string(), "/C \"a && b\""));
        assert_eq!(split_windows_program("\"C:\\Program Files\\x.exe\" -a"), ("C:\\Program Files\\x.exe".to_string(), "-a"));
        assert_eq!(split_windows_program("tool"), ("tool".to_string(), ""));
    }

    #[tokio::test]
    async fn runs_and_captures_output() {
        let cmd = if cfg!(windows) { "cmd /c echo hello" } else { "echo hello" };
        let (st, out) = run_local_command(cmd, false).await;
        assert_eq!(st, ExitStatus::Success);
        assert_eq!(String::from_utf8_lossy(&out).trim(), "hello");
    }

    #[tokio::test]
    async fn failure_exit_code() {
        let cmd = if cfg!(windows) { "cmd /c exit 3" } else { "exit 3" };
        let (st, _) = run_local_command(cmd, false).await;
        assert_eq!(st, ExitStatus::Failure(3));
    }

    /// A program that can't start fails its command, said as ninja says it
    /// on that system.
    #[tokio::test]
    async fn program_that_cannot_start() {
        let argv = vec!["ninja-rs-no-such-program".to_string()];
        let (st, out) = run_argv(&argv, &std::env::current_dir().unwrap(), &[], &Stop::default()).await;
        let what = if cfg!(windows) { "CreateProcess failed: " } else { "posix_spawn: " };
        assert_eq!(st, ExitStatus::Failure(1));
        assert!(String::from_utf8_lossy(&out).starts_with(what), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn simple_commands() {
        assert_eq!(simple_argv("clang++ -c a.cpp -o obj/a.o -DX=1 -I../inc"), Some(vec!["clang++", "-c", "a.cpp", "-o", "obj/a.o", "-DX=1", "-I../inc"]));
        assert_eq!(simple_argv("  ./tool\targ  "), Some(vec!["./tool", "arg"]));
        // Shell syntax.
        for c in ["a && b", "a | b", "a > out", "a $X", "a 'q'", "a \"q\"", "a *.o", "a ~/x", "a # c", "a\nb", "a;b", "a `b`", "a {x,y}", "a\\ b"] {
            assert_eq!(simple_argv(c), None, "{c}");
        }
        // Assignments, builtins, keywords, nothing.
        for c in ["X=1 a", "cd dir", "exit 3", "pwd", "time a", "if", ":", ". env", "", "  "] {
            assert_eq!(simple_argv(c), None, "{c}");
        }
        // Not ASCII: left to the shell.
        assert_eq!(simple_argv("cc é.c"), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_commands_fall_back_to_the_shell() {
        // Runs directly.
        let (st, out) = run_local_command("printf %s,%s a b", false).await;
        assert_eq!((st, String::from_utf8_lossy(&out).into_owned()), (ExitStatus::Success, "a,b".to_string()));
        // A missing program: the shell's status and message.
        let (st, out) = run_local_command("ninja-rs-no-such-program x", false).await;
        assert_eq!(st, ExitStatus::Failure(127));
        assert!(String::from_utf8_lossy(&out).contains("not found"), "{}", String::from_utf8_lossy(&out));
    }

    /// How long tests wait for what a healthy run does at once: only a
    /// broken one gets there (the machine may be busy).
    const PATIENCE: Duration = Duration::from_secs(60);

    /// Waits until `path` exists.
    async fn until_exists(path: &std::path::Path) {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The process id a `sleeper` (or a script) wrote to `path`.
    fn read_pid(path: &std::path::Path) -> u32 {
        std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }

    /// Whether process `pid` still runs (a zombie doesn't).
    fn alive(pid: u32) -> bool {
        ninja_os::process_running(pid) == Some(true)
    }

    /// Whether process `pid` is gone once a stop returned: at once on POSIX
    /// (the stop waits until its process group is empty); on Windows, a job
    /// counts a terminated process out a moment before it is gone (it runs
    /// no more code then).
    async fn gone(pid: u32) -> bool {
        if cfg!(windows) {
            let _ = tokio::time::timeout(PATIENCE, async {
                while alive(pid) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
        }
        !alive(pid)
    }

    /// The state letter of process `pid` (`R`, `S`, `Z`...), where `/proc`
    /// tells; None once it's gone (or without `/proc`).
    #[cfg(unix)]
    fn process_state(pid: u32) -> Option<char> {
        // "pid (name) S ...": the state follows the name.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.trim_start().chars().next()
    }

    /// The environment of a command whose process runs the `sleeper`, which
    /// writes its process id to `pid_file`. On Windows, `on_break` says what
    /// it does on Ctrl-Break: note it (`PID.got`) and `end` as by default,
    /// or note it and `ignore` it.
    fn sleeper_env(pid_file: &std::path::Path, on_break: Option<&str>) -> Vec<(String, String)> {
        let mut env = vec![("NINJA_RS_TEST_SLEEPER".to_string(), pid_file.to_string_lossy().into_owned())];
        env.extend(on_break.map(|b| ("NINJA_RS_TEST_ON_BREAK".to_string(), b.to_string())));
        env
    }

    /// The test binary running the ignored test `name`.
    fn helper_args(name: &str) -> String {
        format!("\"{}\" --exact {name} --ignored --nocapture --test-threads=1", std::env::current_exe().unwrap().display())
    }

    /// A command whose process's child is the `sleeper` (see `sleeper_env`):
    /// stopping only the command's own process doesn't stop it.
    fn sleeper_command() -> String {
        let sleeper = helper_args("subprocess::tests::sleeper");
        if cfg!(windows) { format!("cmd /c \"{sleeper}\"") } else { format!("sh -c '{sleeper}; true'") }
    }

    /// Run by other tests as a process of a command (see `sleeper_env`):
    /// writes its process id, then sleeps for longer than the tests wait,
    /// then writes `late` next to it, which a stop must keep it from doing.
    /// With `NINJA_RS_TEST_LEAVE=PID_FILE`, it first starts another sleeper
    /// writing there, which ignores Ctrl-Break.
    #[test]
    #[ignore = "run by other tests"]
    fn sleeper() {
        let Some(pid_file) = std::env::var_os("NINJA_RS_TEST_SLEEPER").map(std::path::PathBuf::from) else { return };
        if let Some(left) = std::env::var_os("NINJA_RS_TEST_LEAVE") {
            // Left behind, on purpose.
            #[allow(clippy::zombie_processes)]
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "subprocess::tests::sleeper", "--ignored", "--nocapture", "--test-threads=1"])
                .env("NINJA_RS_TEST_SLEEPER", left)
                .env("NINJA_RS_TEST_ON_BREAK", "ignore")
                .env_remove("NINJA_RS_TEST_LEAVE")
                .spawn()
                .unwrap();
        }
        #[cfg(windows)]
        if let Ok(on_break) = std::env::var("NINJA_RS_TEST_ON_BREAK") {
            use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
            static GOT: std::sync::OnceLock<(std::path::PathBuf, bool)> = std::sync::OnceLock::new();
            /// Notes Ctrl-Break (`PID.got`); handled (ignored), or passed on
            /// to the default handler, which ends the process.
            unsafe extern "system" fn note(_: u32) -> windows_sys::core::BOOL {
                let Some((got, ignore)) = GOT.get() else { return 0 };
                let _ = std::fs::write(got, "");
                (*ignore).into()
            }
            GOT.set((pid_file.with_extension("got"), on_break == "ignore")).unwrap();
            // SAFETY: a handler that only writes a file.
            assert_ne!(unsafe { SetConsoleCtrlHandler(Some(note), 1) }, 0);
        }
        let tmp = pid_file.with_extension("tmp");
        std::fs::write(&tmp, std::process::id().to_string()).unwrap();
        std::fs::rename(&tmp, &pid_file).unwrap();
        std::thread::sleep(2 * PATIENCE);
        std::fs::write(pid_file.with_extension("late"), "late").unwrap();
    }

    #[tokio::test]
    async fn cancelling_stops_every_process() {
        let dir = test_dirs::TestDir::new("ninja-rs-subprocess-cancel");
        let pid_file = dir.join("sleeper.pid");
        let stop = Stop::default();
        // Cancelled once the sleeper runs: it is killed, and waited for.
        let (cmd, env) = (sleeper_command(), sleeper_env(&pid_file, None));
        let ran = run_local_command_cancellable(&cmd, &env, &stop, until_exists(&pid_file));
        let ran = tokio::time::timeout(PATIENCE, ran).await.expect("cancelling waited for the command to end");
        assert!(ran.is_none());
        assert!(gone(read_pid(&pid_file)).await, "a process of the command kept running");
        // Not cancelled: the command's result.
        let ok = if cfg!(windows) { "cmd /c echo done" } else { "echo done" };
        let (st, out) = run_local_command_cancellable(ok, &[], &stop, std::future::pending()).await.unwrap();
        assert_eq!((st, String::from_utf8_lossy(&out).trim().to_string()), (ExitStatus::Success, "done".to_string()));
    }

    #[tokio::test]
    async fn stopping_waits_for_every_process() {
        let dir = test_dirs::TestDir::new("ninja-rs-subprocess-stop");
        let pid_file = dir.join("sleeper.pid");
        let stop = Stop::new();
        let s = stop.clone();
        let env = sleeper_env(&pid_file, None);
        let ran = tokio::spawn(async move { run_local_command_env(&sleeper_command(), false, &env, &s).await });
        until_exists(&pid_file).await;
        tokio::time::timeout(PATIENCE, stop.stop(None)).await.expect("the stop waited for the command to end by itself");
        // Gone once `stop` returns.
        assert!(ran.is_finished());
        assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);
        assert!(gone(read_pid(&pid_file)).await, "a process of the command kept running");
        // Nothing starts any more.
        let ok = if cfg!(windows) { "cmd /c echo done" } else { "echo done" };
        assert_eq!(run_local_command_env(ok, false, &[], &stop).await, stopped());
    }

    /// A command going on after the signal is waited for, until another
    /// interrupt kills it.
    #[cfg(unix)]
    #[tokio::test]
    async fn another_interrupt_kills() {
        let dir = test_dirs::TestDir::new("ninja-rs-subprocess-kill");
        let (pid_file, got) = (dir.join("shell.pid"), dir.join("got"));
        let stop = Stop::new();
        let s = stop.clone();
        let cmd = format!(
            "trap 'echo > {got}' INT TERM HUP; echo $$ > {pid}.tmp; mv {pid}.tmp {pid}; while :; do sleep 0.1; done",
            got = got.display(),
            pid = pid_file.display()
        );
        let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
        until_exists(&pid_file).await;
        let interrupt = Interrupt::new();
        interrupt.trigger();
        let (s, i) = (stop.clone(), interrupt.clone());
        let stopping = tokio::spawn(async move { s.stop(Some(&i)).await });
        // It got the signal, and goes on: the stop waits for it.
        until_exists(&got).await;
        assert!(!stopping.is_finished(), "the stop didn't wait for the command");
        interrupt.trigger();
        tokio::time::timeout(PATIENCE, stopping).await.expect("another interrupt didn't kill the command").unwrap();
        assert!(ran.is_finished());
        assert!(gone(read_pid(&pid_file)).await, "the command kept running");
    }

    /// An interrupt while something acts on interrupts is its own; one
    /// while nothing does is unhandled: the invocation ends at once.
    #[tokio::test]
    async fn interrupts_nothing_handles() {
        let interrupt = Interrupt::new();
        let pending = |i: &Arc<Interrupt>| {
            let i = i.clone();
            async move {
                tokio::select! {
                    biased;
                    _ = i.unhandled() => false,
                    _ = std::future::ready(()) => true,
                }
            }
        };
        let handling = interrupt.handle();
        interrupt.trigger();
        assert!(pending(&interrupt).await);
        drop(handling);
        assert!(pending(&interrupt).await, "an interrupt handled once stays handled");
        interrupt.trigger();
        assert_eq!(interrupt.unhandled().await, SIGINT);
        assert_eq!((interrupt.count(), interrupt.last_signal()), (2, SIGINT));
        // Not a signal: no sender.
        assert_eq!(interrupt.last_sent_by_process(), None);
    }

    /// A command leaving a process in the background that holds its output
    /// is done when that process is, as in ninja; a stop meanwhile reaches it
    /// too.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_reaches_processes_holding_the_output() {
        let dir = test_dirs::TestDir::new("ninja-rs-subprocess-holder");
        let (pid_file, started) = (dir.join("shell.pid"), dir.join("started"));
        let stop = Stop::new();
        let s = stop.clone();
        let cmd = format!("echo $$ > {pid}.tmp; mv {pid}.tmp {pid}; sleep 30 & echo > {}", started.display(), pid = pid_file.display());
        let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
        until_exists(&started).await;
        // The shell exits; `sleep` holds the output.
        let shell = read_pid(&pid_file);
        while alive(shell) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!ran.is_finished());
        tokio::time::timeout(PATIENCE, stop.stop(None)).await.expect("the stop didn't reach the background process");
        assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);
    }

    /// On Windows, as ninja does, a stop sends Ctrl-Break to the command's
    /// process group first and waits for it to act on it; a command going on
    /// is waited for until another interrupt, which terminates its job; what
    /// a command leaves behind gets the grace, then is killed. Run with a
    /// console of its own, which Ctrl-Break needs (the tests may have none):
    /// see `ctrl_break_helper`.
    #[cfg(windows)]
    #[test]
    fn stopping_sends_ctrl_break_then_escalates() {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        let dir = test_dirs::TestDir::new("ninja-rs-subprocess-ctrl-break");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess::tests::ctrl_break_helper", "--ignored", "--nocapture", "--test-threads=1"])
            .env("NINJA_RS_TEST_DIR", dir.path())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "run by stopping_sends_ctrl_break_then_escalates"]
    fn ctrl_break_helper() {
        use windows_sys::Win32::System::Console::GetConsoleProcessList;
        let Some(dir) = std::env::var_os("NINJA_RS_TEST_DIR").map(std::path::PathBuf::from) else { return };
        let mut list = [0u32; 4];
        // SAFETY: a buffer of the size given.
        assert_ne!(unsafe { GetConsoleProcessList(list.as_mut_ptr(), 4) }, 0, "no console");
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // Ctrl-Break first, which the command's processes get (the
            // sleeper notes it, then ends as by default).
            let pid_file = dir.join("break.pid");
            let stop = Stop::new();
            let s = stop.clone();
            let env = sleeper_env(&pid_file, Some("end"));
            let ran = tokio::spawn(async move { run_local_command_env(&sleeper_command(), false, &env, &s).await });
            until_exists(&pid_file).await;
            tokio::time::timeout(PATIENCE, stop.stop(None)).await.expect("the stop didn't end the command");
            // The sleeper noted Ctrl-Break before it ended, which the stop
            // waited for. (The command's task may end a moment after: on
            // this runtime, another thread completes it.)
            assert!(pid_file.with_extension("got").exists(), "the command didn't get Ctrl-Break");
            assert!(gone(read_pid(&pid_file)).await, "the sleeper kept running");
            assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);

            // The command's own process going on after Ctrl-Break (`cmd`
            // waiting for the sleeper, which ignores it).
            let pid_file = dir.join("own.pid");
            let stop = Stop::new();
            let s = stop.clone();
            let env = sleeper_env(&pid_file, Some("ignore"));
            let ran = tokio::spawn(async move { run_local_command_env(&sleeper_command(), false, &env, &s).await });
            until_exists(&pid_file).await;
            let interrupt = Interrupt::new();
            interrupt.trigger();
            let (s, i) = (stop.clone(), interrupt.clone());
            let stopping = tokio::spawn(async move { s.stop(Some(&i)).await });
            until_exists(&pid_file.with_extension("got")).await;
            assert!(!stopping.is_finished(), "the stop didn't wait for the command");
            interrupt.trigger();
            tokio::time::timeout(PATIENCE, stopping).await.expect("another interrupt didn't end the command").unwrap();
            assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);
            assert!(gone(read_pid(&pid_file)).await, "the command kept running");

            // A process the command leaves behind going on after Ctrl-Break,
            // while the command ends on it: the stop kills it after the
            // grace, without another interrupt.
            let (pid_file, left) = (dir.join("leader.pid"), dir.join("left.pid"));
            let stop = Stop::new();
            let s = stop.clone();
            let mut env = sleeper_env(&pid_file, Some("end"));
            env.push(("NINJA_RS_TEST_LEAVE".into(), left.to_string_lossy().into_owned()));
            let ran = tokio::spawn(async move { run_local_command_env(&sleeper_command(), false, &env, &s).await });
            until_exists(&pid_file).await;
            until_exists(&left).await;
            tokio::time::timeout(PATIENCE, stop.stop(None)).await.expect("the stop didn't kill what the command left");
            assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);
            assert!(pid_file.with_extension("got").exists() && left.with_extension("got").exists(), "Ctrl-Break wasn't sent first");
            assert!(gone(read_pid(&left)).await, "the process the command left kept running");

            // One in a process group of its own (`start /b`), which Ctrl-Break
            // to the command's doesn't reach: killed with the command's job.
            let pid_file = dir.join("own-group.pid");
            let stop = Stop::new();
            let s = stop.clone();
            let env = sleeper_env(&pid_file, Some("ignore"));
            let cmd = format!("cmd /c start /b \"\" {}", helper_args("subprocess::tests::sleeper"));
            let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &env, &s).await });
            until_exists(&pid_file).await;
            tokio::time::timeout(PATIENCE, stop.stop(None)).await.expect("the stop didn't kill what the command left");
            assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);
            assert!(gone(read_pid(&pid_file)).await, "the process the command left kept running");
        });
    }

    /// As a reaper of orphans (PID 1 in a container, a subreaper), a stop
    /// reaps the processes a command left, escalates to a kill after the
    /// grace, and leaves no zombie. Run in a process of its own (the setting
    /// is the process's): see `reaper_helper`.
    #[cfg(target_os = "linux")]
    #[test]
    fn stopping_as_a_reaper_of_orphans() {
        let dir = test_dirs::TestDir::new("ninja-rs-subprocess-reaper");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess::tests::reaper_helper", "--ignored", "--nocapture", "--test-threads=1"])
            .env("NINJA_RS_TEST_REAPER", dir.path())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "run by stopping_as_a_reaper_of_orphans"]
    fn reaper_helper() {
        let Some(dir) = std::env::var_os("NINJA_RS_TEST_REAPER").map(std::path::PathBuf::from) else { return };
        // SAFETY: plain system call.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let pid_file = dir.join("ignoring.pid");
        // A process ignoring the signal, orphaned when the shell dies.
        let cmd = format!(
            "(trap '' TERM; sh -c 'echo $PPID > {pid}.tmp'; mv {pid}.tmp {pid}; sleep 100; echo late > {pid}.late) & sleep 100",
            pid = pid_file.display()
        );
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let stop = Stop::new();
            let s = stop.clone();
            let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
            until_exists(&pid_file).await;
            tokio::time::timeout(PATIENCE, stop.stop(None)).await.expect("the stop didn't kill the process ignoring the signal");
            assert!(ran.await.is_ok());
        });
        let mut status = 0;
        // SAFETY: plain system call.
        let zombie = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        assert!(zombie <= 0, "a zombie was left: {zombie}");
        assert!(!alive(read_pid(&pid_file)), "the process ignoring the signal wasn't killed");
    }

    /// As a reaper of orphans, the processes commands leave behind are
    /// reaped as they exit (whenever a command ends), not only by a stop:
    /// else they stay zombies for the rest of the build. Run in a process of
    /// its own: see `orphan_helper`.
    #[cfg(target_os = "linux")]
    #[test]
    fn orphans_are_reaped_as_commands_end() {
        let dir = test_dirs::TestDir::new("ninja-rs-subprocess-orphans");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess::tests::orphan_helper", "--ignored", "--nocapture", "--test-threads=1"])
            .env("NINJA_RS_TEST_REAPER", dir.path())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "run by orphans_are_reaped_as_commands_end"]
    fn orphan_helper() {
        let Some(dir) = std::env::var_os("NINJA_RS_TEST_REAPER").map(std::path::PathBuf::from) else { return };
        // SAFETY: plain system call.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let pid_file = dir.join("orphan.pid");
        // Done at once: what it leaves running doesn't hold its output.
        let cmd = format!("sh -c 'echo $$ > {pid}.tmp; mv {pid}.tmp {pid}; exec sleep 0.2' </dev/null >/dev/null 2>&1 &", pid = pid_file.display());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            assert_eq!(run_local_command_env(&cmd, false, &[], &Stop::default()).await.0, ExitStatus::Success);
            until_exists(&pid_file).await;
            let orphan = read_pid(&pid_file);
            // It exits: a zombie, until reaped.
            while process_state(orphan).is_some_and(|s| s != 'Z') {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Another command ends.
            assert_eq!(run_local_command_env("true", false, &[], &Stop::default()).await.0, ExitStatus::Success);
            assert_eq!(process_state(orphan), None, "the orphan was left a zombie");
        });
    }

    #[tokio::test]
    async fn stderr_is_captured() {
        let cmd = if cfg!(windows) { "cmd /c echo oops 1>&2" } else { "echo oops 1>&2" };
        let (_, out) = run_local_command(cmd, false).await;
        assert_eq!(String::from_utf8_lossy(&out).trim(), "oops");
    }
}
