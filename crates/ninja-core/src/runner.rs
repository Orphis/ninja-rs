//! The interface between the build engine and whatever runs commands
//! (local subprocesses, a remote executor, or a dry run).
//!
//! The engine never shares its graph with runners: it hands them a
//! self-contained [`Task`] and gets a [`TaskResult`] back.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::graph::EdgeId;
use crate::subprocess::Stop;

/// Everything a runner needs to execute one edge.
#[derive(Clone, Debug, Default)]
pub struct Task {
    pub edge: EdgeId,
    /// Name of the edge's rule.
    pub rule: String,
    /// The evaluated `command` (with `$in`/`$out` shell-escaped).
    pub command: String,
    pub description: String,
    pub use_console: bool,
    /// `rspfile` path (unescaped) and content. The engine writes it to disk
    /// before starting the task.
    pub rspfile: Option<(String, String)>,
    /// `depfile` path (unescaped), if any.
    pub depfile: Option<String>,
    /// `deps` binding (`gcc`, `msvc`), if any.
    pub deps: Option<String>,
    pub msvc_deps_prefix: String,
    /// All outputs (explicit then implicit), canonical paths.
    pub outputs: Vec<String>,
    /// Explicit and implicit inputs (no order-only), canonical paths, and
    /// the deps earlier builds discovered. Those of an edge that was dirty
    /// before reading them aren't in the graph; they are hints, and may name
    /// files that are gone. Empty for a runner that doesn't read them
    /// (`CommandRunner::wants_inputs`).
    pub inputs: Vec<String>,
    pub restat: bool,
    pub generator: bool,
    /// Extra edge bindings requested through `BuildConfig::task_bindings`
    /// (e.g. `rbe_local`, `rbe_mnemonic`, `rbe_target`), evaluated; empty values
    /// are omitted.
    pub bindings: Vec<(String, String)>,
    /// The kind of slots the engine started it in, as
    /// `CommandRunner::slots_of` told (0 for a runner of one kind).
    pub slots: usize,
}

impl Task {
    pub fn binding(&self, key: &str) -> Option<&str> {
        self.bindings.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Success,
    Failure(i32),
    Interrupted,
}

impl ExitStatus {
    pub fn success(self) -> bool {
        self == ExitStatus::Success
    }
}

/// How a task was executed, for status reporting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExecKind {
    #[default]
    Local,
    Remote,
    RemoteCacheHit,
    /// Outputs restored from the local disk cache.
    DiskCacheHit,
    DryRun,
}

#[derive(Clone, Debug)]
pub struct TaskResult {
    pub edge: EdgeId,
    pub status: ExitStatus,
    /// Combined stdout/stderr.
    pub output: Vec<u8>,
    pub kind: ExecKind,
    /// Outputs a remote execution left in the CAS (for build events).
    pub cas_outputs: Vec<CasOutput>,
    /// Inputs (as in `Task::inputs`) the task couldn't read because they
    /// only existed remotely and are gone: the engine runs the edges
    /// producing them again, then the task (action rewinding).
    pub lost_inputs: Vec<String>,
}

/// An output file stored in a content-addressable store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CasOutput {
    /// As in `Task::outputs`.
    pub path: String,
    pub hash: String,
    pub size: i64,
}

pub type WaitFuture<'a> = Pin<Box<dyn Future<Output = Option<TaskResult>> + Send + 'a>>;

pub type AbortFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// What a remote backend did with a task.
pub enum RemoteOutcome {
    /// The task ran remotely (or was a cache hit); outputs are on disk.
    Done(TaskResult),
    /// The task can't or shouldn't run remotely after all (e.g. its command
    /// can't be turned into an argv, or the service failed and local
    /// fallback is allowed): run it locally. The string says why; the
    /// command, when the action was prepared, is the one to run.
    Fallback(String, Option<LocalCommand>),
    /// Run the task locally (with the command, when given), then give its
    /// result to the capture (e.g. to store its outputs in a cache).
    RunLocally(Box<dyn LocalCapture>, Option<LocalCommand>),
    /// Racing: the local branch claimed the outputs first; the remote
    /// branch stopped without writing anything.
    Lost,
}

/// Which branch of a raced task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

/// The command a local run of a prepared action uses: the remote action's,
/// so that it writes the bytes remote execution would (paths relative to
/// the working directory, which locally is the build directory, rather than
/// the absolute paths a build file may have). Results stored in caches and
/// the inputs of later actions then don't depend on the checkout.
#[derive(Clone)]
pub struct LocalCommand {
    pub argv: Vec<String>,
    pub dir: std::path::PathBuf,
    pub env: Vec<(String, String)>,
    /// Files to write first: response files as the remote action reads them.
    pub files: Vec<(std::path::PathBuf, Vec<u8>)>,
    /// After a successful run: maps what the command wrote with the remote
    /// action's paths (its depfile, `/showIncludes` lines) to local ones,
    /// as for a remote result.
    pub map_back: Option<Arc<dyn MapBack>>,
}

impl std::fmt::Debug for LocalCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCommand").field("argv", &self.argv).field("dir", &self.dir).finish_non_exhaustive()
    }
}

/// See `LocalCommand::map_back`.
pub trait MapBack: Send + Sync {
    fn map_back(&self, task: &Task, result: &mut TaskResult);
}

/// The state two racing branches share.
#[derive(Clone, Debug, Default)]
pub struct RaceState {
    /// The branch that claimed the task's outputs: only it writes them.
    pub winner: Option<Side>,
    /// The local command is running.
    pub local_running: bool,
    /// The remote branch gave up (a fallback): the local branch must run.
    pub local_needed: bool,
    /// The remote action's command, once prepared. Without it (the action
    /// couldn't be prepared), a local run uses the task's command.
    pub command: Option<Arc<LocalCommand>>,
}

/// A task run locally and remotely at once: whichever branch first needs
/// to write the task's outputs claims them, and the other stops. A remote
/// claim stops a local command already running (all its processes) and
/// waits until it has exited, so outputs are never written by both.
pub struct Race {
    state: tokio::sync::watch::Sender<RaceState>,
}

impl Race {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { state: tokio::sync::watch::Sender::new(RaceState::default()) })
    }

    pub fn state(&self) -> RaceState {
        self.state.borrow().clone()
    }

    /// Remote: the action is prepared; a local run uses its command.
    pub fn set_command(&self, command: LocalCommand) {
        self.state.send_modify(|s| s.command = Some(Arc::new(command)));
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<RaceState> {
        self.state.subscribe()
    }

    /// Remote: about to write outputs (or run steps on the host). False if
    /// the local branch won; otherwise, once a running local command has
    /// stopped.
    pub async fn claim_remote(&self) -> bool {
        let mut won = false;
        self.state.send_if_modified(|s| match s.winner {
            Some(Side::Local) => false,
            Some(Side::Remote) => {
                won = true;
                false
            }
            None => {
                s.winner = Some(Side::Remote);
                won = true;
                true
            }
        });
        if won {
            let _ = self.state.subscribe().wait_for(|s| !s.local_running).await;
        }
        won
    }

    /// Local: about to start the command. False if the remote branch won.
    pub fn start_local(&self) -> bool {
        let mut started = false;
        self.state.send_if_modified(|s| {
            started = s.winner.is_none();
            if started {
                s.local_running = true;
            }
            started
        });
        started
    }

    /// Local: the command finished on its own; claims its result. False if
    /// the remote branch claimed meanwhile (and waits for `local_stopped`).
    pub fn claim_local(&self) -> bool {
        let mut won = false;
        self.state.send_if_modified(|s| {
            won = s.winner != Some(Side::Remote);
            let changed = s.winner.is_none();
            if changed {
                s.winner = Some(Side::Local);
            }
            changed
        });
        won
    }

    /// Local: the command ended or was stopped.
    pub fn local_stopped(&self) {
        self.state.send_modify(|s| s.local_running = false);
    }

    /// Remote: it gave up, the local branch must run (not only when a local
    /// slot is free).
    pub fn need_local(&self) {
        self.state.send_modify(|s| s.local_needed = true);
    }
}

/// Receives the result of a task a backend had run locally.
pub trait LocalCapture: Send {
    /// Called after the command ran, before the engine reads its depfile.
    fn capture(self: Box<Self>, result: &TaskResult);
}

pub type RemoteFuture = Pin<Box<dyn Future<Output = RemoteOutcome> + Send>>;

/// Why a task's inputs couldn't be put on disk.
#[derive(Clone, Debug, Default)]
pub struct MaterializeError {
    pub message: String,
    /// Inputs gone from the remote store (see `TaskResult::lost_inputs`).
    pub lost: Vec<String>,
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for MaterializeError {
    fn from(message: String) -> Self {
        Self { message, lost: Vec::new() }
    }
}

/// Completes once a task's inputs are on disk; an error fails the task.
pub type MaterializeFuture = Pin<Box<dyn Future<Output = Result<(), MaterializeError>> + Send>>;

tokio::task_local! {
    static HOST_STOP: Arc<Stop>;
}

/// Runs `future`, a backend's (see [`RemoteBackend`]), for a runner whose
/// local commands `stop` stops: the steps the backend runs on the host
/// stop with them (see [`host_stop`]).
pub async fn with_host_stop<F: Future>(stop: Arc<Stop>, future: F) -> F::Output {
    HOST_STOP.scope(stop, future).await
}

/// What a backend runs commands on the host with (`subprocess::run_argv`),
/// so that they are stopped and waited for as the runner's local commands
/// are, when the build is interrupted or fails: the runner's stop, or
/// outside one a stop of their own.
pub fn host_stop() -> Arc<Stop> {
    HOST_STOP.try_with(Arc::clone).unwrap_or_default()
}

/// A remote execution backend (REAPI). Lives in its own crate; the engine
/// only knows this interface. Its futures run under [`with_host_stop`]:
/// commands it runs on the host use [`host_stop`].
pub trait RemoteBackend: Send + Sync {
    /// Whether the execution policy sends this task to the remote backend.
    fn wants(&self, task: &Task) -> bool;
    /// Executes the task remotely.
    fn execute(&self, task: Task) -> RemoteFuture;
    /// How many remote actions may be in flight.
    fn max_jobs(&self) -> usize;
    /// Whether it runs tasks remotely. A cache alone (a disk cache, a
    /// remote cache without remote execution) doesn't: it looks results up,
    /// and a miss runs on the host, in a local slot.
    fn executes(&self) -> bool {
        true
    }
    /// Before a task runs on the host: downloads its inputs that only exist
    /// remotely (outputs of remote actions that weren't downloaded).
    fn materialize(&self, _task: &Task) -> MaterializeFuture {
        Box::pin(async { Ok(()) })
    }
    /// `execute`, racing a local run of the task: the backend claims the
    /// outputs through `race` before writing anything, and answers
    /// `RemoteOutcome::Lost` when the local branch claimed them first.
    fn execute_raced(&self, task: Task, _race: Arc<Race>) -> RemoteFuture {
        self.execute(task)
    }
}

pub trait CommandRunner: Send {
    /// How many more tasks may be started now.
    fn can_run_more(&self) -> usize;
    /// Starts a task without blocking.
    fn start_command(&mut self, task: Task) -> Result<(), String>;
    /// Waits for the next task to finish. `None` if nothing is running.
    /// The engine also polls it once for a task finished already, and
    /// drops it if none is: a result must not be lost then.
    fn wait_for_command(&mut self) -> WaitFuture<'_>;
    fn active_edges(&self) -> Vec<EdgeId>;
    /// Stops everything running. Completes once the commands started have
    /// exited, so that none writes the outputs the engine deletes next.
    fn abort(&mut self) -> AbortFuture<'_>;
    /// Whether it reads `Task::inputs` (a remote backend does): the engine
    /// doesn't gather them otherwise.
    fn wants_inputs(&self) -> bool {
        true
    }
    /// How many kinds of slots it has (local ones, remote ones...): tasks
    /// of a kind run at most so many at once. One by default.
    fn slot_kinds(&self) -> usize {
        1
    }
    /// Which kind of slots `task` would take, below `slot_kinds`, from its
    /// edge's bindings (its inputs aren't gathered yet), for
    /// `has_free_slot`. Asked once per edge: the task started gets it as
    /// `Task::slots`.
    fn slots_of(&self, _task: &Task) -> usize {
        0
    }
    /// Whether a task taking slots of `kind` may start now. One whose slots
    /// are all taken waits in the plan, other kinds starting meanwhile,
    /// rather than in the runner, where it would count as started (and its
    /// wait in its times) and take room tasks of other kinds could use. A
    /// task waiting in the runner for slots of another kind than it started
    /// in (one sent remotely running locally after all) counts as taking
    /// those.
    fn has_free_slot(&self, _kind: usize) -> bool {
        true
    }
}

/// `-n`: pretends every command succeeded.
#[derive(Default)]
pub struct DryRunRunner {
    finished: std::collections::VecDeque<EdgeId>,
}

impl CommandRunner for DryRunRunner {
    fn can_run_more(&self) -> usize {
        usize::MAX
    }
    fn start_command(&mut self, task: Task) -> Result<(), String> {
        self.finished.push_back(task.edge);
        Ok(())
    }
    fn wait_for_command(&mut self) -> WaitFuture<'_> {
        let r = self.finished.pop_front().map(|edge| TaskResult {
            edge,
            status: ExitStatus::Success,
            output: Vec::new(),
            kind: ExecKind::DryRun,
            cas_outputs: Vec::new(),
            lost_inputs: Vec::new(),
        });
        Box::pin(async move { r })
    }
    fn active_edges(&self) -> Vec<EdgeId> {
        Vec::new()
    }
    fn abort(&mut self) -> AbortFuture<'_> {
        self.finished.clear();
        Box::pin(std::future::ready(()))
    }
    fn wants_inputs(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod race_tests {
    use super::*;

    #[tokio::test]
    async fn remote_claim_waits_for_the_local_command() {
        let race = Race::new();
        assert!(race.start_local());
        let r = race.clone();
        let claim = tokio::spawn(async move { r.claim_remote().await });
        // The local branch sees the remote claim, stops its command.
        let _ = race.subscribe().wait_for(|s| s.winner == Some(Side::Remote)).await;
        assert!(!claim.is_finished());
        assert!(!race.claim_local());
        race.local_stopped();
        assert!(claim.await.unwrap());
        // Too late for the local branch.
        assert!(!race.start_local());
    }

    #[tokio::test]
    async fn local_claim_wins() {
        let race = Race::new();
        assert!(race.start_local());
        assert!(race.claim_local());
        race.local_stopped();
        assert!(!race.claim_remote().await);
        assert_eq!(race.state().winner, Some(Side::Local));
    }
}
