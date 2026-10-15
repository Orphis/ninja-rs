//! The command runner used by the CLI: runs each task locally or through a
//! remote backend, according to the backend's policy.
//!
//! Local tasks are limited by `-j`, or by a jobserver's tokens, remote ones
//! by `--remote_jobs`: the engine starts a task when a slot of its kind is
//! free (see `CommandRunner::has_free_slot`), so that up to
//! `-j + --remote_jobs` run at once, those waiting for a slot waiting in
//! the plan. A task still waits for a slot inside its own tokio task when
//! it needs another than the one it was started for, or a jobserver token
//! held elsewhere: a remote action run locally after all (a fallback) then
//! counts as a local task, so that the engine starts no more of those
//! meanwhile. Without remote execution (a disk cache, or a remote cache,
//! alone), where every miss runs locally, tasks take local slots only, the
//! cache looked up in theirs.
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
    /// semaphore hands released permits to its waiters first, and the
    /// jobserver client its tokens).
    fn try_acquire(&self) -> Option<LocalSlot<'_>> {
        let held = match &self.kind {
            SlotKind::Count(s) => Held::Count(s.try_acquire().ok()?),
            SlotKind::Jobserver(c) => Held::Jobserver(c.try_acquire_in_turn()?),
        };
        Some(self.slot(held))
    }
}

/// How many tasks take slots of each kind (`LOCAL`, `REMOTE`), shared with
/// the tasks (see `Counted`).
type Counts = Arc<[AtomicUsize; 2]>;

/// A task in `Counts`, under the kind of slots it takes, until it ends
/// (dropped, its future done or dropped).
struct Counted {
    counts: Counts,
    kind: AtomicUsize,
}

impl Counted {
    fn new(counts: Counts, kind: usize) -> Self {
        counts[kind].fetch_add(1, Ordering::Relaxed);
        Self { counts, kind: AtomicUsize::new(kind) }
    }

    /// The task waits for a local slot, or holds one, from now on.
    fn local(&self) {
        let was = self.kind.swap(LOCAL, Ordering::Relaxed);
        if was != LOCAL {
            self.counts[LOCAL].fetch_add(1, Ordering::Relaxed);
            self.counts[was].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.counts[self.kind.load(Ordering::Relaxed)].fetch_sub(1, Ordering::Relaxed);
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
    /// How many tasks of each kind run at once (`LOCAL`, `REMOTE`).
    jobs: [usize; 2],
    tasks: JoinSet<TaskResult>,
    ids: HashMap<tokio::task::Id, EdgeId>,
    /// The tasks started and not reaped, and how many of those running
    /// take slots of each kind.
    active: HashSet<EdgeId>,
    counts: Counts,
    remote: Option<Arc<dyn RemoteBackend>>,
    /// The backend runs tasks remotely (see `RemoteBackend::executes`).
    executes: bool,
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
        let executes = remote.as_ref().is_some_and(|r| r.executes());
        Self {
            local_slots: Arc::new(LocalSlots::new(SlotKind::Count(Semaphore::new(parallelism.min(Semaphore::MAX_PERMITS))))),
            env: Arc::default(),
            remote_slots: Arc::new(Semaphore::new(remote_jobs.max(1))),
            capacity: parallelism.saturating_add(if executes { remote_jobs } else { 0 }),
            jobs: [parallelism, remote_jobs],
            tasks: JoinSet::new(),
            ids: HashMap::new(),
            active: HashSet::new(),
            counts: Counts::default(),
            remote,
            executes,
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
/// one, else the build file's. It counts as local (`counted`) from when it
/// waits for a local slot.
async fn run_local(
    task: &Task,
    slots: &LocalSlots,
    env: &[(String, String)],
    remote: Option<&Arc<dyn RemoteBackend>>,
    command: Option<&LocalCommand>,
    stop: &Stop,
    counted: &Counted,
) -> TaskResult {
    if let Some(backend) = remote
        && let Err(e) = backend.materialize(task).await
    {
        let output = format!("ninja: fetching inputs: {}\n", e.message).into_bytes();
        return TaskResult { edge: task.edge, status: ExitStatus::Failure(1), output, kind: ExecKind::Local, cas_outputs: Vec::new(), lost_inputs: e.lost };
    }
    counted.local();
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
/// write the same bytes), its inputs are fetched, and it starts when a
/// local slot is free, without queueing for one (local tasks keep
/// priority); when the remote branch gave up, it queues like any local
/// task, with the task's own command if the action couldn't be prepared,
/// and counts as one (`counted`). `None` when the remote branch won, or
/// the inputs couldn't be fetched while the remote branch still runs.
#[allow(clippy::too_many_arguments)]
async fn race_local(
    task: &Task,
    slots: &LocalSlots,
    env: &[(String, String)],
    backend: &Arc<dyn RemoteBackend>,
    race: &Race,
    delay: Duration,
    stop: &Stop,
    counted: &Counted,
) -> Option<TaskResult> {
    let mut rx = race.subscribe();
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = rx.wait_for(|s| s.local_needed || s.winner.is_some()) => {}
    }
    let _ = rx.wait_for(|s| s.command.is_some() || s.local_needed || s.winner.is_some()).await;
    if race.state().winner.is_some() {
        return None;
    }
    // Before taking a slot, which a download would keep idle.
    if let Err(e) = backend.materialize(task).await {
        if !race.state().local_needed {
            return None;
        }
        let output = format!("ninja: fetching inputs: {}\n", e.message).into_bytes();
        return Some(TaskResult {
            edge: task.edge,
            status: ExitStatus::Failure(1),
            output,
            kind: ExecKind::Local,
            cas_outputs: Vec::new(),
            lost_inputs: e.lost,
        });
    }
    let _slot = loop {
        let state = race.state();
        if state.winner.is_some() {
            return None;
        }
        if state.local_needed {
            counted.local();
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
    counted: Counted,
) -> TaskResult {
    let race = Race::new();
    let remote = async {
        let _permit = remote_slots.acquire().await;
        with_host_stop(stop.clone(), backend.execute_raced(task.clone(), race.clone())).await
    };
    let local = race_local(&task, &local_slots, &env, &backend, &race, racing.delay, &stop, &counted);
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
                    let r = run_local(&task, &local_slots, &env, Some(&backend), command.as_deref(), &stop, &counted).await;
                    return finish_local(&task, r, capture, reason, verbose).await;
                }
            }
            ran = &mut local, if !local_done => {
                local_done = true;
                if let Some(r) = ran {
                    if reason.is_none() && capture.is_none() {
                        racing.stats.local.fetch_add(1, Ordering::Relaxed);
                    }
                    return finish_local(&task, r, capture, reason, verbose).await;
                }
                if remote_done {
                    if let Some(r) = pending.take() {
                        return r;
                    }
                    let command = race.state().command;
                    let r = run_local(&task, &local_slots, &env, Some(&backend), command.as_deref(), &stop, &counted).await;
                    return finish_local(&task, r, capture, reason, verbose).await;
                }
            }
        }
    }
}

/// The result of a local run of a task the remote branch gave up on: given
/// to the capture, explained in verbose mode.
async fn finish_local(task: &Task, r: TaskResult, capture: Option<Box<dyn LocalCapture>>, reason: Option<String>, verbose: bool) -> TaskResult {
    if let Some(capture) = capture {
        // Storing outputs reads files: off the async threads.
        let result = r.clone();
        let _ = tokio::task::spawn_blocking(move || capture.capture(&result)).await;
    }
    if verbose && let Some(reason) = reason {
        ran_locally(task, &reason);
    }
    r
}

/// `--remote_verbose`: says why a task sent remotely ran locally, on
/// stderr, as the other remote decisions.
fn ran_locally(task: &Task, reason: &str) {
    ninja_core::status::message(&format!("{}: running locally ({reason})", task.outputs.join(" ")));
}

impl CommandRunner for ExecRunner {
    fn can_run_more(&self) -> usize {
        self.capacity.saturating_sub(self.active.len())
    }

    /// Sends the task remotely when the engine started it in remote slots
    /// (see `slots_of`); without remote execution, to the cache when the
    /// backend's policy says so, in the local slot it was started in.
    fn start_command(&mut self, task: Task) -> Result<(), String> {
        let edge = task.edge;
        let local_slots = self.local_slots.clone();
        let env = self.env.clone();
        let inputs_from = self.remote.clone();
        let wanted = |r: &Arc<dyn RemoteBackend>| if self.executes { task.slots == REMOTE } else { !task.use_console && r.wants(&task) };
        let remote = self.remote.clone().filter(wanted);
        self.active.insert(edge);
        let counted = Counted::new(self.counts.clone(), if self.executes && remote.is_some() { REMOTE } else { LOCAL });
        let verbose = self.verbose_remote;
        let stop = self.stop.clone();
        match remote {
            None => {
                let h = self.tasks.spawn(async move { run_local(&task, &local_slots, &env, inputs_from.as_ref(), None, &stop, &counted).await });
                self.ids.insert(h.id(), edge);
            }
            Some(backend) if self.racing.is_some() => {
                let remote_slots = self.remote_slots.clone();
                let racing = self.racing.clone().expect("racing");
                let h = self.tasks.spawn(run_raced(task, backend, local_slots, remote_slots, env, racing, verbose, stop, counted));
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
                            output: b"ninja: internal error: a task that wasn't raced lost\n".to_vec(),
                            kind: ExecKind::Remote,
                            cas_outputs: Vec::new(),
                            lost_inputs: Vec::new(),
                        },
                        RemoteOutcome::Fallback(reason, command) => {
                            let r = run_local(&task, &local_slots, &env, Some(&backend), command.as_ref(), &stop, &counted).await;
                            if verbose {
                                ran_locally(&task, &reason);
                            }
                            r
                        }
                        RemoteOutcome::RunLocally(capture, command) => {
                            let r = run_local(&task, &local_slots, &env, Some(&backend), command.as_ref(), &stop, &counted).await;
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
                        output: format!("ninja: internal error: {e}\n").into_bytes(),
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

    /// Only a backend reads them: remote actions send them, and those of
    /// local commands that exist remotely only are downloaded first.
    fn wants_inputs(&self) -> bool {
        self.remote.is_some()
    }

    /// Local and remote slots, with remote execution.
    fn slot_kinds(&self) -> usize {
        if self.executes { 2 } else { 1 }
    }

    /// Remote slots for a task the backend's policy sends there (raced ones
    /// too, which take a local slot only when one is free), else local.
    fn slots_of(&self, task: &Task) -> usize {
        if self.executes && self.remote.as_ref().is_some_and(|r| !task.use_console && r.wants(task)) { REMOTE } else { LOCAL }
    }

    fn has_free_slot(&self, kind: usize) -> bool {
        self.counts[kind].load(Ordering::Relaxed) < self.jobs[kind]
    }
}

/// The kinds of slots of an `ExecRunner`'s tasks (see `CommandRunner::slots_of`).
const LOCAL: usize = 0;
const REMOTE: usize = 1;

#[cfg(test)]
mod tests {
    use super::*;

    /// How long tests wait for what a healthy run does at once: only a
    /// broken one gets there (the machine may be busy).
    const PATIENCE: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn raced_runs_only_take_free_slots() {
        let slots = Arc::new(LocalSlots::new(SlotKind::Count(Semaphore::new(1))));
        let held = slots.try_acquire().expect("a free slot");
        assert!(slots.try_acquire().is_none());
        // A local task waits for the slot: it gets it when it's given back,
        // not a raced run trying at the same time.
        let waiter = slots.acquire();
        tokio::pin!(waiter);
        let pending = std::future::poll_fn(|cx| std::task::Poll::Ready(waiter.as_mut().poll(cx).is_pending())).await;
        assert!(pending, "the slot is held");
        let released = slots.released.notified();
        drop(held);
        released.await;
        assert!(slots.try_acquire().is_none(), "a raced run took the slot a local task waits for");
        assert!(waiter.await.is_some());
        assert!(slots.try_acquire().is_some());
    }

    /// A backend sending the tasks of the rule `remote` there, whose
    /// executions never end.
    struct Endless;

    impl RemoteBackend for Endless {
        fn wants(&self, task: &Task) -> bool {
            task.rule == "remote"
        }
        fn execute(&self, _: Task) -> ninja_core::runner::RemoteFuture {
            Box::pin(std::future::pending())
        }
        fn max_jobs(&self) -> usize {
            1
        }
    }

    /// A task takes a slot of its kind, local or remote, as the backend's
    /// policy decides: the engine starts one only when one is free, other
    /// kinds starting meanwhile (see `CommandRunner::has_free_slot`).
    #[tokio::test]
    async fn tasks_take_slots_of_their_kind() {
        let task = |rule: &str, edge: u32| Task { edge: EdgeId(edge), rule: rule.into(), outputs: vec![format!("o{edge}")], ..Default::default() };
        let mut runner = ExecRunner::new(2, Some(Arc::new(Endless)), Interrupt::new());
        assert_eq!(runner.slot_kinds(), 2);
        assert_eq!((runner.slots_of(&task("remote", 0)), runner.slots_of(&task("cc", 1))), (REMOTE, LOCAL));
        // A console task runs on the terminal: locally.
        assert_eq!(runner.slots_of(&Task { use_console: true, ..task("remote", 2) }), LOCAL);
        assert!(runner.has_free_slot(REMOTE) && runner.has_free_slot(LOCAL));
        runner.start_command(Task { slots: REMOTE, ..task("remote", 0) }).unwrap();
        assert!(!runner.has_free_slot(REMOTE) && runner.has_free_slot(LOCAL));
        assert_eq!(runner.can_run_more(), 2);
        runner.abort().await;
        assert!(runner.has_free_slot(REMOTE));
        // Without a backend, one kind.
        assert_eq!(ExecRunner::new(2, None, Interrupt::new()).slot_kinds(), 1);
    }

    /// A backend sending the tasks of the rule `remote` there, which run
    /// locally after all (cache misses without remote execution).
    struct Misses;

    impl RemoteBackend for Misses {
        fn wants(&self, task: &Task) -> bool {
            task.rule == "remote"
        }
        fn execute(&self, _: Task) -> ninja_core::runner::RemoteFuture {
            Box::pin(async { RemoteOutcome::Fallback("a cache miss".into(), None) })
        }
        fn max_jobs(&self) -> usize {
            1
        }
    }

    /// A task sent remotely that runs locally after all (a cache miss with
    /// a disk cache alone, a fallback) counts as a local one once it waits
    /// for a local slot: the engine starts no other local task meanwhile,
    /// and starts remote ones. (It took a remote slot while it waited.)
    #[tokio::test]
    async fn tasks_run_locally_after_all_count_as_local() {
        let mut runner = ExecRunner::new(1, Some(Arc::new(Misses)), Interrupt::new());
        let slots = runner.local_slots.clone();
        let held = slots.try_acquire().expect("a free slot");
        let command = if cfg!(windows) { "cmd /c exit 0" } else { "true" }.to_string();
        runner.start_command(Task { rule: "remote".into(), command, slots: REMOTE, ..Default::default() }).unwrap();
        let start = std::time::Instant::now();
        while runner.has_free_slot(LOCAL) {
            assert!(start.elapsed() < PATIENCE, "a task waiting for a local slot doesn't count as local");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(runner.has_free_slot(REMOTE));
        drop(held);
        let r = runner.wait_for_command().await.expect("a result");
        assert_eq!((r.status, r.kind), (ExitStatus::Success, ExecKind::Local));
        assert!(runner.has_free_slot(LOCAL) && runner.has_free_slot(REMOTE));
    }

    /// A cache without remote execution, where the tasks of the rule
    /// `remote` are looked up (counted in `lookups`), and miss.
    struct CacheAlone {
        lookups: Arc<AtomicUsize>,
    }

    impl RemoteBackend for CacheAlone {
        fn wants(&self, task: &Task) -> bool {
            task.rule == "remote"
        }
        fn execute(&self, _: Task) -> ninja_core::runner::RemoteFuture {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { RemoteOutcome::Fallback("a cache miss".into(), None) })
        }
        fn max_jobs(&self) -> usize {
            50
        }
        fn executes(&self) -> bool {
            false
        }
    }

    /// Without remote execution (a disk cache, a remote cache, alone), where
    /// every miss runs locally, tasks take local slots only: no more start
    /// than `-j`, in the plan's order. (Up to `-j + --remote_jobs` started,
    /// the misses waiting in the runner for a local slot, in turn.)
    #[tokio::test]
    async fn caches_alone_take_local_slots() {
        let lookups = Arc::new(AtomicUsize::new(0));
        let mut runner = ExecRunner::new(2, Some(Arc::new(CacheAlone { lookups: lookups.clone() })), Interrupt::new());
        let remote = Task { rule: "remote".into(), command: if cfg!(windows) { "cmd /c exit 0" } else { "true" }.to_string(), ..Default::default() };
        assert_eq!((runner.slot_kinds(), runner.can_run_more(), runner.slots_of(&remote)), (1, 2, LOCAL));
        runner.start_command(remote).unwrap();
        let r = runner.wait_for_command().await.expect("a result");
        assert_eq!((r.status, lookups.load(Ordering::Relaxed)), (ExitStatus::Success, 1));
    }

    /// A backend whose inputs take until `release` to download, saying when
    /// they start (`started`).
    struct SlowInputs {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl RemoteBackend for SlowInputs {
        fn wants(&self, _: &Task) -> bool {
            true
        }
        fn execute(&self, _: Task) -> ninja_core::runner::RemoteFuture {
            Box::pin(std::future::pending())
        }
        fn max_jobs(&self) -> usize {
            1
        }
        fn materialize(&self, _: &Task) -> ninja_core::runner::MaterializeFuture {
            let (started, release) = (self.started.clone(), self.release.clone());
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
                Ok(())
            })
        }
    }

    /// A raced local run fetches its inputs before it takes a local slot:
    /// other local tasks use the slot meanwhile.
    #[tokio::test]
    async fn raced_runs_fetch_inputs_before_taking_a_slot() {
        let slots = LocalSlots::new(SlotKind::Count(Semaphore::new(1)));
        let started = Arc::new(Notify::new());
        let backend: Arc<dyn RemoteBackend> = Arc::new(SlowInputs { started: started.clone(), release: Arc::new(Notify::new()) });
        let race = Race::new();
        // The remote branch gave up: the local one runs.
        race.need_local();
        let (task, stop) = (Task::default(), Stop::new());
        let counted = Counted::new(Counts::default(), REMOTE);
        let local = race_local(&task, &slots, &[], &backend, &race, Duration::ZERO, &stop, &counted);
        tokio::pin!(local);
        tokio::select! {
            _ = &mut local => panic!("ran before its inputs were fetched"),
            _ = started.notified() => {}
        }
        assert!(slots.try_acquire().is_some(), "a slot held while the inputs download");
    }

    /// Run by `abort_waits_for_every_local_process` as a command's child:
    /// writes its process id, then sleeps for longer than the test waits.
    #[test]
    #[ignore = "run by abort_waits_for_every_local_process"]
    fn sleeper() {
        let Some(pid_file) = std::env::var_os("NINJA_RS_TEST_SLEEPER").map(std::path::PathBuf::from) else { return };
        let tmp = pid_file.with_extension("tmp");
        std::fs::write(&tmp, std::process::id().to_string()).unwrap();
        std::fs::rename(&tmp, &pid_file).unwrap();
        std::thread::sleep(2 * PATIENCE);
    }

    /// Whether process `pid` still runs (a zombie doesn't).
    fn alive(pid: u32) -> bool {
        ninja_os::process_running(pid) == Some(true)
    }

    #[tokio::test]
    async fn abort_waits_for_every_local_process() {
        let dir = test_dirs::TestDir::new("ninja-rs-runner-abort");
        let pid_file = dir.join("sleeper.pid");
        // The command's child sleeps for longer than the test waits.
        let sleeper = format!("\"{}\" --exact exec_runner::tests::sleeper --ignored --nocapture", std::env::current_exe().unwrap().display());
        let command = if cfg!(windows) { format!("cmd /c \"{sleeper}\"") } else { format!("sh -c '{sleeper}; true'") };
        let interrupt = Interrupt::new();
        let mut runner = ExecRunner::new(2, None, interrupt.clone());
        runner.set_env(vec![("NINJA_RS_TEST_SLEEPER".into(), pid_file.to_string_lossy().into_owned())]);
        runner.start_command(Task { command, ..Default::default() }).unwrap();
        while !pid_file.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        interrupt.trigger();
        assert_eq!(runner.wait_for_command().await.map(|r| r.status), Some(ExitStatus::Interrupted));
        tokio::time::timeout(PATIENCE, runner.abort()).await.expect("the abort waited for the command to end by itself");
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        // On Windows, a job counts a terminated process out a moment before
        // it is gone (it runs no more code then).
        if cfg!(windows) {
            let _ = tokio::time::timeout(PATIENCE, async {
                while alive(pid) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
        }
        assert!(!alive(pid), "a process of the command kept running");
    }

    /// A backend whose action is a step on the host, run in `dir`.
    #[cfg(unix)]
    struct HostStep {
        step: String,
        dir: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl RemoteBackend for HostStep {
        fn wants(&self, _: &Task) -> bool {
            true
        }
        fn execute(&self, task: Task) -> ninja_core::runner::RemoteFuture {
            let argv = vec!["sh".to_string(), "-c".to_string(), self.step.clone()];
            let dir = self.dir.clone();
            Box::pin(async move {
                let stop = ninja_core::runner::host_stop();
                let (status, output) = run_argv(&argv, &dir, &[], &stop).await;
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
        let dir = test_dirs::TestDir::new("ninja-rs-runner-abort-host");
        let (ready, stopped) = (dir.join("ready"), dir.join("stopped"));
        let step = format!("trap 'echo > {}; exit 1' INT; echo > {}; while :; do sleep 0.1; done", stopped.display(), ready.display());
        let interrupt = Interrupt::new();
        let mut runner = ExecRunner::new(2, Some(Arc::new(HostStep { step, dir: dir.to_path_buf() })), interrupt.clone());
        runner.start_command(Task { slots: REMOTE, ..Default::default() }).unwrap();
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        interrupt.trigger();
        assert_eq!(runner.wait_for_command().await.map(|r| r.status), Some(ExitStatus::Interrupted));
        runner.abort().await;
        assert!(stopped.exists(), "the step didn't get the interrupt's signal before the build went on");
    }
}
