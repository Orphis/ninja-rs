//! The REAPI backend: execution policy, action cache lookups (the local
//! disk cache, then the remote one), upload, Execute, and output download
//! (with depfile and /showIncludes mapping). With only a disk cache, cache
//! misses run locally and their results are stored.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ninja_core::clparser::{DEFAULT_DEPS_PREFIX, filter_show_includes};
use ninja_core::depfile::parse_depfile;
use ninja_core::runner::{
    CasOutput, ExecKind, ExitStatus, LocalCapture, LocalCommand, MapBack, MaterializeError, MaterializeFuture, Race, RemoteBackend, RemoteFuture,
    RemoteOutcome, Task, TaskResult,
};
use rbe_client::proto as re;
use rbe_client::{ActionMeta, Client};

use crate::action::{Prepared, Shared, prepare};
use crate::disk_cache::DiskCache;
use crate::remote_outputs::{Entry, RemoteOutputs};
use crate::store::Store;
use crate::targets::Targets;
use crate::toolchains::Toolchains;

#[derive(Clone, Debug)]
pub struct BackendConfig {
    /// Absolute build directory (where build.ninja is).
    pub build_dir: PathBuf,
    /// `rbe_toolchains`, relative to the build directory.
    pub toolchains_file: Option<String>,
    /// The `rbe_targets` file's contents: labels and configurations for the
    /// actions' request metadata.
    pub targets: Arc<Targets>,
    pub exec_root: Option<PathBuf>,
    pub jobs: usize,
    pub mnemonic_suffixes: Vec<String>,
    pub local_fallback: bool,
    /// Actions the policy sends remotely run there or fail: no local run
    /// for one that can't be sent, nor after a remote failure.
    pub strict: bool,
    /// Local runs after remote failures allowed before they fail instead.
    pub max_fallbacks: Option<usize>,
    /// Largest share of remote actions that may run locally after remote
    /// failures, checked from `FALLBACK_RATIO_MIN_ACTIONS` actions on.
    pub max_fallback_ratio: Option<f64>,
    /// The remote service is a cache without execution: a miss runs the
    /// command locally.
    pub cache_only: bool,
    /// With `cache_only`, store the results of commands run locally there.
    pub upload_local_results: bool,
    pub verbose: bool,
    /// Outputs of remote actions that weren't downloaded: remote actions
    /// read them by digest, local commands get them downloaded first.
    pub remote_outputs: Option<Arc<RemoteOutputs>>,
    /// `--remote_download_outputs=minimal`: outputs of remote actions are
    /// recorded in `remote_outputs` instead of downloaded, but for
    /// deliverables (`rbe_deliverable`), depfiles and `requested`.
    pub download_minimal: bool,
    /// Paths the command line names, as `remote_outputs::key`s.
    pub requested: Arc<std::collections::HashSet<String>>,
    /// `--execution_log_file`: one JSON line per remote action (key,
    /// command, inputs), to compare the actions of two builds.
    pub execution_log: Option<Arc<std::sync::Mutex<std::fs::File>>>,
    /// `--remote_include_scanner`: how compiles' inputs are found (the
    /// conservative scanner when the chosen one can't tell).
    pub include_scanner: crate::action::IncludeScanner,
}

pub struct Backend {
    inner: Arc<Inner>,
}

struct Inner {
    /// The remote service; without it, the disk cache alone.
    client: Option<Client>,
    /// Results of commands run locally on a cache miss go to the remote
    /// cache (`cache_only`, when the server accepts them).
    upload: bool,
    disk: Option<Arc<DiskCache>>,
    shared: Shared,
    config: BackendConfig,
    /// Held while downloading remote-only inputs, so that two tasks
    /// reading the same file don't both write it.
    materializing: tokio::sync::Mutex<()>,
    fallbacks: Fallbacks,
}

/// Remote actions that ran locally instead, with the first reason of each
/// kind.
#[derive(Default)]
struct Fallbacks {
    /// Actions the policy sent remotely.
    started: std::sync::atomic::AtomicUsize,
    /// Actions that couldn't be sent (`prepare` refused them).
    unsent: std::sync::Mutex<(usize, Option<String>)>,
    /// Actions run locally after a remote failure.
    failed: std::sync::Mutex<(usize, Option<String>)>,
    /// Why remote failures no longer fall back, once a limit is crossed.
    stopped: std::sync::Mutex<Option<String>>,
}

/// `--remote_max_fallback_ratio` applies from this many remote actions on,
/// so that a failure among the first few doesn't stop a build.
pub const FALLBACK_RATIO_MIN_ACTIONS: usize = 20;

/// Why the `count`th local run after a remote failure, out of `started`
/// remote actions, exceeds the limits, if it does.
fn fallback_limit(count: usize, started: usize, max: Option<usize>, ratio: Option<f64>) -> Option<String> {
    if let Some(max) = max
        && count > max
    {
        let actions = if max == 1 { "action already ran locally after a remote failure" } else { "actions already ran locally after remote failures" };
        return Some(format!("{max} {actions} (--remote_max_fallbacks={max})"));
    }
    if let Some(r) = ratio
        && started >= FALLBACK_RATIO_MIN_ACTIONS
        && count as f64 > r * started as f64
    {
        return Some(format!("{count} of {started} remote actions failed remotely (--remote_max_fallback_ratio={r})"));
    }
    None
}

/// The first line of a reason, for summaries.
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().trim().to_string()
}

impl Backend {
    /// A backend with a remote service, a disk cache, or both.
    pub async fn new(config: BackendConfig, client: Option<Client>, disk: Option<Arc<DiskCache>>, store: Arc<Store>) -> Result<Self> {
        let windows = cfg!(windows);
        let toolchains = match &config.toolchains_file {
            Some(f) => Toolchains::load(&config.build_dir.join(f), windows).map_err(anyhow::Error::msg)?,
            None => Toolchains::default(),
        };
        // The capabilities also configure the client (batch sizes,
        // compression, split and spliced blobs): keep that one.
        let mut client = client;
        let mut upload = false;
        if let Some(client) = &mut client {
            let caps = client.capabilities().await.context("GetCapabilities")?;
            if let Some(cc) = &caps.cache_capabilities
                && !cc.digest_functions.is_empty()
                && !cc.digest_functions.contains(&(re::digest_function::Value::Sha256 as i32))
            {
                bail!("the remote cache does not support SHA-256");
            }
            if config.cache_only {
                let updates = caps.cache_capabilities.as_ref().and_then(|c| c.action_cache_update_capabilities.as_ref()).is_some_and(|u| u.update_enabled);
                upload = config.upload_local_results && updates;
            } else if caps.execution_capabilities.as_ref().is_some_and(|e| !e.exec_enabled) {
                bail!("the remote instance does not support execution");
            }
        }
        let shared = Shared {
            build_dir: config.build_dir.clone(),
            toolchains,
            exec_root: config.exec_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            store,
            windows,
            remote_outputs: config.remote_outputs.clone(),
            log_inputs: config.execution_log.is_some(),
            include_scanner: config.include_scanner,
            verbose: config.verbose,
        };
        shared.store.new_build();
        Ok(Self { inner: Arc::new(Inner { client, upload, disk, shared, config, materializing: Default::default(), fallbacks: Fallbacks::default() }) })
    }

    /// Whether results of commands run locally go to the remote cache.
    pub fn uploads_local_results(&self) -> bool {
        self.inner.upload
    }

    /// How many remote actions ran locally instead, and why: a line for
    /// the end of the build, and whether some did after remote failures.
    pub fn fallback_summary(&self) -> Option<(String, bool)> {
        let f = &self.inner.fallbacks;
        let (unsent, unsent_reason) = f.unsent.lock().unwrap().clone();
        let (failed, failed_reason) = f.failed.lock().unwrap().clone();
        if unsent + failed == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if unsent > 0 {
            parts.push(format!("{unsent} couldn't be sent (first: {})", unsent_reason.unwrap_or_default()));
        }
        if failed > 0 {
            parts.push(format!("{failed} after remote failures (first: {})", failed_reason.unwrap_or_default()));
        }
        let started = f.started.load(std::sync::atomic::Ordering::Relaxed);
        Some((format!("{} of {started} remote actions ran locally: {}.", unsent + failed, parts.join("; ")), failed > 0))
    }
}

/// The phase-1 policy: a task runs remotely if it is not marked
/// `rbe_local = 1`, does not regenerate the manifest (`generator = 1`), and
/// its mnemonic ends with one of the allowed suffixes (`Compile`).
pub fn policy_wants(task: &Task, suffixes: &[String]) -> bool {
    if task.use_console || task.generator || task.binding("rbe_local") == Some("1") {
        return false;
    }
    match task.binding("rbe_mnemonic") {
        Some(m) => suffixes.iter().any(|s| m.ends_with(s.as_str())),
        None => false,
    }
}

impl RemoteBackend for Backend {
    fn wants(&self, task: &Task) -> bool {
        policy_wants(task, &self.inner.config.mnemonic_suffixes)
    }

    fn execute(&self, task: Task) -> RemoteFuture {
        let inner = self.inner.clone();
        Box::pin(async move { inner.execute(task, None).await })
    }

    fn execute_raced(&self, task: Task, race: Arc<Race>) -> RemoteFuture {
        let inner = self.inner.clone();
        Box::pin(async move { inner.execute(task, Some(race)).await })
    }

    fn max_jobs(&self) -> usize {
        self.inner.config.jobs
    }

    fn materialize(&self, task: &Task) -> MaterializeFuture {
        let inner = self.inner.clone();
        let paths = task.inputs.clone();
        Box::pin(async move { inner.materialize_paths(&paths).await })
    }
}

impl Backend {
    /// Downloads those of `paths` (relative to the build directory) that
    /// only exist remotely, giving them their recorded time.
    pub async fn materialize_paths(&self, paths: &[String]) -> Result<(), MaterializeError> {
        self.inner.materialize_paths(paths).await
    }

    /// Drops the records of remote-only outputs the remote CAS no longer
    /// has, so that the build re-runs the actions producing them. Returns
    /// how many were dropped.
    pub async fn forget_evicted_outputs(&self) -> Result<usize> {
        let (Some(client), Some(outputs)) = (&self.inner.client, &self.inner.config.remote_outputs) else { return Ok(0) };
        let entries = outputs.entries();
        if entries.is_empty() {
            return Ok(0);
        }
        let missing: std::collections::HashSet<String> =
            client.find_missing(entries.iter().map(|e| e.digest.clone()).collect()).await?.into_iter().map(|d| d.hash).collect();
        let mut dropped = 0;
        for e in entries {
            if missing.contains(&e.digest.hash) {
                outputs.forget(&e.path);
                dropped += 1;
            }
        }
        Ok(dropped)
    }
}

enum Failure {
    /// The action could not be prepared: run locally.
    Prepare(String),
    /// The remote service failed.
    Infra(anyhow::Error),
    /// A cache miss without remote execution (the disk cache alone, or a
    /// remote cache): run locally, then store the result.
    RunLocally(Box<Prepared>),
    /// Racing: the local branch claimed the outputs first.
    Lost,
}

/// The command a local run of the action uses (see `LocalCommand`): the
/// remote action's. None for a command with steps on the host, which runs
/// as the build file has it.
fn local_command(p: &Prepared, build_dir: &std::path::Path) -> Option<LocalCommand> {
    if !p.local_before.is_empty() || !p.local_after.is_empty() {
        return None;
    }
    Some(LocalCommand {
        argv: p.command.arguments.clone(),
        dir: PathBuf::from(&p.mapper.build_dir),
        env: p.command.environment_variables.iter().map(|e| (e.name.clone(), e.value.clone())).collect(),
        files: p.rsp_files.clone(),
        map_back: Some(Arc::new(LocalMapBack { mapper: p.mapper.clone(), tree_index: p.tree_index.clone(), build_dir: build_dir.to_path_buf() })),
    })
}

/// Maps a local run of the remote command back, as a download does.
struct LocalMapBack {
    mapper: crate::paths::PathMapper,
    tree_index: crate::paths::TreeIndex,
    build_dir: PathBuf,
}

impl MapBack for LocalMapBack {
    fn map_back(&self, task: &Task, result: &mut TaskResult) {
        if let Some(depfile) = &task.depfile
            && let Ok(data) = std::fs::read(depfile)
        {
            let _ = std::fs::write(depfile, map_depfile(&data, &self.mapper, &self.tree_index));
        }
        if task.deps.as_deref() == Some("msvc") {
            result.output = map_show_includes(&result.output, task, &self.mapper, &self.tree_index, &self.build_dir);
        }
    }
}

/// A depfile in remote form (paths of the remote action) with local paths.
fn map_depfile(data: &[u8], mapper: &crate::paths::PathMapper, tree_index: &crate::paths::TreeIndex) -> Vec<u8> {
    let Ok(d) = parse_depfile(data) else { return data.to_vec() };
    let map = |s: &str| mapper.map_back(s, tree_index);
    let mut out = String::new();
    let outs: Vec<String> = d.outs.iter().map(|o| map(o)).collect();
    out.push_str(&outs.iter().map(|o| escape_make(o)).collect::<Vec<_>>().join(" "));
    out.push(':');
    for i in &d.ins {
        out.push_str(" \\\n  ");
        out.push_str(&escape_make(&map(i)));
    }
    out.push('\n');
    out.into_bytes()
}

/// `/showIncludes` lines in remote form with local paths.
fn map_show_includes(
    stdout: &[u8],
    task: &Task,
    mapper: &crate::paths::PathMapper,
    tree_index: &crate::paths::TreeIndex,
    build_dir: &std::path::Path,
) -> Vec<u8> {
    let prefix = if task.msvc_deps_prefix.is_empty() { DEFAULT_DEPS_PREFIX } else { task.msvc_deps_prefix.as_str() };
    let text = String::from_utf8_lossy(stdout);
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        match filter_show_includes(body, prefix) {
            Some(path) if !path.is_empty() => {
                let mapped = mapper.map_back(path, tree_index);
                let mapped = if !crate::paths::is_abs(&mapped, cfg!(windows)) {
                    // Relative to the build dir: make it absolute so that
                    // ninja's normalization sees what a local cl.exe prints.
                    crate::scanner::normalize(&build_dir.join(&mapped)).to_string_lossy().into_owned()
                } else {
                    mapped
                };
                out.push_str(prefix);
                out.push(' ');
                out.push_str(&mapped);
                out.push_str(ending);
            }
            _ => out.push_str(line),
        }
    }
    out.into_bytes()
}

/// Racing: claims the task's outputs for the remote branch, before it
/// writes anything on the host.
async fn claim(race: Option<&Arc<Race>>) -> Result<(), Failure> {
    match race {
        Some(r) if !r.claim_remote().await => Err(Failure::Lost),
        _ => Ok(()),
    }
}

impl Inner {
    async fn execute(self: Arc<Self>, task: Task, race: Option<Arc<Race>>) -> RemoteOutcome {
        self.fallbacks.started.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let failed = |output: String| {
            RemoteOutcome::Done(TaskResult {
                edge: task.edge,
                status: ExitStatus::Failure(1),
                output: output.into_bytes(),
                kind: ExecKind::Remote,
                cas_outputs: Vec::new(),
                lost_inputs: Vec::new(),
            })
        };
        let mut command = None;
        match self.clone().try_execute(&task, race.as_ref(), &mut command).await {
            Ok(r) => RemoteOutcome::Done(r),
            Err(Failure::Lost) => RemoteOutcome::Lost,
            Err(Failure::Prepare(reason)) => {
                if self.config.strict {
                    return failed(format!("ninja-rs: the action can't run remotely (--remote_strict): {reason}\n"));
                }
                let mut unsent = self.fallbacks.unsent.lock().unwrap();
                unsent.0 += 1;
                unsent.1.get_or_insert_with(|| first_line(&reason));
                drop(unsent);
                RemoteOutcome::Fallback(reason, None)
            }
            Err(Failure::RunLocally(prepared)) => {
                let command = local_command(&prepared, &self.shared.build_dir);
                RemoteOutcome::RunLocally(Box::new(Capture { inner: self, prepared, task }), command)
            }
            Err(Failure::Infra(e)) => {
                let reason = format!("remote execution failed: {e:#}");
                if !self.config.local_fallback || self.config.strict {
                    return failed(format!("ninja-rs: {reason}\n"));
                }
                match self.allow_fallback(&reason) {
                    Ok(()) => RemoteOutcome::Fallback(reason, command),
                    Err(why) => failed(format!("ninja-rs: {reason}\nninja-rs: not running it locally: {why}\n")),
                }
            }
        }
    }

    /// Counts a local run after a remote failure, unless it crosses a
    /// limit: from then on, remote failures fail their action.
    fn allow_fallback(&self, reason: &str) -> Result<(), String> {
        let mut stopped = self.fallbacks.stopped.lock().unwrap();
        if let Some(why) = &*stopped {
            return Err(why.clone());
        }
        let mut failed = self.fallbacks.failed.lock().unwrap();
        let started = self.fallbacks.started.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(why) = fallback_limit(failed.0 + 1, started, self.config.max_fallbacks, self.config.max_fallback_ratio) {
            *stopped = Some(why.clone());
            return Err(why);
        }
        failed.0 += 1;
        failed.1.get_or_insert_with(|| first_line(reason));
        Ok(())
    }

    /// Runs the task's action. `command` gets the command a local run would
    /// use, once the action is prepared.
    async fn try_execute(self: Arc<Self>, task: &Task, race: Option<&Arc<Race>>, command: &mut Option<LocalCommand>) -> Result<TaskResult, Failure> {
        let me = self.clone();
        let t = task.clone();
        let prepared = tokio::task::spawn_blocking(move || prepare(&t, &me.shared))
            .await
            .map_err(|e| Failure::Prepare(format!("internal error: {e}")))?
            .map_err(Failure::Prepare)?;
        let meta = self.action_meta(task);
        // A local run uses the remote action's command, for the same outputs.
        // A command with steps on the host isn't raced (see below).
        *command = local_command(&prepared, &self.shared.build_dir);
        if let (Some(race), Some(c)) = (race, command.clone()) {
            race.set_command(c);
        }
        let digest = prepared.action_digest.clone();
        if let Some(log) = &self.config.execution_log {
            log_action(log, task, &meta, &prepared);
        }

        // The disk cache first.
        let disk_hit = match &self.disk {
            Some(disk) => {
                let (disk, d) = (disk.clone(), digest.clone());
                tokio::task::spawn_blocking(move || disk.get_action_result(&d)).await.ok().flatten().filter(|r| r.exit_code == 0)
            }
            None => None,
        };
        if let Some(result) = disk_hit {
            claim(race).await?;
            let mut output = Vec::new();
            if let Some(failed) = self.run_local_steps(task, &prepared.local_before, &mut output).await {
                return Ok(failed);
            }
            output.extend(self.download(None, task, &prepared, &result, false).await.map_err(Failure::Infra)?);
            if let Some(mut failed) = self.run_local_steps(task, &prepared.local_after, &mut output).await {
                failed.kind = ExecKind::DiskCacheHit;
                return Ok(failed);
            }
            if self.config.verbose {
                let mut msg = format!("ninja-rs: disk cache hit {}\n", digest.hash).into_bytes();
                msg.extend_from_slice(&output);
                output = msg;
            }
            return Ok(TaskResult {
                edge: task.edge,
                status: ExitStatus::Success,
                output,
                kind: ExecKind::DiskCacheHit,
                cas_outputs: Vec::new(),
                lost_inputs: Vec::new(),
            });
        }
        let Some(client) = &self.client else { return Err(Failure::RunLocally(Box::new(prepared))) };
        let client = client.for_action(&meta, &prepared.action_digest.hash);

        // Then the remote action cache. With minimal downloads, a hit whose
        // outputs left the CAS is a miss: they wouldn't be downloaded now,
        // and rewinding would only find the same result again.
        let minimal = self.config.download_minimal;
        let mut cached = client.get_action_result(&digest).await.map_err(Failure::Infra)?.filter(|r| r.exit_code == 0);
        let mut stale = false;
        if minimal
            && let Some(r) = &cached
            && !outputs_in_cas(&client, r).await.map_err(Failure::Infra)?
        {
            cached = None;
            stale = true;
        }
        // Without remote execution, a miss runs locally.
        if self.config.cache_only && cached.is_none() {
            return Err(Failure::RunLocally(Box::new(prepared)));
        }

        // Local steps (e.g. deleting an archive before re-creating it). They
        // write on the host: a command with some isn't raced.
        if !prepared.local_before.is_empty() || !prepared.local_after.is_empty() {
            claim(race).await?;
        }
        let mut output = Vec::new();
        if let Some(failed) = self.run_local_steps(task, &prepared.local_before, &mut output).await {
            return Ok(failed);
        }
        let (result, hit) = match cached {
            Some(r) => (r, true),
            None => {
                let to_upload = self.shared.store.filter_unknown(prepared_blobs(&prepared));
                if let Err(e) = client.upload_blobs(&to_upload).await {
                    // Inputs that only existed remotely are gone: rewind.
                    if let Some(missing) = e.downcast_ref::<rbe_client::MissingRemoteBlobs>() {
                        let lost: Vec<String> = prepared.remote_inputs.iter().filter(|(p, _, _)| missing.0.contains(p)).map(|(_, i, _)| i.clone()).collect();
                        if let Some(outputs) = &self.config.remote_outputs {
                            for i in &lost {
                                outputs.forget(i);
                            }
                        }
                        return Ok(lost_inputs_result(task, lost, output));
                    }
                    return Err(Failure::Infra(e));
                }
                self.shared.store.mark_uploaded(to_upload.0.keys());
                let mut resp = client.execute(&digest, stale).await.map_err(Failure::Infra)?;
                // FAILED_PRECONDITION: inputs missing from the CAS, which may
                // be remote-only ones believed uploaded.
                if resp.status.as_ref().is_some_and(|st| st.code == tonic_code::FAILED_PRECONDITION) {
                    if !prepared.remote_inputs.is_empty() {
                        let inputs = prepared.remote_inputs.iter().map(|(_, i, d)| (i.clone(), d.clone())).collect();
                        let lost = self.lost_inputs(inputs).await;
                        if !lost.is_empty() {
                            return Ok(lost_inputs_result(task, lost, output));
                        }
                    }
                    // Blobs believed uploaded were evicted: upload again, once.
                    let all = prepared_blobs(&prepared);
                    self.shared.store.forget_uploaded(all.0.keys());
                    client.upload_blobs(&all).await.map_err(Failure::Infra)?;
                    self.shared.store.mark_uploaded(all.0.keys());
                    resp = client.execute(&digest, stale).await.map_err(Failure::Infra)?;
                }
                if let Some(st) = &resp.status
                    && st.code != 0
                {
                    return Err(Failure::Infra(anyhow::anyhow!("Execute returned status {}: {}", st.code, st.message)));
                }
                // A cached result from Execute gets the same check.
                if minimal
                    && resp.cached_result
                    && let Some(r) = &resp.result
                    && !outputs_in_cas(&client, r).await.map_err(Failure::Infra)?
                {
                    resp = client.execute(&digest, true).await.map_err(Failure::Infra)?;
                    if let Some(st) = &resp.status
                        && st.code != 0
                    {
                        return Err(Failure::Infra(anyhow::anyhow!("Execute returned status {}: {}", st.code, st.message)));
                    }
                }
                let hit = resp.cached_result;
                (resp.result.ok_or_else(|| Failure::Infra(anyhow::anyhow!("Execute returned no result")))?, hit)
            }
        };
        let keep = self.disk.is_some() && result.exit_code == 0;
        claim(race).await?;
        output.extend(self.download(Some(&client), task, &prepared, &result, keep).await.map_err(Failure::Infra)?);
        let kind = if hit { ExecKind::RemoteCacheHit } else { ExecKind::Remote };
        if result.exit_code == 0
            && let Some(mut failed) = self.run_local_steps(task, &prepared.local_after, &mut output).await
        {
            failed.kind = kind;
            return Ok(failed);
        }
        if self.config.verbose {
            let target = match task.binding("rbe_target") {
                Some(name) => match self.config.targets.get(name) {
                    Some(t) if !t.kind.is_empty() => format!(", {} {name}", t.kind),
                    _ => format!(", target {name}"),
                },
                None => String::new(),
            };
            let mut msg =
                format!("ninja-rs: {} {} ({} inputs{target})\n", if hit { "remote cache hit" } else { "executed remotely" }, digest.hash, prepared.input_files)
                    .into_bytes();
            for w in &prepared.warnings {
                msg.extend_from_slice(format!("ninja-rs: warning: {w}\n").as_bytes());
            }
            msg.extend_from_slice(&output);
            output = msg;
        }
        let status = if result.exit_code == 0 { ExitStatus::Success } else { ExitStatus::Failure(result.exit_code) };
        // The outputs are in the CAS now: build events can point there.
        let cas_outputs = result
            .output_files
            .iter()
            .filter_map(|f| {
                let i = prepared.outputs.iter().position(|(rel, _)| *rel == f.path)?;
                let d = f.digest.as_ref()?;
                Some(CasOutput { path: task.outputs.get(i)?.clone(), hash: d.hash.clone(), size: d.size_bytes })
            })
            .collect();
        Ok(TaskResult { edge: task.edge, status, output, kind, cas_outputs, lost_inputs: Vec::new() })
    }

    /// The request metadata of `task`'s action.
    fn action_meta(&self, task: &Task) -> ActionMeta {
        ActionMeta {
            mnemonic: task.binding("rbe_mnemonic").unwrap_or_default().to_string(),
            target_id: task.binding("rbe_target").map(|t| self.config.targets.label(t)).unwrap_or_default(),
            configuration_id: self.config.targets.configuration(task.binding("rbe_config")).map(|c| c.id()).unwrap_or_default(),
        }
    }

    /// Runs steps on the host, in the build directory, appending their
    /// output. Returns the result of the task if a step fails.
    async fn run_local_steps(&self, task: &Task, steps: &[Vec<String>], output: &mut Vec<u8>) -> Option<TaskResult> {
        for argv in steps {
            let program = argv[0].clone();
            let args = argv[1..].to_vec();
            let dir = self.shared.build_dir.clone();
            let r = tokio::task::spawn_blocking(move || std::process::Command::new(program).args(args).current_dir(dir).output()).await;
            let code = match r {
                Ok(Ok(out)) => {
                    output.extend_from_slice(&out.stdout);
                    output.extend_from_slice(&out.stderr);
                    if out.status.success() {
                        continue;
                    }
                    out.status.code().unwrap_or(1)
                }
                Ok(Err(e)) => {
                    output.extend_from_slice(format!("ninja-rs: running {}: {e}\n", argv[0]).as_bytes());
                    1
                }
                Err(e) => {
                    output.extend_from_slice(format!("ninja-rs: internal error: {e}\n").as_bytes());
                    1
                }
            };
            return Some(TaskResult {
                edge: task.edge,
                status: ExitStatus::Failure(code),
                output: std::mem::take(output),
                kind: ExecKind::Remote,
                cas_outputs: Vec::new(),
                lost_inputs: Vec::new(),
            });
        }
        None
    }

    /// Writes the outputs of an action result, from the disk cache or the
    /// remote one (skipping files that are already up to date, so that
    /// `restat` works) and returns the combined stdout/stderr. With `keep`,
    /// the result and its outputs are stored in the disk cache.
    async fn download(&self, client: Option<&Client>, task: &Task, p: &Prepared, result: &re::ActionResult, keep: bool) -> Result<Vec<u8>> {
        let (stdout, stderr) = match client {
            Some(c) => (
                c.output_bytes(&result.stdout_raw, result.stdout_digest.as_ref()).await?,
                c.output_bytes(&result.stderr_raw, result.stderr_digest.as_ref()).await?,
            ),
            // Disk cache entries hold their output inline.
            None => (result.stdout_raw.clone(), result.stderr_raw.clone()),
        };
        let mut files: Vec<(PathBuf, re::OutputFile)> = Vec::new();
        for f in &result.output_files {
            let local = self.shared.build_dir.join(&f.path);
            files.push((local, f.clone()));
        }
        // Output directories (unusual for compiles).
        for d in &result.output_directories {
            let Some(client) = client else { bail!("output directories need a remote cache") };
            let tree_digest = d.tree_digest.clone().unwrap_or_default();
            let tree = client.fetch_tree(&tree_digest).await?;
            for (rel, f) in rbe_client::tree::tree_files(&tree)? {
                let local = self.shared.build_dir.join(&d.path).join(rel);
                files.push((
                    local,
                    re::OutputFile { path: String::new(), digest: f.digest, is_executable: f.is_executable, contents: vec![], node_properties: None },
                ));
            }
        }
        let depfile_local = p.depfile.as_ref().map(|(_, l)| l.clone());
        // In minimal mode a remote action's outputs stay in the remote CAS,
        // recorded, unless they are deliverables, named on the command line,
        // or the depfile (which the build reads).
        let minimal = self.config.download_minimal && client.is_some() && result.output_directories.is_empty();
        let deliverable = task.binding("rbe_deliverable") == Some("1");
        let may_stay_remote = |local: &PathBuf, f: &re::OutputFile| {
            minimal && !deliverable && Some(local) != depfile_local.as_ref() && !self.config.requested.contains(&crate::remote_outputs::key(&f.path))
        };
        // A thin archive left remote is recorded with its members, read with
        // ranged reads, so that the actions reading it get them without
        // downloading it. When that fails, it's downloaded. Archives are
        // recognized by their archiver's flags, or else by their name.
        let thin_command = crate::archive::creates_thin(&p.command.arguments);
        let mut thin: std::collections::HashMap<String, Arc<Vec<String>>> = Default::default();
        let mut download_anyway: std::collections::HashSet<String> = Default::default();
        if let Some(c) = client {
            for (local, f) in &files {
                let d = f.digest.clone().unwrap_or_default();
                let named_like_archive = [".a", ".lib"].iter().any(|ext| f.path.to_ascii_lowercase().ends_with(ext));
                if !may_stay_remote(local, f) || !(thin_command || named_like_archive) || same_content(local, &d, &self.shared.store) {
                    continue;
                }
                match self.remote_thin_members(c, &d).await {
                    Ok(Some(members)) => {
                        thin.insert(f.path.clone(), Arc::new(members));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if self.config.verbose {
                            eprintln!("ninja-rs: reading the members of {}: {e:#}; downloading it", f.path);
                        }
                        download_anyway.insert(f.path.clone());
                    }
                }
            }
        }
        let stays_remote = |local: &PathBuf, f: &re::OutputFile| may_stay_remote(local, f) && !download_anyway.contains(&f.path);
        let mut needed = Vec::new();
        for (local, f) in &files {
            let d = f.digest.clone().unwrap_or_default();
            if Some(local) != depfile_local.as_ref() && same_content(local, &d, &self.shared.store) {
                continue;
            }
            if stays_remote(local, f) {
                continue;
            }
            if f.contents.is_empty() || d.size_bytes == 0 {
                needed.push(d);
            }
        }
        let blobs = self.fetch_blobs(client, &needed).await?;
        let mut left_remote = false;
        for (local, f) in &files {
            let d = f.digest.clone().unwrap_or_default();
            let is_depfile = Some(local) == depfile_local.as_ref();
            if !is_depfile && same_content(local, &d, &self.shared.store) {
                if let Some(outputs) = &self.config.remote_outputs {
                    outputs.forget(&f.path);
                }
                continue;
            }
            if stays_remote(local, f) {
                // A stale local copy must not be what local commands read.
                if local.exists() {
                    std::fs::remove_file(local).with_context(|| format!("removing stale {}", local.display()))?;
                }
                if let Some(outputs) = &self.config.remote_outputs {
                    outputs.record(Entry {
                        path: f.path.clone(),
                        digest: d,
                        executable: f.is_executable,
                        mtime: ninja_core::disk::now_ns(),
                        members: thin.get(&f.path).cloned(),
                    });
                }
                left_remote = true;
                continue;
            }
            if let Some(outputs) = &self.config.remote_outputs {
                outputs.forget(&f.path);
            }
            let data: Vec<u8> = if !f.contents.is_empty() {
                f.contents.clone()
            } else if d.size_bytes == 0 {
                Vec::new()
            } else {
                blobs.get(&d.hash).cloned().with_context(|| format!("missing blob for {}", local.display()))?
            };
            let data = if is_depfile { self.map_depfile(&data, p) } else { data };
            write_output(local, &data, f.is_executable)?;
        }
        // The disk cache only keeps results whose outputs it can hold.
        if keep
            && !left_remote
            && result.output_directories.is_empty()
            && let Some(disk) = &self.disk
        {
            // Fetched blobs are stored already; the others are the local
            // files they matched, or inline.
            for (local, f) in &files {
                let d = f.digest.clone().unwrap_or_default();
                if !f.contents.is_empty() {
                    disk.put_bytes(&d, &f.contents)?;
                } else if !disk.contains_blob(&d) {
                    disk.put_file(local, Some(&self.shared.store.digests))?;
                }
            }
            let mut stored = result.clone();
            stored.stdout_raw = stdout.clone();
            stored.stderr_raw = stderr.clone();
            stored.stdout_digest = None;
            stored.stderr_digest = None;
            disk.put_action_result(&p.action_digest, &stored)?;
        }
        let mut out = stdout;
        if task.deps.as_deref() == Some("msvc") {
            out = self.map_show_includes(&out, task, p);
        }
        out.extend_from_slice(&stderr);
        Ok(out)
    }

    async fn materialize_paths(&self, paths: &[String]) -> Result<(), MaterializeError> {
        let Some(outputs) = &self.config.remote_outputs else { return Ok(()) };
        if outputs.is_empty() {
            return Ok(());
        }
        let _guard = self.materializing.lock().await;
        // A local command reading a thin archive reads its members too.
        let members = crate::archive::expand(paths, &self.shared.build_dir, Some(outputs), &self.shared.store).map_err(MaterializeError::from)?;
        let mut wanted: Vec<(PathBuf, Entry, &String)> = Vec::new();
        for p in paths.iter().chain(&members) {
            let local = self.shared.build_dir.join(p);
            if local.exists() {
                continue;
            }
            if let Some(e) = outputs.get(p) {
                wanted.push((local, e, p));
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let digests: Vec<re::Digest> = wanted.iter().map(|(_, e, _)| e.digest.clone()).collect();
        let blobs = match self.fetch_blobs(self.client.as_ref(), &digests).await {
            Ok(b) => b,
            Err(e) => {
                // Those gone from the remote CAS are rebuilt (rewinding).
                let lost = self.lost_inputs(wanted.iter().map(|(_, e, p)| (p.to_string(), e.digest.clone())).collect()).await;
                return Err(MaterializeError { message: format!("downloading outputs of remote actions: {e:#}"), lost });
            }
        };
        let write = |local: &PathBuf, e: &Entry| -> Result<()> {
            let data = if e.digest.size_bytes == 0 {
                Vec::new()
            } else {
                blobs.get(&e.digest.hash).cloned().with_context(|| format!("missing blob for {}", local.display()))?
            };
            write_output(local, &data, e.executable)?;
            crate::remote_outputs::set_mtime(local, e.mtime).with_context(|| format!("setting the time of {}", local.display()))
        };
        for (local, e, _) in &wanted {
            write(local, e).map_err(|err| MaterializeError::from(format!("{err:#}")))?;
            outputs.forget(&e.path);
        }
        Ok(())
    }

    /// The members of a blob if it's a thin archive: from the disk cache
    /// when it has it, else with ranged reads that skip the symbol table
    /// (small archives are read whole).
    async fn remote_thin_members(&self, client: &Client, d: &re::Digest) -> Result<Option<Vec<String>>> {
        const READ_WHOLE: i64 = 64 * 1024;
        if let Some(data) = self.disk.as_ref().and_then(|disk| disk.read_blob(d)) {
            return crate::archive::members(&data).map_err(anyhow::Error::msg);
        }
        if d.size_bytes <= READ_WHOLE {
            return crate::archive::members(&client.read_blob(d).await?).map_err(anyhow::Error::msg);
        }
        let head = client.read_range(d, 0, crate::archive::HEAD as i64).await?;
        if !crate::archive::is_thin(&head) {
            return Ok(None);
        }
        let start = crate::archive::symbols_end(&head).map_err(anyhow::Error::msg)? as i64;
        if start > d.size_bytes {
            bail!("truncated thin archive");
        }
        let rest = if start == d.size_bytes { Vec::new() } else { client.read_range(d, start, d.size_bytes - start).await? };
        crate::archive::members_from(&rest).map(Some).map_err(anyhow::Error::msg)
    }

    /// Which of these remote-only inputs (task input, digest) the remote CAS
    /// lost. Their records are dropped, so that rebuilding them records them
    /// again.
    async fn lost_inputs(&self, inputs: Vec<(String, re::Digest)>) -> Vec<String> {
        let Some(client) = &self.client else { return Vec::new() };
        let Ok(missing) = client.find_missing(inputs.iter().map(|(_, d)| d.clone()).collect()).await else { return Vec::new() };
        let missing: std::collections::HashSet<String> = missing.into_iter().map(|d| d.hash).collect();
        let mut lost = Vec::new();
        for (path, d) in inputs {
            if missing.contains(&d.hash) {
                if let Some(outputs) = &self.config.remote_outputs {
                    outputs.forget(&path);
                }
                lost.push(path);
            }
        }
        lost
    }

    /// Blobs from the disk cache, else from the remote CAS (stored on disk
    /// then). A large blob is fetched as its chunks when the remote splits
    /// blobs, downloading only those the disk cache lacks.
    async fn fetch_blobs(&self, client: Option<&Client>, needed: &[re::Digest]) -> Result<std::collections::HashMap<String, Vec<u8>>> {
        let mut blobs = std::collections::HashMap::new();
        let mut missing = Vec::new();
        for d in needed {
            match self.disk.as_ref().and_then(|disk| disk.read_blob(d)) {
                Some(data) => {
                    blobs.insert(d.hash.clone(), data);
                }
                None => missing.push(d.clone()),
            }
        }
        if missing.is_empty() {
            return Ok(blobs);
        }
        let Some(client) = client else { bail!("{} blobs are missing from the disk cache", missing.len()) };
        if let Some(disk) = &self.disk
            && client.chunking().split
        {
            let mut whole = Vec::new();
            for d in missing {
                if !crate::disk_cache::is_chunked(&d) {
                    whole.push(d);
                    continue;
                }
                match fetch_chunked(client, disk, &d).await {
                    Ok(Some(data)) => {
                        blobs.insert(d.hash.clone(), data);
                    }
                    // Not split by the server: fetch it whole.
                    Ok(None) => whole.push(d),
                    Err(e) => {
                        if self.config.verbose {
                            eprintln!("ninja-rs: fetching {} as chunks: {e:#}", rbe_client::digest_str(&d));
                        }
                        whole.push(d);
                    }
                }
            }
            missing = whole;
        }
        for (hash, data) in client.read_blobs(&missing).await? {
            if let Some(disk) = &self.disk {
                disk.put_bytes(&re::Digest { hash: hash.clone(), size_bytes: data.len() as i64 }, &data)?;
            }
            blobs.insert(hash, data);
        }
        Ok(blobs)
    }

    /// Rewrites a depfile written by the remote compiler to local paths.
    fn map_depfile(&self, data: &[u8], p: &Prepared) -> Vec<u8> {
        map_depfile(data, &p.mapper, &p.tree_index)
    }
    fn map_show_includes(&self, stdout: &[u8], task: &Task, p: &Prepared) -> Vec<u8> {
        map_show_includes(stdout, task, &p.mapper, &p.tree_index, &self.shared.build_dir)
    }
}

/// Stores the result of an action that ran locally after a disk cache miss,
/// in the form a remote execution would have given (paths relative to the
/// working directory), so that both kinds of entries are interchangeable.
struct Capture {
    inner: Arc<Inner>,
    prepared: Box<Prepared>,
    task: Task,
}

impl LocalCapture for Capture {
    fn capture(mut self: Box<Self>, result: &TaskResult) {
        if !result.status.success() {
            return;
        }
        if let Err(e) = self.store(&result.output)
            && self.inner.config.verbose
        {
            eprintln!("ninja-rs: not cached: {e:#}");
        }
    }
}

/// Above this size, the stdout of a local result goes to the remote CAS
/// rather than inside its action cache entry.
const INLINE_STDOUT_MAX: usize = 64 << 10;

impl Capture {
    /// Stores the result of the command, run locally on a cache miss, in the
    /// disk cache and the remote cache (when the server accepts results).
    /// Runs on a blocking thread.
    fn store(&mut self, output: &[u8]) -> Result<()> {
        let disk = self.inner.disk.clone();
        let client = self
            .inner
            .client
            .as_ref()
            .filter(|_| self.inner.upload)
            .map(|c| c.for_action(&self.inner.action_meta(&self.task), &self.prepared.action_digest.hash));
        if disk.is_none() && client.is_none() {
            return Ok(());
        }
        let digests = Some(&self.inner.shared.store.digests as &dyn rbe_client::tree::DigestCache);
        let mut blobs = rbe_client::BlobSet::default();
        let mut put_file = |local: &PathBuf| -> Result<re::Digest> {
            let digest = match &disk {
                Some(disk) => disk.put_file(local, digests)?,
                None => rbe_client::tree::file_digest(local, digests)?,
            };
            blobs.insert(digest.clone(), rbe_client::BlobSource::File(local.clone()));
            Ok(digest)
        };
        let mut files = Vec::new();
        for (rel, local) in &self.prepared.outputs {
            if !local.is_file() {
                bail!("{} was not produced", local.display());
            }
            let digest = put_file(local)?;
            files.push(re::OutputFile { path: rel.clone(), digest: Some(digest), is_executable: is_executable(local), ..Default::default() });
        }
        // Side outputs (a link's PDB) when the command wrote them.
        for (rel, local) in &self.prepared.side_outputs {
            if local.is_file() {
                let digest = put_file(local)?;
                files.push(re::OutputFile { path: rel.clone(), digest: Some(digest), is_executable: false, ..Default::default() });
            }
        }
        if let Some((rel, local)) = self.prepared.depfile.clone()
            && let Ok(data) = std::fs::read(&local)
        {
            let data = self.depfile_to_remote(&data);
            let digest = rbe_client::digest_of(&data);
            if let Some(disk) = &disk {
                disk.put_bytes(&digest, &data)?;
            }
            blobs.insert(digest.clone(), rbe_client::BlobSource::Bytes(data));
            files.push(re::OutputFile { path: rel, digest: Some(digest), ..Default::default() });
        }
        let stdout = if self.task.deps.as_deref() == Some("msvc") { self.show_includes_to_remote(output) } else { output.to_vec() };
        // Paths in diagnostics as a remote execution prints them: another
        // checkout replaying them gets them relative to its build directory.
        let stdout = self.prepared.mapper.rewrite_text(&stdout);
        let mut result = re::ActionResult {
            output_files: files,
            exit_code: 0,
            stdout_raw: stdout,
            execution_metadata: Some(re::ExecutedActionMetadata { worker: "local".into(), ..Default::default() }),
            ..Default::default()
        };
        if let Some(disk) = &disk {
            disk.put_action_result(&self.prepared.action_digest, &result)?;
        }
        let Some(client) = client else { return Ok(()) };
        if result.stdout_raw.len() > INLINE_STDOUT_MAX {
            let digest = rbe_client::digest_of(&result.stdout_raw);
            blobs.insert(digest.clone(), rbe_client::BlobSource::Bytes(result.stdout_raw.clone()));
            result.stdout_digest = Some(digest);
        }
        // The action and its command too, as remote execution would have
        // them: tools inspecting the cache can show what produced a result.
        let command_digest = rbe_client::digest_of(&prost::Message::encode_to_vec(&self.prepared.command));
        for d in [&self.prepared.action_digest, &command_digest] {
            if let Some((digest, src)) = self.prepared.blobs.0.get(&d.hash) {
                blobs.insert(digest.clone(), src.clone());
            }
        }
        // The outputs first, then the entry pointing at them.
        let store = &self.inner.shared.store;
        let to_upload = store.filter_unknown(blobs);
        tokio::runtime::Handle::current().block_on(async {
            client.upload_blobs(&to_upload).await?;
            client.update_action_result(&self.prepared.action_digest, result).await
        })?;
        store.mark_uploaded(to_upload.0.keys());
        Ok(())
    }

    /// The opposite of `Inner::map_depfile`: local paths to remote ones.
    fn depfile_to_remote(&mut self, data: &[u8]) -> Vec<u8> {
        let Ok(d) = parse_depfile(data) else { return data.to_vec() };
        let mapper = &mut self.prepared.mapper;
        let mut out = String::new();
        let outs: Vec<String> = d.outs.iter().map(|o| escape_make(&mapper.rewrite_arg(o))).collect();
        out.push_str(&outs.join(" "));
        out.push(':');
        for i in &d.ins {
            out.push_str(" \\\n  ");
            out.push_str(&escape_make(&mapper.rewrite_arg(i)));
        }
        out.push('\n');
        out.into_bytes()
    }

    /// The opposite of `Inner::map_show_includes`.
    fn show_includes_to_remote(&mut self, stdout: &[u8]) -> Vec<u8> {
        let prefix = if self.task.msvc_deps_prefix.is_empty() { DEFAULT_DEPS_PREFIX } else { self.task.msvc_deps_prefix.as_str() };
        let text = String::from_utf8_lossy(stdout);
        let mut out = String::new();
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            let ending = &line[body.len()..];
            match filter_show_includes(body, prefix) {
                Some(path) if !path.is_empty() => {
                    out.push_str(prefix);
                    out.push(' ');
                    out.push_str(&self.prepared.mapper.rewrite_arg(path));
                    out.push_str(ending);
                }
                _ => out.push_str(line),
            }
        }
        out.into_bytes()
    }
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &std::path::Path) -> bool {
    false
}

/// Appends an action to the execution log: what makes its key.
fn log_action(log: &std::sync::Mutex<std::fs::File>, task: &Task, meta: &ActionMeta, p: &Prepared) {
    use std::io::Write;
    #[allow(deprecated)] // The Action carries it too.
    let platform: serde_json::Map<String, serde_json::Value> =
        p.command.platform.iter().flat_map(|pl| &pl.properties).map(|prop| (prop.name.clone(), prop.value.clone().into())).collect();
    let environment: serde_json::Map<String, serde_json::Value> =
        p.command.environment_variables.iter().map(|e| (e.name.clone(), e.value.clone().into())).collect();
    let inputs: serde_json::Map<String, serde_json::Value> = p.input_digests.iter().map(|(path, d)| (path.clone(), d.clone().into())).collect();
    let entry = serde_json::json!({
        "outputs": task.outputs,
        "mnemonic": meta.mnemonic,
        "target": meta.target_id,
        "action": rbe_client::digest_str(&p.action_digest),
        "arguments": p.command.arguments,
        "working_directory": p.command.working_directory,
        "environment": environment,
        "platform": platform,
        "input_root": rbe_client::digest_str(&p.input_root_digest),
        "inputs": inputs,
    });
    let _ = writeln!(log.lock().unwrap(), "{entry}");
}

/// gRPC status codes in `google.rpc.Status`.
mod tonic_code {
    pub const FAILED_PRECONDITION: i32 = 9;
}

/// Whether the CAS still holds all of an action result's outputs.
async fn outputs_in_cas(client: &Client, result: &re::ActionResult) -> Result<bool> {
    let digests: Vec<re::Digest> = result.output_files.iter().filter_map(|f| f.digest.clone()).filter(|d| d.size_bytes > 0).collect();
    if digests.is_empty() {
        return Ok(true);
    }
    Ok(client.find_missing(digests).await?.is_empty())
}

/// The result of a task whose inputs were lost from the remote CAS: the
/// engine rebuilds them and runs the task again.
fn lost_inputs_result(task: &Task, lost: Vec<String>, mut output: Vec<u8>) -> TaskResult {
    output.extend_from_slice(format!("ninja-rs: gone from the remote cache: {}\n", lost.join(", ")).as_bytes());
    TaskResult { edge: task.edge, status: ExitStatus::Failure(1), output, kind: ExecKind::Remote, cas_outputs: Vec::new(), lost_inputs: lost }
}

/// Fetches a large blob as the chunks the remote splits it into, downloading
/// only those the disk cache lacks, and stores it there in those chunks.
/// `None` when the remote can't split it.
async fn fetch_chunked(client: &Client, disk: &DiskCache, d: &re::Digest) -> Result<Option<Vec<u8>>> {
    let Some(split) = client.split_blob(d).await? else { return Ok(None) };
    let lacking: Vec<re::Digest> = split.chunk_digests.iter().filter(|c| !disk.contains_chunk(c)).cloned().collect();
    let fetched = client.read_blobs(&lacking).await?;
    let mut data = Vec::with_capacity(d.size_bytes as usize);
    for c in &split.chunk_digests {
        match fetched.get(&c.hash) {
            Some(chunk) => {
                // Checked before it's stored under that digest.
                if rbe_client::digest_of(chunk) != *c {
                    bail!("chunk {} has other contents", rbe_client::digest_str(c));
                }
                disk.put_chunk_bytes(c, chunk)?;
                data.extend_from_slice(chunk);
            }
            None => data.extend_from_slice(&disk.read_chunk(c).with_context(|| format!("chunk {} was evicted", rbe_client::digest_str(c)))?),
        }
    }
    // REAPI asks clients to check the assembled blob.
    if rbe_client::digest_of(&data) != *d {
        bail!("the chunks don't make up the blob");
    }
    disk.put_chunked(d, split)?;
    Ok(Some(data))
}

fn prepared_blobs(p: &Prepared) -> rbe_client::BlobSet {
    let mut b = rbe_client::BlobSet::default();
    for (h, (d, s)) in &p.blobs.0 {
        b.0.insert(h.clone(), (d.clone(), s.clone()));
    }
    b
}

fn same_content(local: &std::path::Path, d: &re::Digest, store: &Store) -> bool {
    match std::fs::metadata(local) {
        Ok(m) if m.is_file() && m.len() as i64 == d.size_bytes => {
            rbe_client::tree::file_digest(local, Some(&store.digests)).map(|ld| ld.hash == d.hash).unwrap_or(false)
        }
        _ => false,
    }
}

fn write_output(path: &std::path::Path, data: &[u8], executable: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.ninja-rs-tmp", path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()));
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(path);
        std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    }
    Ok(())
}

/// Escapes a path for a Makefile-style depfile.
fn escape_make(p: &str) -> String {
    let mut out = String::new();
    for ch in p.chars() {
        match ch {
            ' ' => out.push_str("\\ "),
            '#' => out.push_str("\\#"),
            '$' => out.push_str("$$"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_limits() {
        // No limits.
        assert_eq!(fallback_limit(1000, 1000, None, None), None);
        // At most 2.
        assert_eq!(fallback_limit(2, 10, Some(2), None), None);
        assert!(fallback_limit(3, 10, Some(2), None).unwrap().contains("--remote_max_fallbacks=2"));
        assert!(fallback_limit(1, 10, Some(0), None).is_some());
        // A ratio: not before enough actions started, then above it.
        assert_eq!(fallback_limit(5, FALLBACK_RATIO_MIN_ACTIONS - 1, None, Some(0.1)), None);
        assert_eq!(fallback_limit(2, 20, None, Some(0.1)), None);
        assert!(fallback_limit(3, 20, None, Some(0.1)).unwrap().contains("3 of 20"));
    }

    fn task(bindings: &[(&str, &str)]) -> Task {
        Task { bindings: bindings.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() }
    }

    #[test]
    fn policy() {
        let s = vec!["Compile".to_string()];
        assert!(policy_wants(&task(&[("rbe_mnemonic", "CXXCompile")]), &s));
        assert!(policy_wants(&task(&[("rbe_mnemonic", "CCompile"), ("rbe_target", "foo")]), &s));
        assert!(!policy_wants(&task(&[("rbe_mnemonic", "CXXCompile"), ("rbe_local", "1")]), &s));
        assert!(!policy_wants(&task(&[("rbe_mnemonic", "CXXLink")]), &s));
        assert!(!policy_wants(&task(&[("rbe_mnemonic", "CXXScan")]), &s));
        let mut regen = task(&[("rbe_mnemonic", "CXXCompile")]);
        regen.generator = true;
        assert!(!policy_wants(&regen, &s));
        assert!(!policy_wants(&task(&[]), &s));
        let mut t = task(&[("rbe_mnemonic", "CXXCompile")]);
        t.use_console = true;
        assert!(!policy_wants(&t, &s));
        assert!(policy_wants(&task(&[("rbe_mnemonic", "CXXArchive")]), &["Compile".into(), "Archive".into()]));
    }

    #[test]
    fn make_escaping() {
        assert_eq!(escape_make("a b#c$d"), "a\\ b\\#c$$d");
    }
}
