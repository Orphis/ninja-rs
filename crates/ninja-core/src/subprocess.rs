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
//! ninja-rs only, and stopping a command (an interrupted or failed build, a
//! raced command losing) reaches every process it started: the interrupt's
//! signal first (Ctrl-Break on Windows, as ninja sends), then, after a
//! grace, a kill. `console` commands stay in the foreground and get the
//! terminal's signals directly.

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
/// one ends the invocation at once (see `unhandled`), as ninja dies of the
/// signal there.
#[derive(Default)]
pub struct Interrupt {
    /// How many interrupts arrived.
    count: AtomicUsize,
    /// The first interrupt's signal, which commands get too.
    signal: AtomicI32,
    /// The last interrupt's signal.
    last: AtomicI32,
    /// How many holders of `handle` act on interrupts, and the signal of an
    /// interrupt that came while there was none.
    handling: Mutex<(usize, Option<i32>)>,
    notify: Notify,
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
        self.trigger_with(SIGINT);
    }

    fn trigger_with(&self, signal: i32) {
        let _ = self.signal.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
        self.last.store(signal, Ordering::SeqCst);
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

    /// Until the guard returned is dropped, its holder acts on interrupts
    /// (a build stopping its commands, `-t exec` passing them on): they
    /// don't end the invocation at once (see `unhandled`).
    pub fn handle(self: &Arc<Self>) -> Handling {
        self.handling.lock().unwrap_or_else(PoisonError::into_inner).0 += 1;
        Handling(self.clone())
    }

    /// Completes once an interrupt came while nothing handled it, with its
    /// signal: the invocation ends at once.
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
    /// Ctrl-Break as Ctrl-C does.
    pub fn install_handlers(self: &Arc<Self>) {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            for (kind, number) in [(SignalKind::interrupt(), SIGINT), (SignalKind::terminate(), SIGTERM), (SignalKind::hangup(), libc::SIGHUP)] {
                let Ok(mut events) = signal(kind) else { continue };
                let me = self.clone();
                tokio::spawn(async move {
                    while events.recv().await.is_some() {
                        me.trigger_with(number);
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
    /// without one), console ones only the terminal's (as in ninja); on
    /// Windows their jobs are terminated. Another interrupt meanwhile kills
    /// what still runs.
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
            return Some((ExitStatus::Failure(1), format!("CreateProcess failed: {e}\n").into_bytes()));
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
            Ok(Self { job: job::Job::start(process)?, group: child.id() })
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
        #[cfg(windows)]
        if let Some(job) = &self.job {
            return job.active_processes() > 0;
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
    fn finished(&mut self) {
        #[cfg(unix)]
        {
            self.group = None;
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
    use std::ffi::c_void;

    type Handle = *mut c_void;
    /// `JobObjectBasicAccountingInformation`.
    const BASIC_ACCOUNTING: i32 = 1;
    /// `JobObjectExtendedLimitInformation`.
    const EXTENDED_LIMITS: i32 = 9;
    /// `JOB_OBJECT_LIMIT_BREAKAWAY_OK`.
    const BREAKAWAY_OK: u32 = 0x800;
    /// `CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP`: a command joins its
    /// job before it runs, and Ctrl-C reaches ninja-rs only, as in ninja.
    pub const CREATION_FLAGS: u32 = 0x4 | 0x200;

    #[repr(C)]
    #[derive(Default)]
    struct BasicAccounting {
        total_user_time: i64,
        total_kernel_time: i64,
        this_period_total_user_time: i64,
        this_period_total_kernel_time: i64,
        total_page_fault_count: u32,
        total_processes: u32,
        active_processes: u32,
        total_terminated_processes: u32,
    }

    /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
        io_counters: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, length: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn QueryInformationJobObject(job: Handle, class: i32, info: *mut c_void, length: u32, returned: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    // std keeps the handle of a new process's thread to itself: a process
    // spawned suspended is resumed whole, with this ntdll export (long
    // stable, if undocumented; PsSuspend and debuggers use it).
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process: Handle) -> i32;
    }

    /// `CTRL_BREAK_EVENT`.
    const CTRL_BREAK: u32 = 1;

    /// Sends Ctrl-Break to the process group `group`, as ninja does to stop
    /// a command. False without a console to send it through.
    pub fn ctrl_break(group: u32) -> bool {
        // SAFETY: plain system call.
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK, group) != 0 }
    }

    /// A job object holding a command's processes.
    pub struct Job(Handle);

    // SAFETY: job handles may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// Puts `process`, created suspended, in a new job (when it can),
        /// then resumes it.
        pub fn start(process: std::os::windows::io::RawHandle) -> std::io::Result<Option<Job>> {
            let job = Self::with_process(process);
            // SAFETY: a valid process handle, with all access rights.
            let status = unsafe { NtResumeProcess(process as Handle) };
            if status < 0 {
                return Err(std::io::Error::other(format!("resuming the process: NTSTATUS {status:#x}")));
            }
            Ok(job)
        }

        fn with_process(process: std::os::windows::io::RawHandle) -> Option<Job> {
            // SAFETY: a new anonymous job, and a valid process handle.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                // A process asking to leave the job (a server, as with
                // setsid on POSIX) may: its creation would fail otherwise,
                // where it works under ninja, which has no jobs.
                let limits = ExtendedLimits { limit_flags: BREAKAWAY_OK, ..Default::default() };
                SetInformationJobObject(job, EXTENDED_LIMITS, (&limits as *const ExtendedLimits).cast(), std::mem::size_of::<ExtendedLimits>() as u32);
                if AssignProcessToJobObject(job, process as Handle) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is open.
            unsafe { TerminateJobObject(self.0, 1) };
        }

        pub fn active_processes(&self) -> u32 {
            let mut info = BasicAccounting::default();
            // SAFETY: the handle is open; the buffer has the class's size.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    BASIC_ACCOUNTING,
                    (&mut info as *mut BasicAccounting).cast(),
                    std::mem::size_of::<BasicAccounting>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 { 0 } else { info.active_processes }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle is open and owned.
            unsafe { CloseHandle(self.0) };
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
            let job = Job::start(child.as_raw_handle()).unwrap().expect("a job");
            assert_eq!(child.wait().unwrap().code(), Some(3));
            let mut limits = ExtendedLimits::default();
            // SAFETY: the handle is open; the buffer has the class's size.
            let ok = unsafe {
                QueryInformationJobObject(
                    job.0,
                    EXTENDED_LIMITS,
                    (&mut limits as *mut ExtendedLimits).cast(),
                    std::mem::size_of::<ExtendedLimits>() as u32,
                    std::ptr::null_mut(),
                )
            };
            assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
            assert_eq!(limits.limit_flags, BREAKAWAY_OK);
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

    #[tokio::test]
    async fn cancelling_stops_every_process() {
        // A shell starting a child that would write a file later.
        let dir = test_dirs::base().join(format!("ninja-rs-test-{}-cancel", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("late.txt");
        let cmd = if cfg!(windows) {
            format!("cmd /c \"ping -n 3 127.0.0.1 >nul & echo late> {}\"", marker.display())
        } else {
            format!("sh -c 'sleep 2; echo late > {}'", marker.display())
        };
        let cancel = tokio::time::sleep(std::time::Duration::from_millis(300));
        let start = std::time::Instant::now();
        let stop = Stop::default();
        assert!(run_local_command_cancellable(&cmd, &[], &stop, cancel).await.is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a process of the command kept running");
        // Not cancelled: the command's result.
        let ok = if cfg!(windows) { "cmd /c echo done" } else { "echo done" };
        let (st, out) = run_local_command_cancellable(ok, &[], &stop, std::future::pending()).await.unwrap();
        assert_eq!((st, String::from_utf8_lossy(&out).trim().to_string()), (ExitStatus::Success, "done".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = test_dirs::base().join(format!("ninja-rs-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A name `waitfor` can wait for (on Windows), its own: only one waits
    /// for a name at a time.
    fn waitfor_name() -> String {
        static N: AtomicUsize = AtomicUsize::new(0);
        format!("ninjarstest{}x{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    /// A command whose grandchild writes `marker` after about 2 s: stopping
    /// only the command's own process doesn't stop it. (On Windows, `waitfor`
    /// stops on Ctrl-Break, and the marker is written only if it timed out.)
    fn late_writer(marker: &std::path::Path) -> String {
        if cfg!(windows) {
            format!("cmd /c cmd /v:on /c \"waitfor /t 2 {} >nul 2>&1 & if !errorlevel! == 1 echo late> {}\"", waitfor_name(), marker.display())
        } else {
            format!("sh -c 'sh -c \"sleep 2; echo late > {}\"; true'", marker.display())
        }
    }

    #[tokio::test]
    async fn stopping_waits_for_every_process() {
        let dir = test_dir("stop");
        let marker = dir.join("late.txt");
        let stop = Stop::new();
        let s = stop.clone();
        let cmd = late_writer(&marker);
        let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let start = Instant::now();
        stop.stop(None).await;
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
        // Gone once `stop` returns.
        assert!(ran.is_finished());
        assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);
        // Nothing starts any more.
        let ok = if cfg!(windows) { "cmd /c echo done" } else { "echo done" };
        assert_eq!(run_local_command_env(ok, false, &[], &stop).await, stopped());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a process of the command kept running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A command ignoring the signal: another interrupt kills it.
    #[cfg(unix)]
    #[tokio::test]
    async fn another_interrupt_kills() {
        let dir = test_dir("stop-kill");
        let marker = dir.join("late.txt");
        let stop = Stop::new();
        let s = stop.clone();
        let cmd = format!("trap '' INT TERM HUP; sleep 3; echo late > {}", marker.display());
        let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let interrupt = Interrupt::new();
        interrupt.trigger();
        let again = interrupt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            again.trigger();
        });
        let start = Instant::now();
        stop.stop(Some(&interrupt)).await;
        let elapsed = start.elapsed();
        assert!(elapsed > Duration::from_millis(400) && elapsed < Duration::from_millis(1500), "{elapsed:?}");
        assert!(ran.is_finished());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a process of the command kept running");
        let _ = std::fs::remove_dir_all(&dir);
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
    }

    /// A command leaving a process in the background that holds its output
    /// is done when that process is, as in ninja; a stop meanwhile reaches it
    /// too.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_reaches_processes_holding_the_output() {
        let dir = test_dir("stop-holder");
        let started = dir.join("started");
        let stop = Stop::new();
        let s = stop.clone();
        let cmd = format!("sleep 30 & echo > {}", started.display());
        let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The shell exits; `sleep` holds the output.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!ran.is_finished());
        tokio::time::timeout(Duration::from_secs(10), stop.stop(None)).await.expect("the stop didn't reach the background process");
        assert_eq!(ran.await.unwrap().0, ExitStatus::Interrupted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On Windows, a stop sends Ctrl-Break to the command's process group
    /// first, as ninja does, and waits for it to act on it.
    #[cfg(windows)]
    #[tokio::test]
    async fn stopping_sends_ctrl_break_first() {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        }
        let mut list = [0u32; 4];
        // SAFETY: a buffer of the size given.
        if unsafe { GetConsoleProcessList(list.as_mut_ptr(), 4) } == 0 {
            println!("not tested: no console");
            return;
        }
        let dir = test_dir("ctrl-break");
        let (started, marker) = (dir.join("started"), dir.join("break"));
        // STATUS_CONTROL_C_EXIT: `waitfor` ended by Ctrl-Break.
        let cmd = format!(
            "cmd /v:on /c \"echo.> {} & waitfor /t 10 {} >nul 2>&1 & if !errorlevel! == -1073741510 echo.> {}\"",
            started.display(),
            waitfor_name(),
            marker.display()
        );
        let stop = Stop::new();
        let s = stop.clone();
        let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.stop(None).await;
        assert!(ran.is_finished());
        assert!(marker.exists(), "the command didn't get Ctrl-Break");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// As a reaper of orphans (PID 1 in a container, a subreaper), a stop
    /// reaps the processes a command left, escalates to a kill after the
    /// grace, and leaves no zombie. Run in a process of its own (the setting
    /// is the process's): see `reaper_helper`.
    #[cfg(target_os = "linux")]
    #[test]
    fn stopping_as_a_reaper_of_orphans() {
        let dir = test_dir("reaper");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess::tests::reaper_helper", "--ignored", "--nocapture", "--test-threads=1"])
            .env("NINJA_RS_TEST_REAPER", &dir)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "run by stopping_as_a_reaper_of_orphans"]
    fn reaper_helper() {
        let Some(dir) = std::env::var_os("NINJA_RS_TEST_REAPER").map(std::path::PathBuf::from) else { return };
        // SAFETY: plain system call.
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
        let marker = dir.join("late.txt");
        // A process ignoring the signal, orphaned when the shell dies.
        let cmd = format!("(trap '' TERM; sleep 4; echo late > {}) & sleep 30", marker.display());
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let stop = Stop::new();
            let s = stop.clone();
            let ran = tokio::spawn(async move { run_local_command_env(&cmd, false, &[], &s).await });
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.stop(None).await;
            assert!(ran.await.is_ok());
        });
        let mut status = 0;
        // SAFETY: plain system call.
        let zombie = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        assert!(zombie <= 0, "a zombie was left: {zombie}");
        std::thread::sleep(Duration::from_secs(5));
        assert!(!marker.exists(), "the process ignoring the signal wasn't killed");
    }

    #[tokio::test]
    async fn stderr_is_captured() {
        let cmd = if cfg!(windows) { "cmd /c echo oops 1>&2" } else { "echo oops 1>&2" };
        let (_, out) = run_local_command(cmd, false).await;
        assert_eq!(String::from_utf8_lossy(&out).trim(), "oops");
    }
}
