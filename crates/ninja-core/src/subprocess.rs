//! Local command execution (ninja's `Subprocess`/`SubprocessSet`).
//!
//! As in ninja, commands run through `/bin/sh -c` on POSIX and are passed
//! to `CreateProcess` verbatim on Windows. On POSIX, a command without
//! shell syntax runs directly instead (see `simple_argv`). stdout and stderr share one pipe
//! so their interleaving is preserved; `console` pool commands inherit the
//! terminal instead.

use std::collections::HashSet;
use std::io::Read;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::graph::EdgeId;
use crate::runner::{CommandRunner, ExecKind, ExitStatus, Task, TaskResult, WaitFuture};

/// Set when the user interrupts the build (Ctrl-C).
#[derive(Default)]
pub struct Interrupt {
    flag: AtomicBool,
    notify: Notify,
}

impl Interrupt {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    pub async fn wait(&self) {
        loop {
            let n = self.notify.notified();
            if self.is_set() {
                return;
            }
            n.await;
        }
    }
    /// Listens for Ctrl-C for the rest of the process lifetime.
    pub fn install_ctrl_c(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                me.trigger();
            }
        });
    }
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
    ".", ":", "alias", "bg", "bind", "break", "builtin", "caller", "case", "cd", "command", "compgen", "complete", "continue",
    "declare", "dirs", "disown", "do", "done", "elif", "else", "enable", "esac", "eval", "exec", "exit", "export", "fc", "fg",
    "fi", "for", "function", "getopts", "hash", "help", "history", "if", "in", "jobs", "let", "local", "logout", "mapfile",
    "popd", "pushd", "pwd", "read", "readarray", "readonly", "return", "select", "set", "shift", "shopt", "source", "suspend",
    "then", "time", "times", "trap", "type", "typeset", "ulimit", "umask", "unalias", "unset", "until", "wait", "while",
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
        let program = if !program.contains(['/', '\\']) && std::path::Path::new(&program).is_file() {
            format!(".\\{program}")
        } else {
            program
        };
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
    run_local_command_env(command, use_console, &[]).await
}

/// Builds the command to run `command` locally, with `env` added.
fn local_command(command: &str, direct: bool, env: &[(String, String)]) -> tokio::process::Command {
    let mut cmd = build_command(command, direct);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.kill_on_drop(true);
    cmd
}

/// [`run_local_command`] with extra environment variables.
pub async fn run_local_command_env(command: &str, use_console: bool, env: &[(String, String)]) -> (ExitStatus, Vec<u8>) {
    if !use_console {
        return run_piped(Spec::Shell(command), env, None::<std::future::Pending<()>>).await.expect("never cancelled");
    }
    let direct = cfg!(not(windows)) && simple_argv(command).is_some();
    // A program run directly that can't start (missing, not executable)
    // runs through the shell instead, for the shell's message and status.
    let spawn = |direct: bool| local_command(command, direct, env).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn();
    let spawned = match spawn(direct) {
        Err(_) if direct => spawn(false),
        r => r,
    };
    match spawned {
        Ok(mut child) => match child.wait().await {
            Ok(st) => (exit_status(st), Vec::new()),
            Err(e) => (ExitStatus::Failure(1), format!("waiting for command: {e}\n").into_bytes()),
        },
        Err(e) => (ExitStatus::Failure(1), format!("{e}\n").into_bytes()),
    }
}

/// Runs one command (not a console one) like [`run_local_command_env`],
/// until `cancel` resolves: then the command and every process it started
/// are stopped, and `None` is returned once they have exited. They run in
/// their own process group on POSIX (Ctrl-C reaches ninja-rs, which stops
/// them) and in a job object on Windows. Racing uses it: a remote result
/// replaces the outputs only once nothing local writes them.
pub async fn run_local_command_cancellable(
    command: &str,
    env: &[(String, String)],
    cancel: impl std::future::Future<Output = ()>,
) -> Option<(ExitStatus, Vec<u8>)> {
    run_piped(Spec::Shell(command), env, Some(cancel)).await
}

/// Runs an argv in `dir`, like [`run_local_command_env`].
pub async fn run_argv(argv: &[String], dir: &std::path::Path, env: &[(String, String)]) -> (ExitStatus, Vec<u8>) {
    run_piped(Spec::Argv { argv, dir }, env, None::<std::future::Pending<()>>).await.expect("never cancelled")
}

/// [`run_local_command_cancellable`] for an argv, run in `dir`.
pub async fn run_argv_cancellable(
    argv: &[String],
    dir: &std::path::Path,
    env: &[(String, String)],
    cancel: impl std::future::Future<Output = ()>,
) -> Option<(ExitStatus, Vec<u8>)> {
    run_piped(Spec::Argv { argv, dir }, env, Some(cancel)).await
}

/// What to run: a build file's command, or an argv in a directory.
#[derive(Clone, Copy)]
enum Spec<'a> {
    Shell(&'a str),
    Argv { argv: &'a [String], dir: &'a std::path::Path },
}

/// Runs a command with stdout and stderr on one pipe. With `cancel`, the
/// command's processes are tracked as a tree that `cancel` stops.
async fn run_piped<F: std::future::Future<Output = ()>>(spec: Spec<'_>, env: &[(String, String)], cancel: Option<F>) -> Option<(ExitStatus, Vec<u8>)> {
    let tree = cancel.is_some();
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
                // Relative programs are relative to `dir`.
                let program = dir.join(&argv[0]);
                let mut c = tokio::process::Command::new(program);
                c.args(&argv[1..]).current_dir(dir).envs(env.iter().map(|(k, v)| (k, v))).kill_on_drop(true);
                c
            }
        };
        if tree {
            ProcessTree::prepare(&mut cmd);
        }
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
    let mut processes = tree.then(|| ProcessTree::attach(&child));
    let read = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let status = match cancel {
        None => child.wait().await,
        Some(cancel) => {
            tokio::select! {
                st = child.wait() => st,
                _ = cancel => {
                    if let Some(p) = &mut processes {
                        p.kill(&mut child).await;
                    }
                    let _ = read.await;
                    return None;
                }
            }
        }
    };
    if let Some(p) = &mut processes {
        p.finished();
    }
    let output = read.await.unwrap_or_default();
    Some(match status {
        Ok(st) => (exit_status(st), output),
        Err(e) => (ExitStatus::Failure(1), format!("waiting for command: {e}\n").into_bytes()),
    })
}

/// Every process a command starts, so that all can be stopped: its own
/// process group on POSIX, a job object on Windows. Dropped before the
/// command finished (a build interrupted), it stops them too.
struct ProcessTree {
    #[cfg(unix)]
    group: Option<i32>,
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl ProcessTree {
    fn prepare(cmd: &mut tokio::process::Command) {
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(not(unix))]
        let _ = cmd;
    }

    fn attach(child: &tokio::process::Child) -> Self {
        #[cfg(unix)]
        {
            Self { group: child.id().and_then(|p| i32::try_from(p).ok()) }
        }
        #[cfg(windows)]
        {
            Self { job: child.raw_handle().and_then(job::Job::with_process) }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Self {}
        }
    }

    fn signal(&self) {
        #[cfg(unix)]
        if let Some(g) = self.group {
            // SAFETY: plain system call; the group is the command's own.
            unsafe { libc::killpg(g, libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
    }

    /// Stops every process and waits until they have exited.
    async fn kill(&mut self, child: &mut tokio::process::Child) {
        self.signal();
        let _ = child.start_kill();
        let _ = child.wait().await;
        #[cfg(windows)]
        if let Some(job) = &self.job {
            // Job processes are terminated asynchronously.
            for _ in 0..500 {
                if job.active_processes() == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.signal();
    }
}

#[cfg(windows)]
mod job {
    use std::ffi::c_void;

    type Handle = *mut c_void;
    /// `JobObjectBasicAccountingInformation`.
    const BASIC_ACCOUNTING: i32 = 1;

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

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
        fn QueryInformationJobObject(job: Handle, class: i32, info: *mut c_void, length: u32, returned: *mut u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    /// A job object holding a command's processes.
    pub struct Job(Handle);

    // SAFETY: job handles may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn with_process(process: std::os::windows::io::RawHandle) -> Option<Job> {
            // SAFETY: a new anonymous job, and a valid process handle.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
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

/// Runs commands as local subprocesses, at most `parallelism` at a time.
pub struct LocalRunner {
    parallelism: usize,
    tasks: JoinSet<TaskResult>,
    active: HashSet<EdgeId>,
    interrupt: Option<Arc<Interrupt>>,
}

impl LocalRunner {
    pub fn new(parallelism: usize, interrupt: Option<Arc<Interrupt>>) -> Self {
        Self { parallelism: parallelism.max(1), tasks: JoinSet::new(), active: HashSet::new(), interrupt }
    }
}

impl CommandRunner for LocalRunner {
    fn can_run_more(&self) -> usize {
        self.parallelism.saturating_sub(self.active.len())
    }

    fn start_command(&mut self, task: Task) -> Result<(), String> {
        self.active.insert(task.edge);
        self.tasks.spawn(async move {
            let (status, output) = run_local_command(&task.command, task.use_console).await;
            TaskResult { edge: task.edge, status, output, kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: Vec::new() }
        });
        Ok(())
    }

    fn wait_for_command(&mut self) -> WaitFuture<'_> {
        Box::pin(async move {
            let interrupt = self.interrupt.clone();
            let joined = match &interrupt {
                Some(i) => {
                    tokio::select! {
                        r = self.tasks.join_next() => r,
                        _ = i.wait() => {
                            let edge = self.active.iter().next().copied().unwrap_or_default();
                            return Some(TaskResult { edge, status: ExitStatus::Interrupted, output: Vec::new(), kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: Vec::new() });
                        }
                    }
                }
                None => self.tasks.join_next().await,
            };
            match joined? {
                Ok(r) => {
                    self.active.remove(&r.edge);
                    Some(r)
                }
                Err(e) => {
                    // A panicking task: report it as a failure of some edge.
                    let edge = self.active.iter().next().copied().unwrap_or_default();
                    self.active.remove(&edge);
                    Some(TaskResult { edge, status: ExitStatus::Failure(1), output: format!("internal error: {e}\n").into_bytes(), kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: Vec::new() })
                }
            }
        })
    }

    fn active_edges(&self) -> Vec<EdgeId> {
        self.active.iter().copied().collect()
    }

    fn abort(&mut self) {
        self.tasks.abort_all();
        self.active.clear();
    }
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
        let dir = std::env::temp_dir().join(format!("ninja-rs-test-{}-cancel", std::process::id()));
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
        assert!(run_local_command_cancellable(&cmd, &[], cancel).await.is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a process of the command kept running");
        // Not cancelled: the command's result.
        let ok = if cfg!(windows) { "cmd /c echo done" } else { "echo done" };
        let (st, out) = run_local_command_cancellable(ok, &[], std::future::pending()).await.unwrap();
        assert_eq!((st, String::from_utf8_lossy(&out).trim().to_string()), (ExitStatus::Success, "done".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stderr_is_captured() {
        let cmd = if cfg!(windows) { "cmd /c echo oops 1>&2" } else { "echo oops 1>&2" };
        let (_, out) = run_local_command(cmd, false).await;
        assert_eq!(String::from_utf8_lossy(&out).trim(), "oops");
    }
}
