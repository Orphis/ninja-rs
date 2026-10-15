//! The command runner used by the CLI: runs each task locally or through a
//! remote backend, according to the backend's policy.
//!
//! Local tasks are limited by `-j`, or by a jobserver's tokens, remote ones
//! by `--remote_jobs`. Tasks are started immediately and wait for a slot of
//! their kind inside their own tokio task, so the engine may have up to
//! `-j + --remote_jobs` tasks in flight.
//!
//! With racing (`--remote_racing`), a task sent remotely also runs locally
//! when a local slot is free: the first branch to finish wins (see
//! `run_raced`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ninja_core::graph::EdgeId;
use ninja_core::runner::{
    AbortFuture, CommandRunner, ExecKind, ExitStatus, LocalCapture, LocalCommand, Race, RemoteBackend, RemoteOutcome, Side, Task, TaskResult, WaitFuture,
    with_host_stop,
};
use ninja_core::subprocess::{Interrupt, Stop, run_argv, run_argv_cancellable, run_local_command_cancellable, run_local_command_env};
use tokio::sync::{Notify, Semaphore, SemaphorePermit};
use tokio::task::JoinSet;

use crate::jobserver;

/// Where local tasks get their slot.
struct LocalSlots {
    kind: SlotKind,
    /// Notified when a slot is given back: raced local runs wait for a free
    /// one this way, without queueing ahead of local tasks.
    released: Notify,
}

enum SlotKind {
    Count(Semaphore),
    Jobserver(Arc<jobserver::Client>),
}

/// Held while the task runs; dropping it gives the slot back.
struct LocalSlot<'a> {
    held: Option<Held<'a>>,
    released: &'a Notify,
}

#[allow(dead_code)]
enum Held<'a> {
    Count(SemaphorePermit<'a>),
    Jobserver(jobserver::Slot),
}

impl Drop for LocalSlot<'_> {
    fn drop(&mut self) {
        // Given back first, so that the waiters notified find it.
        drop(self.held.take());
        self.released.notify_waiters();
    }
}

impl LocalSlots {
    fn new(kind: SlotKind) -> Self {
        Self { kind, released: Notify::new() }
    }

    fn slot<'a>(&'a self, held: Held<'a>) -> LocalSlot<'a> {
        LocalSlot { held: Some(held), released: &self.released }
    }

    async fn acquire(&self) -> Option<LocalSlot<'_>> {
        let held = match &self.kind {
            SlotKind::Count(s) => Held::Count(s.acquire().await.ok()?),
            SlotKind::Jobserver(c) => Held::Jobserver(c.acquire().await),
        };
        Some(self.slot(held))
    }

    /// A slot only if one is free now: tasks waiting for one keep it (the
    /// semaphore hands released permits to its waiters first).
    fn try_acquire(&self) -> Option<LocalSlot<'_>> {
        let held = match &self.kind {
            SlotKind::Count(s) => Held::Count(s.try_acquire().ok()?),
            SlotKind::Jobserver(c) => Held::Jobserver(c.try_acquire()?),
        };
        Some(self.slot(held))
    }
}

/// `--remote_racing`: a task sent remotely also runs locally, after
/// `delay`, when a local slot is free.
#[derive(Clone)]
pub struct Racing {
    pub delay: Duration,
    pub stats: Arc<RaceStats>,
}

/// Raced tasks, by the branch that finished first.
#[derive(Default)]
pub struct RaceStats {
    pub local: AtomicUsize,
    pub remote: AtomicUsize,
}

pub struct ExecRunner {
    local_slots: Arc<LocalSlots>,
    /// Extra environment of local commands.
    env: Arc<Vec<(String, String)>>,
    remote_slots: Arc<Semaphore>,
    capacity: usize,
    tasks: JoinSet<TaskResult>,
    ids: HashMap<tokio::task::Id, EdgeId>,
    active: HashSet<EdgeId>,
    remote: Option<Arc<dyn RemoteBackend>>,
    interrupt: Arc<Interrupt>,
    /// Stops the local commands (see `abort`).
    stop: Arc<Stop>,
    /// Messages about fallbacks, printed by the caller.
    pub verbose_remote: bool,
    /// Race tasks sent remotely with local runs.
    pub racing: Option<Racing>,
}

impl ExecRunner {
    pub fn new(parallelism: usize, remote: Option<Arc<dyn RemoteBackend>>, interrupt: Arc<Interrupt>) -> Self {
        let parallelism = parallelism.max(1);
        let remote_jobs = remote.as_ref().map(|r| r.max_jobs().max(1)).unwrap_or(0);
        Self {
            local_slots: Arc::new(LocalSlots::new(SlotKind::Count(Semaphore::new(parallelism.min(Semaphore::MAX_PERMITS))))),
            env: Arc::default(),
            remote_slots: Arc::new(Semaphore::new(remote_jobs.max(1))),
            capacity: parallelism.saturating_add(remote_jobs),
            tasks: JoinSet::new(),
            ids: HashMap::new(),
            active: HashSet::new(),
            remote,
            interrupt,
            stop: Stop::new(),
            verbose_remote: false,
            racing: None,
        }
    }

    /// Local tasks take a jobserver token instead of a `-j` slot.
    pub fn use_jobserver(&mut self, client: Arc<jobserver::Client>) {
        self.local_slots = Arc::new(LocalSlots::new(SlotKind::Jobserver(client)));
    }

    /// Sets environment variables for local commands.
    pub fn set_env(&mut self, env: Vec<(String, String)>) {
        self.env = Arc::new(env);
    }
}

/// Runs a task on the host, once its inputs that only exist remotely are
/// downloaded: the remote action's `command` when the backend prepared
/// one, else the build file's.
async fn run_local(
    task: &Task,
    slots: &LocalSlots,
    env: &[(String, String)],
    remote: Option<&Arc<dyn RemoteBackend>>,
    command: Option<&LocalCommand>,
    stop: &Stop,
) -> TaskResult {
    if let Some(backend) = remote
        && let Err(e) = backend.materialize(task).await
    {
        let output = format!("ninja-rs: fetching inputs: {}\n", e.message).into_bytes();
        return TaskResult { edge: task.edge, status: ExitStatus::Failure(1), output, kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: e.lost };
    }
    let _permit = slots.acquire().await;
    let (status, output) = match command {
        Some(cmd) => {
            write_files(cmd);
            run_argv(&cmd.argv, &cmd.dir, &with_env(env, cmd), stop).await
        }
        None => run_local_command_env(&task.command, task.use_console, env, stop).await,
    };
    let mut r = TaskResult { edge: task.edge, status, output, kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: Vec::new() };
    map_back(task, command, &mut r);
    r
}

/// After a local run of a prepared command: its depfile and `/showIncludes`
/// lines with local paths.
fn map_back(task: &Task, command: Option<&LocalCommand>, result: &mut TaskResult) {
    if result.status.success()
        && let Some(m) = command.and_then(|c| c.map_back.as_ref())
    {
        m.map_back(task, result);
    }
}

/// Writes the files a prepared command reads (rewritten response files),
/// unless they already hold that: through a temporary file renamed into
/// place, so that a command reading one never sees it half written.
fn write_files(cmd: &LocalCommand) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    for (path, data) in &cmd.files {
        if std::fs::read(path).is_ok_and(|old| old == *data) {
            continue;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{}.{n}", std::process::id()));
        if std::fs::write(&tmp, data).is_err() || std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::write(path, data);
        }
    }
}

/// The local environment, plus the prepared command's variables.
fn with_env(env: &[(String, String)], cmd: &LocalCommand) -> Vec<(String, String)> {
    env.iter().cloned().chain(cmd.env.iter().cloned()).collect()
}

/// The local branch of a raced task. After `delay`, and once the remote
/// branch prepared the action (a local run uses its command, so that both
/// write the same bytes), it starts when a local slot is free, without
/// queueing for one (local tasks keep priority); when the remote branch
/// gave up, it queues like any local task, with the task's own command if
/// the action couldn't be prepared. `None` when the remote branch won, or
/// the inputs couldn't be fetched while the remote branch still runs.
async fn race_local(
    task: &Task,
    slots: &LocalSlots,
    env: &[(String, String)],
    backend: &Arc<dyn RemoteBackend>,
    race: &Race,
    delay: Duration,
    stop: &Stop,
) -> Option<TaskResult> {
    let mut rx = race.subscribe();
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = rx.wait_for(|s| s.local_needed || s.winner.is_some()) => {}
    }
    let _ = rx.wait_for(|s| s.command.is_some() || s.local_needed || s.winner.is_some()).await;
    let _slot = loop {
        let state = race.state();
        if state.winner.is_some() {
            return None;
        }
        if state.local_needed {
            break slots.acquire().await?;
        }
        if let Some(slot) = slots.try_acquire() {
            break slot;
        }
        // Jobserver tokens other processes give back aren't signalled.
        tokio::select! {
            _ = slots.released.notified() => {}
            _ = rx.changed() => {}
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    };
    if let Err(e) = backend.materialize(task).await {
        if !race.state().local_needed {
            return None;
        }
        let output = format!("ninja-rs: fetching inputs: {}\n", e.message).into_bytes();
        return Some(TaskResult {
            edge: task.edge,
            status: ExitStatus::Failure(1),
            output,
            kind: ExecKind::Local,
            cas_outputs: Vec::new(),
            lost_inputs: e.lost,
        });
    }
    if !race.start_local() {
        return None;
    }
    let cancel = async {
        let _ = race.subscribe().wait_for(|s| s.winner == Some(Side::Remote)).await;
    };
    let ran = match race.state().command {
        Some(cmd) => {
            write_files(&cmd);
            run_argv_cancellable(&cmd.argv, &cmd.dir, &with_env(env, &cmd), stop, cancel).await
        }
        None => run_local_command_cancellable(&task.command, env, stop, cancel).await,
    };
    let won = ran.is_some() && race.claim_local();
    race.local_stopped();
    let (status, output) = ran?;
    if !won {
        return None;
    }
    let mut r = TaskResult { edge: task.edge, status, output, kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: Vec::new() };
    map_back(task, race.state().command.as_deref(), &mut r);
    Some(r)
}

/// A task sent remotely, raced by a local run: whichever branch finishes
/// first wins. Only one writes the outputs: the remote one claims them
/// before writing, which stops a local command already running; a local
/// command that finished first claims them, and the remote branch stops.
#[allow(clippy::too_many_arguments)]
async fn run_raced(
    task: Task,
    backend: Arc<dyn RemoteBackend>,
    local_slots: Arc<LocalSlots>,
    remote_slots: Arc<Semaphore>,
    env: Arc<Vec<(String, String)>>,
    racing: Racing,
    verbose: bool,
    stop: Arc<Stop>,
) -> TaskResult {
    let race = Race::new();
    let remote = async {
        let _permit = remote_slots.acquire().await;
        with_host_stop(stop.clone(), backend.execute_raced(task.clone(), race.clone())).await
    };
    let local = race_local(&task, &local_slots, &env, &backend, &race, racing.delay, &stop);
    tokio::pin!(remote, local);
    let (mut remote_done, mut local_done) = (false, false);
    // A remote result that wrote nothing, while a local command runs.
    let mut pending: Option<TaskResult> = None;
    let mut reason: Option<String> = None;
    let mut capture: Option<Box<dyn LocalCapture>> = None;
    loop {
        tokio::select! {
            outcome = &mut remote, if !remote_done => {
                remote_done = true;
                match outcome {
                    RemoteOutcome::Done(r) => {
                        if race.state().winner == Some(Side::Remote) {
                            racing.stats.remote.fetch_add(1, Ordering::Relaxed);
                            return r;
                        }
                        // It wrote nothing (a failure): a running local
                        // command decides, else it's the result.
                        if local_done || !race.state().local_running && race.claim_remote().await {
                            return r;
                        }
                        pending = Some(r);
                    }
                    RemoteOutcome::Lost => {}
                    // The race has the prepared command, if any.
                    RemoteOutcome::Fallback(why, _) => {
                        reason = Some(why);
                        race.need_local();
                    }
                    RemoteOutcome::RunLocally(c, _) => {
                        capture = Some(c);
                        race.need_local();
                    }
                }
                if local_done {
                    // The local branch gave up before running.
                    let command = race.state().command;
                    let r = run_local(&task, &local_slots, &env, Some(&backend), command.as_deref(), &stop).await;
                    return finish_local(r, capture, reason, verbose).await;
                }
            }
            ran = &mut local, if !local_done => {
                local_done = true;
                if let Some(r) = ran {
                    if reason.is_none() && capture.is_none() {
                        racing.stats.local.fetch_add(1, Ordering::Relaxed);
                    }
                    return finish_local(r, capture, reason, verbose).await;
                }
                if remote_done {
                    if let Some(r) = pending.take() {
                        return r;
                    }
                    let command = race.state().command;
                    let r = run_local(&task, &local_slots, &env, Some(&backend), command.as_deref(), &stop).await;
                    return finish_local(r, capture, reason, verbose).await;
                }
            }
        }
    }
}

/// The result of a local run of a task the remote branch gave up on: given
/// to the capture, explained in verbose mode.
async fn finish_local(mut r: TaskResult, capture: Option<Box<dyn LocalCapture>>, reason: Option<String>, verbose: bool) -> TaskResult {
    if let Some(capture) = capture {
        // Storing outputs reads files: off the async threads.
        let result = r.clone();
        let _ = tokio::task::spawn_blocking(move || capture.capture(&result)).await;
    }
    if verbose && let Some(reason) = reason {
        let mut msg = format!("ninja-rs: running locally ({reason})\n").into_bytes();
        msg.extend_from_slice(&r.output);
        r.output = msg;
    }
    r
}

impl CommandRunner for ExecRunner {
    fn can_run_more(&self) -> usize {
        self.capacity.saturating_sub(self.active.len())
    }

    fn start_command(&mut self, task: Task) -> Result<(), String> {
        let edge = task.edge;
        self.active.insert(edge);
        let local_slots = self.local_slots.clone();
        let env = self.env.clone();
        let inputs_from = self.remote.clone();
        let remote = self.remote.clone().filter(|r| !task.use_console && r.wants(&task));
        let verbose = self.verbose_remote;
        let stop = self.stop.clone();
        match remote {
            None => {
                let h = self.tasks.spawn(async move { run_local(&task, &local_slots, &env, inputs_from.as_ref(), None, &stop).await });
                self.ids.insert(h.id(), edge);
            }
            Some(backend) if self.racing.is_some() => {
                let remote_slots = self.remote_slots.clone();
                let racing = self.racing.clone().expect("racing");
                let h = self.tasks.spawn(run_raced(task, backend, local_slots, remote_slots, env, racing, verbose, stop));
                self.ids.insert(h.id(), edge);
            }
            Some(backend) => {
                let remote_slots = self.remote_slots.clone();
                let h = self.tasks.spawn(async move {
                    let outcome = {
                        let _permit = remote_slots.acquire().await;
                        with_host_stop(stop.clone(), backend.execute(task.clone())).await
                    };
                    match outcome {
                        RemoteOutcome::Done(r) => r,
                        // Only raced tasks lose.
                        RemoteOutcome::Lost => TaskResult {
                            edge: task.edge,
                            status: ExitStatus::Failure(1),
                            output: b"ninja-rs: internal error: a task that wasn't raced lost\n".to_vec(),
                            kind: ExecKind::Remote,
                            cas_outputs: Vec::new(),
                            lost_inputs: Vec::new(),
                        },
                        RemoteOutcome::Fallback(reason, command) => {
                            let mut r = run_local(&task, &local_slots, &env, Some(&backend), command.as_ref(), &stop).await;
                            if verbose {
                                let mut msg = format!("ninja-rs: running locally ({reason})\n").into_bytes();
                                msg.extend_from_slice(&r.output);
                                r.output = msg;
                            }
                            r
                        }
                        RemoteOutcome::RunLocally(capture, command) => {
                            let r = run_local(&task, &local_slots, &env, Some(&backend), command.as_ref(), &stop).await;
                            // Storing outputs reads files: off the async threads.
                            let result = r.clone();
                            let _ = tokio::task::spawn_blocking(move || capture.capture(&result)).await;
                            r
                        }
                    }
                });
                self.ids.insert(h.id(), edge);
            }
        }
        Ok(())
    }

    fn wait_for_command(&mut self) -> WaitFuture<'_> {
        Box::pin(async move {
            let interrupt = self.interrupt.clone();
            let joined = tokio::select! {
                r = self.tasks.join_next_with_id() => r,
                _ = interrupt.wait() => {
                    let edge = self.active.iter().next().copied().unwrap_or_default();
                    return Some(TaskResult { edge, status: ExitStatus::Interrupted, output: Vec::new(), kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: Vec::new() });
                }
            };
            match joined? {
                Ok((id, r)) => {
                    self.ids.remove(&id);
                    self.active.remove(&r.edge);
                    Some(r)
                }
                Err(e) => {
                    let edge = self.ids.remove(&e.id()).unwrap_or_default();
                    self.active.remove(&edge);
                    Some(TaskResult {
                        edge,
                        status: ExitStatus::Failure(1),
                        output: format!("ninja-rs: internal error: {e}\n").into_bytes(),
                        kind: ExecKind::Local,
                        cas_outputs: Vec::new(),
                        lost_inputs: Vec::new(),
                    })
                }
            }
        })
    }

    fn active_edges(&self) -> Vec<EdgeId> {
        self.active.iter().copied().collect()
    }

    fn abort(&mut self) -> AbortFuture<'_> {
        Box::pin(async move {
            // Local commands first (steps backends run on the host too),
            // waited for, so that none writes outputs after the build
            // deletes them; then the rest (remote executions, tasks waiting
            // for a slot) is dropped.
            self.stop.stop(Some(&self.interrupt)).await;
            self.tasks.abort_all();
            while self.tasks.join_next().await.is_some() {}
            self.ids.clear();
            self.active.clear();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn raced_runs_only_take_free_slots() {
        let slots = Arc::new(LocalSlots::new(SlotKind::Count(Semaphore::new(1))));
        let held = slots.try_acquire().expect("a free slot");
        assert!(slots.try_acquire().is_none());
        // A local task waits for the slot: it gets it when it's given back,
        // not a raced run trying at the same time.
        let s = slots.clone();
        let waiter = tokio::spawn(async move {
            let _slot = s.acquire().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let released = slots.released.notified();
        drop(held);
        released.await;
        assert!(slots.try_acquire().is_none(), "a raced run took the slot a local task waits for");
        waiter.await.unwrap();
        assert!(slots.try_acquire().is_some());
    }

    #[tokio::test]
    async fn abort_waits_for_every_local_process() {
        let dir = test_dirs::base().join(format!("ninja-rs-test-{}-abort", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The command's grandchild writes the marker after about 2 s.
        let marker = dir.join("late.txt");
        let command = if cfg!(windows) {
            // `waitfor` stops on Ctrl-Break; the marker is written only if it
            // timed out.
            format!("cmd /c cmd /v:on /c \"waitfor /t 2 ninjarsabort{} >nul 2>&1 & if !errorlevel! == 1 echo late> {}\"", std::process::id(), marker.display())
        } else {
            format!("sh -c 'sh -c \"sleep 2; echo late > {}\"; true'", marker.display())
        };
        let interrupt = Interrupt::new();
        let mut runner = ExecRunner::new(2, None, interrupt.clone());
        runner.start_command(Task { command, ..Default::default() }).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        interrupt.trigger();
        assert_eq!(runner.wait_for_command().await.map(|r| r.status), Some(ExitStatus::Interrupted));
        let start = std::time::Instant::now();
        runner.abort().await;
        assert!(start.elapsed() < Duration::from_secs(1), "{:?}", start.elapsed());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a process of the command kept running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backend whose action is a step on the host.
    #[cfg(unix)]
    struct HostStep(String);

    #[cfg(unix)]
    impl RemoteBackend for HostStep {
        fn wants(&self, _: &Task) -> bool {
            true
        }
        fn execute(&self, task: Task) -> ninja_core::runner::RemoteFuture {
            let argv = vec!["sh".to_string(), "-c".to_string(), self.0.clone()];
            Box::pin(async move {
                let stop = ninja_core::runner::host_stop();
                let (status, output) = run_argv(&argv, &std::env::temp_dir(), &[], &stop).await;
                RemoteOutcome::Done(TaskResult { edge: task.edge, status, output, kind: ExecKind::Remote, cas_outputs: Vec::new(), lost_inputs: Vec::new() })
            })
        }
        fn max_jobs(&self) -> usize {
            1
        }
    }

    /// Steps a backend runs on the host are stopped as local commands are:
    /// with the interrupt's signal, and waited for.
    #[cfg(unix)]
    #[tokio::test]
    async fn abort_stops_host_steps() {
        let dir = std::env::temp_dir().join(format!("ninja-rs-test-{}-abort-host", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ready, stopped) = (dir.join("ready"), dir.join("stopped"));
        let step = format!("trap 'echo > {}; exit 1' INT; echo > {}; while :; do sleep 0.1; done", stopped.display(), ready.display());
        let interrupt = Interrupt::new();
        let mut runner = ExecRunner::new(2, Some(Arc::new(HostStep(step))), interrupt.clone());
        runner.start_command(Task::default()).unwrap();
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        interrupt.trigger();
        assert_eq!(runner.wait_for_command().await.map(|r| r.status), Some(ExitStatus::Interrupted));
        runner.abort().await;
        assert!(stopped.exists(), "the step didn't get the interrupt's signal before the build went on");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
