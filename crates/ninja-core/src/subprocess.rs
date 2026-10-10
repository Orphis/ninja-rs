//! Local command execution (ninja's `Subprocess`/`SubprocessSet`).
//!
//! As in ninja, commands run through `/bin/sh -c` on POSIX and are passed
//! to `CreateProcess` verbatim on Windows. stdout and stderr share one pipe
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

fn build_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
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
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Runs one command. Returns the exit status and combined stdout/stderr.
pub async fn run_local_command(command: &str, use_console: bool) -> (ExitStatus, Vec<u8>) {
    run_local_command_env(command, use_console, &[]).await
}

/// [`run_local_command`] with extra environment variables.
pub async fn run_local_command_env(command: &str, use_console: bool, env: &[(String, String)]) -> (ExitStatus, Vec<u8>) {
    let mut cmd = build_command(command);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    cmd.kill_on_drop(true);
    if use_console {
        cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        return match cmd.spawn() {
            Ok(mut child) => match child.wait().await {
                Ok(st) => (exit_status(st), Vec::new()),
                Err(e) => (ExitStatus::Failure(1), format!("waiting for command: {e}\n").into_bytes()),
            },
            Err(e) => (ExitStatus::Failure(1), format!("{e}\n").into_bytes()),
        };
    }
    let (mut reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => return (ExitStatus::Failure(1), format!("pipe: {e}\n").into_bytes()),
    };
    let writer2 = match writer.try_clone() {
        Ok(w) => w,
        Err(e) => return (ExitStatus::Failure(1), format!("pipe: {e}\n").into_bytes()),
    };
    cmd.stdin(Stdio::null()).stdout(writer).stderr(writer2);
    let spawned = cmd.spawn();
    // Close our copies of the write end so the reader sees EOF.
    drop(cmd);
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return (ExitStatus::Failure(1), format!("CreateProcess failed: {e}\n").into_bytes());
        }
    };
    let read = tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let status = child.wait().await;
    let output = read.await.unwrap_or_default();
    match status {
        Ok(st) => (exit_status(st), output),
        Err(e) => (ExitStatus::Failure(1), format!("waiting for command: {e}\n").into_bytes()),
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

    #[tokio::test]
    async fn stderr_is_captured() {
        let cmd = if cfg!(windows) { "cmd /c echo oops 1>&2" } else { "echo oops 1>&2" };
        let (_, out) = run_local_command(cmd, false).await;
        assert_eq!(String::from_utf8_lossy(&out).trim(), "oops");
    }
}
