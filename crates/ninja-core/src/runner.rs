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
    /// Explicit and implicit inputs (no order-only), canonical paths.
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
    DryRun,
}

#[derive(Clone, Debug)]
pub struct TaskResult {
    pub edge: EdgeId,
    pub status: ExitStatus,
    /// Combined stdout/stderr.
    pub output: Vec<u8>,
    pub kind: ExecKind,
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
}

pub type RemoteFuture = Pin<Box<dyn Future<Output = RemoteOutcome> + Send>>;

/// A remote execution backend (REAPI). Lives in its own crate; the engine
/// only knows this interface.
pub trait RemoteBackend: Send + Sync {
    /// Whether the execution policy sends this task to the remote backend.
    fn wants(&self, task: &Task) -> bool;
    /// Executes the task remotely.
    fn execute(&self, task: Task) -> RemoteFuture;
    /// How many remote actions may be in flight.
    fn max_jobs(&self) -> usize;
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
