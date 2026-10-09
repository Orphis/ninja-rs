//! The command runner used by the CLI: runs each task locally or through a
//! remote backend, according to the backend's policy.
//!
//! Local tasks are limited by `-j`, remote ones by `--remote_jobs`. Tasks
//! are started immediately and wait for a slot of their kind inside their
//! own tokio task, so the engine may have up to `-j + --remote_jobs` tasks
//! in flight.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ninja_core::graph::EdgeId;
use ninja_core::runner::{CommandRunner, ExecKind, ExitStatus, RemoteBackend, RemoteOutcome, Task, TaskResult, WaitFuture};
use ninja_core::subprocess::{Interrupt, run_local_command};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub struct ExecRunner {
    local_slots: Arc<Semaphore>,
    remote_slots: Arc<Semaphore>,
    capacity: usize,
    tasks: JoinSet<TaskResult>,
    ids: HashMap<tokio::task::Id, EdgeId>,
    active: HashSet<EdgeId>,
    remote: Option<Arc<dyn RemoteBackend>>,
    interrupt: Arc<Interrupt>,
    /// Messages about fallbacks, printed by the caller.
    pub verbose_remote: bool,
}

impl ExecRunner {
    pub fn new(parallelism: usize, remote: Option<Arc<dyn RemoteBackend>>, interrupt: Arc<Interrupt>) -> Self {
        let parallelism = parallelism.max(1);
        let remote_jobs = remote.as_ref().map(|r| r.max_jobs().max(1)).unwrap_or(0);
        Self {
            local_slots: Arc::new(Semaphore::new(parallelism.min(Semaphore::MAX_PERMITS))),
            remote_slots: Arc::new(Semaphore::new(remote_jobs.max(1))),
            capacity: parallelism.saturating_add(remote_jobs),
            tasks: JoinSet::new(),
            ids: HashMap::new(),
            active: HashSet::new(),
            remote,
            interrupt,
            verbose_remote: false,
        }
    }
}

async fn run_local(task: &Task, slots: &Semaphore) -> TaskResult {
    let _permit = slots.acquire().await;
    let (status, output) = run_local_command(&task.command, task.use_console).await;
    TaskResult { edge: task.edge, status, output, kind: ExecKind::Local, cas_outputs: Vec::new() }
}

impl CommandRunner for ExecRunner {
    fn can_run_more(&self) -> usize {
        self.capacity.saturating_sub(self.active.len())
    }

    fn start_command(&mut self, task: Task) -> Result<(), String> {
        let edge = task.edge;
        self.active.insert(edge);
        let local_slots = self.local_slots.clone();
        let remote = self.remote.clone().filter(|r| !task.use_console && r.wants(&task));
        let verbose = self.verbose_remote;
        match remote {
            None => {
                let h = self.tasks.spawn(async move { run_local(&task, &local_slots).await });
                self.ids.insert(h.id(), edge);
            }
            Some(backend) => {
                let remote_slots = self.remote_slots.clone();
                let h = self.tasks.spawn(async move {
                    let outcome = {
                        let _permit = remote_slots.acquire().await;
                        backend.execute(task.clone()).await
                    };
                    match outcome {
                        RemoteOutcome::Done(r) => r,
                        RemoteOutcome::Fallback(reason) => {
                            let mut r = run_local(&task, &local_slots).await;
                            if verbose {
                                let mut msg = format!("ninja-rs: running locally ({reason})\n").into_bytes();
                                msg.extend_from_slice(&r.output);
                                r.output = msg;
                            }
                            r
                        }
                        RemoteOutcome::RunLocally(capture) => {
                            let r = run_local(&task, &local_slots).await;
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
                    return Some(TaskResult { edge, status: ExitStatus::Interrupted, output: Vec::new(), kind: ExecKind::Local, cas_outputs: Vec::new() });
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
                    })
                }
            }
        })
    }

    fn active_edges(&self) -> Vec<EdgeId> {
        self.active.iter().copied().collect()
    }

    fn abort(&mut self) {
        self.tasks.abort_all();
        self.ids.clear();
        self.active.clear();
    }
}
