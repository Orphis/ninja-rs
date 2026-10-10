//! The interface between the build engine and whatever runs commands
//! (local subprocesses, a remote executor, or a dry run).
//!
//! The engine never shares its graph with runners: it hands them a
//! self-contained [`Task`] and gets a [`TaskResult`] back.

use std::future::Future;
use std::pin::Pin;

use crate::graph::EdgeId;

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
    /// files that are gone.
    pub inputs: Vec<String>,
    pub restat: bool,
    pub generator: bool,
    /// Extra edge bindings requested through `BuildConfig::task_bindings`
    /// (e.g. `rbe_local`, `rbe_mnemonic`, `rbe_target`), evaluated; empty values
    /// are omitted.
    pub bindings: Vec<(String, String)>,
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

/// What a remote backend did with a task.
pub enum RemoteOutcome {
    /// The task ran remotely (or was a cache hit); outputs are on disk.
    Done(TaskResult),
    /// The task can't or shouldn't run remotely after all (e.g. its command
    /// can't be turned into an argv, or the service failed and local
    /// fallback is allowed): run it locally. The string says why.
    Fallback(String),
    /// Run the task locally, then give its result to the capture (e.g. to
    /// store its outputs in a cache).
    RunLocally(Box<dyn LocalCapture>),
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

/// A remote execution backend (REAPI). Lives in its own crate; the engine
/// only knows this interface.
pub trait RemoteBackend: Send + Sync {
    /// Whether the execution policy sends this task to the remote backend.
    fn wants(&self, task: &Task) -> bool;
    /// Executes the task remotely.
    fn execute(&self, task: Task) -> RemoteFuture;
    /// How many remote actions may be in flight.
    fn max_jobs(&self) -> usize;
    /// Before a task runs on the host: downloads its inputs that only exist
    /// remotely (outputs of remote actions that weren't downloaded).
    fn materialize(&self, _task: &Task) -> MaterializeFuture {
        Box::pin(async { Ok(()) })
    }
}

pub trait CommandRunner: Send {
    /// How many more tasks may be started now.
    fn can_run_more(&self) -> usize;
    /// Starts a task without blocking.
    fn start_command(&mut self, task: Task) -> Result<(), String>;
    /// Waits for the next task to finish. `None` if nothing is running.
    fn wait_for_command(&mut self) -> WaitFuture<'_>;
    fn active_edges(&self) -> Vec<EdgeId>;
    /// Kills everything running.
    fn abort(&mut self);
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
    fn abort(&mut self) {
        self.finished.clear();
    }
}
