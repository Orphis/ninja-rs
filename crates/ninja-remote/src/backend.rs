//! The REAPI backend: execution policy, action cache lookups (the local
//! disk cache, then the remote one), upload, Execute, and output download
//! (with depfile and /showIncludes mapping; fetching blobs in `download`).
//! With only a disk cache, cache misses run locally and their results are
//! stored (`capture`).

mod capture;
mod download;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ninja_core::clparser::{DEFAULT_DEPS_PREFIX, filter_show_includes};
use ninja_core::depfile::parse_depfile;
use ninja_core::runner::{
    CasOutput, ExecKind, ExitStatus, LocalCommand, MapBack, MaterializeError, MaterializeFuture, Race, RemoteBackend, RemoteFuture, RemoteOutcome, Task,
    TaskResult,
};
use ninja_core::status::message;
use rbe_client::proto as re;
use rbe_client::{ActionMeta, Client};

use crate::action::{Prepared, Shared, prepare};
use crate::disk_cache::DiskCache;
use crate::remote_outputs::{Entry, RemoteOutputs};
use crate::store::Store;
use crate::targets::Targets;
use crate::toolchains::Toolchains;
use capture::Capture;
use download::{DOWNLOAD_BUDGET_KIB, Fetched, FsClock, disk_store, write_output};

#[derive(Clone, Debug)]
pub struct BackendConfig {
    /// Absolute build directory (where build.ninja is).
    pub build_dir: PathBuf,
    /// Other directories the build writes to (see `Shared::volatile`).
    pub volatile: Vec<PathBuf>,
    /// The directories build edges write to, inside the build directory or
    /// not (see `Shared::written`).
    pub written: Vec<PathBuf>,
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
    /// The daemon (`--daemon`), for a build it has begun.
    pub daemon: Option<Arc<ninja_daemon::Client>>,
    /// `--action_env`: variables every action gets (name, value), in order.
    pub action_env: Vec<(String, String)>,
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
    /// Remote-only outputs being downloaded, by path: a task needing one
    /// another task is downloading waits for it (`true` once written)
    /// rather than writing it too; tasks needing others download theirs
    /// meanwhile.
    materializing: ClaimMap,
    fallbacks: Fallbacks,
    /// Results of commands run locally on a cache miss that weren't cached
    /// (see `Capture::store`), by reason.
    uncached: Reasons,
    /// Hashes of blobs found gone from the remote CAS during this build: a
    /// disk cache result referencing one is stale (see `try_execute`).
    lost: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Whether the build was told why local results aren't cached.
    env_warned: std::sync::atomic::AtomicBool,
    /// Whether a disk cache write failed (and the build was told).
    disk_warned: Arc<std::sync::atomic::AtomicBool>,
    /// Bytes downloads hold in memory, in KiB (see `DOWNLOAD_BUDGET_KIB`).
    download_budget: Arc<tokio::sync::Semaphore>,
    /// Directories this build wrote outputs to, swept of temporary files
    /// interrupted writes left (see `sweep`).
    swept: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// Local results going to the remote cache (see `Capture`), which the
    /// build waits for at its end (`Backend::finish_uploads`).
    uploads: std::sync::Mutex<tokio::task::JoinSet<()>>,
    /// Bytes of outputs those uploads hold in memory, in KiB (see
    /// `capture::UPLOAD_BUDGET_KIB`).
    upload_budget: Arc<tokio::sync::Semaphore>,
    /// The clocks of the file systems outputs are written to.
    fs_clock: FsClock,
}

/// Remote-only outputs being downloaded, by path (see `Inner::materializing`).
type ClaimMap = std::sync::Mutex<std::collections::HashMap<String, tokio::sync::watch::Receiver<Option<bool>>>>;

/// Remote actions that ran locally instead, with the first reason of each
/// kind.
#[derive(Default)]
struct Fallbacks {
    /// Actions the policy sent remotely.
    started: std::sync::atomic::AtomicUsize,
    /// Actions that couldn't be sent (`prepare` refused them), by reason.
    unsent: Reasons,
    /// Actions run locally after a remote failure.
    failed: std::sync::Mutex<(usize, Option<String>)>,
    /// Why remote failures no longer fall back, once a limit is crossed.
    stopped: std::sync::Mutex<Option<String>>,
}

/// How long the build waits for its uploads (see `Backend::finish_uploads`)
/// before it says so.
const UPLOADS_NOTICED: std::time::Duration = std::time::Duration::from_secs(1);

/// How many times an action is prepared, its inputs changing as they're
/// uploaded (see `Inner::execute`).
const PREPARES: usize = 3;

/// Reasons counted by what they are (see `Reasons::what`), for a line at
/// the end of the build.
#[derive(Default)]
struct Reasons(std::sync::Mutex<std::collections::BTreeMap<String, usize>>);

/// The longest reason a summary quotes, in characters.
const REASON_MAX: usize = 100;

impl Reasons {
    fn add(&self, reason: &str) {
        *self.0.lock().unwrap().entry(Self::what(reason)).or_default() += 1;
    }

    /// What a reason is: its first line, without the details after it
    /// ("what: details", which name files or quote a command line, which
    /// can be kilobytes long), and `REASON_MAX` characters at most.
    fn what(reason: &str) -> String {
        let line = first_line(reason);
        let what = line.split(": ").next().unwrap_or_default();
        match what.char_indices().nth(REASON_MAX) {
            Some((end, _)) => format!("{}...", &what[..end]),
            None => what.to_string(),
        }
    }

    /// How many, and why: each reason's count, the most frequent first.
    fn summary(&self) -> (usize, String) {
        let mut reasons: Vec<(String, usize)> = self.0.lock().unwrap().iter().map(|(what, n)| (what.clone(), *n)).collect();
        reasons.sort_by_key(|r| std::cmp::Reverse(r.1));
        let total = reasons.iter().map(|(_, n)| n).sum();
        let mut shown: Vec<String> = reasons.iter().take(4).map(|(what, n)| format!("{n}: {what}")).collect();
        if reasons.len() > 4 {
            shown.push(format!("{} more reasons", reasons.len() - 4));
        }
        (total, shown.join("; "))
    }
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
            daemon: config.daemon.clone(),
            daemon_error: Default::default(),
            profile_dir: disk.as_ref().map(|d| d.profile_dir()),
            volatile: std::iter::once(config.build_dir.clone()).chain(config.volatile.iter().cloned()).collect(),
            written: crate::scanner::WrittenDirs::new(std::iter::once(config.build_dir.clone()).chain(config.written.iter().cloned()), windows),
            action_env: config.action_env.clone(),
            compiler_scans: Default::default(),
        };
        shared.store.new_build();
        // No command runs yet: copies of response files nothing used for
        // a while can go (a local run writes again those it needs).
        sweep_rsp_copies(&config.build_dir.join(RSP_COPIES), RSP_COPIES_UNUSED);
        if shared.include_scanner == crate::action::IncludeScanner::Preprocessor {
            let p = shared.store.preprocessor(&shared.build_dir, windows);
            for d in &shared.volatile {
                p.cache.add_volatile(d);
            }
            // The rest of the build directory (dependencies fetched there)
            // is checked once per command run on the host, which the runner
            // reports (`RemoteBackend::host_wrote`).
            for d in std::iter::once(&config.build_dir).chain(&config.written) {
                p.cache.add_written(d);
            }
            p.cache.track_host_writes();
        }
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                upload,
                disk,
                shared,
                config,
                materializing: Default::default(),
                fallbacks: Fallbacks::default(),
                uncached: Reasons::default(),
                lost: Default::default(),
                env_warned: Default::default(),
                disk_warned: Default::default(),
                download_budget: Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_BUDGET_KIB)),
                swept: Default::default(),
                uploads: Default::default(),
                upload_budget: Arc::new(tokio::sync::Semaphore::new(capture::UPLOAD_BUDGET_KIB)),
                fs_clock: FsClock::default(),
            }),
        })
    }

    /// Waits for the local results going to the remote cache, until `stop`
    /// completes (an interrupt): those left are given up, and their number
    /// returned. `waiting` is told how many there are once the wait is
    /// noticeable.
    pub async fn finish_uploads(&self, stop: impl std::future::Future<Output = ()>, waiting: impl FnOnce(usize)) -> usize {
        let mut uploads = std::mem::take(&mut *self.inner.uploads.lock().unwrap());
        let mut waiting = Some(waiting);
        let mut stop = std::pin::pin!(stop);
        let noticed = tokio::time::sleep(UPLOADS_NOTICED);
        let mut noticed = std::pin::pin!(noticed);
        loop {
            tokio::select! {
                biased;
                () = &mut stop => {
                    // Those done already aren't given up.
                    while uploads.try_join_next().is_some() {}
                    let left = uploads.len();
                    uploads.shutdown().await;
                    return left;
                }
                done = uploads.join_next() => {
                    if done.is_none() {
                        return 0;
                    }
                }
                () = &mut noticed, if waiting.is_some() => {
                    if let Some(tell) = waiting.take() {
                        tell(uploads.len());
                    }
                }
            }
        }
    }

    /// Whether results of commands run locally go to the remote cache.
    pub fn uploads_local_results(&self) -> bool {
        self.inner.upload
    }

    /// Blob bytes transferred with the remote service in this build.
    pub fn transfers(&self) -> Option<rbe_client::Transfers> {
        self.inner.client.as_ref().map(|c| c.transfers())
    }

    /// Why the build stopped using the daemon, if it did.
    pub fn daemon_error(&self) -> Option<String> {
        self.inner.shared.daemon_error.lock().unwrap().clone()
    }

    /// How many remote actions ran locally instead, and why: a line for
    /// the end of the build, and whether some did after remote failures.
    pub fn fallback_summary(&self) -> Option<(String, bool)> {
        let f = &self.inner.fallbacks;
        let (unsent, unsent_reasons) = f.unsent.summary();
        let (failed, failed_reason) = f.failed.lock().unwrap().clone();
        // Compiles the chosen include scanner left to the conservative one.
        let scans = self.inner.shared.store.scan.fallback_summary().map(|s| format!("{s}."));
        // And those the preprocessor left to the compiler.
        let scans = match (scans, self.inner.shared.compiler_scans.summary()) {
            (Some(s), Some(c)) => Some(format!("{c} {s}")),
            (s, c) => s.or(c),
        };
        if unsent + failed == 0 {
            return scans.map(|s| (s, false));
        }
        let mut parts = Vec::new();
        if unsent > 0 {
            parts.push(format!("{unsent} couldn't be sent ({unsent_reasons})"));
        }
        if failed > 0 {
            parts.push(format!("{failed} after remote failures (first: {})", failed_reason.unwrap_or_default()));
        }
        let started = f.started.load(std::sync::atomic::Ordering::Relaxed);
        let mut summary = format!("{} of {started} remote actions ran locally: {}.", unsent + failed, parts.join("; "));
        if let Some(scans) = scans {
            summary = format!("{summary} {scans}");
        }
        Some((summary, failed > 0))
    }

    /// How many results of commands run locally on a cache miss weren't
    /// cached, and why: a line for the end of the build (`--remote_verbose`
    /// says it of each).
    pub fn uncached_summary(&self) -> Option<String> {
        let (n, reasons) = self.inner.uncached.summary();
        (n > 0).then(|| format!("{n} local result{} not cached: {reasons}.", if n == 1 { " was" } else { "s were" }))
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

    fn host_wrote(&self) {
        self.inner.shared.store.host_wrote();
    }

    fn execute_raced(&self, task: Task, race: Arc<Race>) -> RemoteFuture {
        let inner = self.inner.clone();
        Box::pin(async move { inner.execute(task, Some(race)).await })
    }

    fn max_jobs(&self) -> usize {
        self.inner.config.jobs
    }

    fn executes(&self) -> bool {
        self.inner.client.is_some() && !self.inner.config.cache_only
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
        self.inner.lost.lock().unwrap().extend(missing);
        Ok(dropped)
    }
}

enum Failure {
    /// The action could not be prepared: run locally.
    Prepare(String),
    /// An input changed since the action was prepared (an editor saving a
    /// source): found as it was uploaded, and not sent. The action is
    /// prepared again.
    InputChanged(PathBuf),
    /// The remote service failed.
    Infra(anyhow::Error),
    /// A cache miss without remote execution (the disk cache alone, or a
    /// remote cache): run locally, then store the result.
    RunLocally(Box<Prepared>),
    /// Racing: the local branch claimed the outputs first.
    Lost,
}

/// What executing an action remotely gave.
enum Executed {
    /// Its result, and whether it came from the action cache.
    Done(Box<re::ActionResult>, bool),
    /// Inputs that only existed remotely are gone: the task's result, for
    /// the build to rewind their actions.
    InputsLost(TaskResult),
}

/// Blobs a disk cache entry names that the disk cache can't give back
/// (removed, damaged), without a remote to fetch them from.
#[derive(Debug)]
struct DiskMiss(usize);

impl std::fmt::Display for DiskMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} blobs are missing from the disk cache", self.0)
    }
}

impl std::error::Error for DiskMiss {}

/// What an upload of an action's inputs failing is: an input changed
/// since the action was prepared, which isn't sent (see
/// `rbe_client::SourceChanged`), or a remote failure.
fn upload_failure(e: anyhow::Error) -> Failure {
    match rbe_client::SourceChanged::of(&e) {
        Some(path) => Failure::InputChanged(path.to_path_buf()),
        None => Failure::Infra(e),
    }
}

/// Whether a remote failure is a blob the CAS doesn't have.
fn is_not_found(e: &anyhow::Error) -> bool {
    rbe_client::status_of(e).is_some_and(|s| s.code() as i32 == tonic_code::NOT_FOUND)
}

/// The command a local run of the action uses (see `LocalCommand`): the
/// remote action's (with the host's paths for other volumes, see
/// `LocalForm`). None for a command with steps on the host, which runs as
/// the build file has it.
fn local_command(p: &Prepared, task: &Task, build_dir: &Path, windows: bool) -> Option<LocalCommand> {
    if !p.local_before.is_empty() || !p.local_after.is_empty() {
        return None;
    }
    let (arguments, rsp_files, env) = match &p.local {
        Some(l) => (&l.arguments, &l.rsp_files, l.env.clone()),
        None => (&p.command.arguments, &p.rsp_files, p.command.environment_variables.iter().map(|e| (e.name.clone(), e.value.clone())).collect()),
    };
    let (argv, files) = local_rsp_files(arguments, rsp_files, task, build_dir, windows);
    Some(LocalCommand {
        argv,
        dir: PathBuf::from(&p.mapper.build_dir),
        env,
        files,
        map_back: Some(Arc::new(LocalMapBack { mapper: p.mapper.clone(), tree_index: p.tree_index.clone(), build_dir: build_dir.to_path_buf() })),
    })
}

/// Where local runs keep private copies of response files (see
/// `local_rsp_files`), in the build directory.
const RSP_COPIES: &str = ".ninja_rs_rsp";

/// How long a copy of a response file stays unused before a build removes
/// it.
const RSP_COPIES_UNUSED: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// Removes the copies of response files in `dir` (named by their contents'
/// hash) written more than `age` ago, and temporary files left by
/// interrupted writes of them, before any command of the build runs: one
/// that needs a copy again writes it again. Nothing else in `dir` goes.
fn sweep_rsp_copies(dir: &Path, age: std::time::Duration) {
    let now = std::time::SystemTime::now();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let hash = name.split('.').next().unwrap_or_default();
        let ours = hash.len() == 32 && hash.bytes().all(|c| c.is_ascii_hexdigit()) && (name.ends_with(".rsp") || name.contains(".tmp."));
        if ours && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > age) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Where a local run of the action reads its response files, rewritten as
/// the remote action reads them, and the command naming them. The task's
/// own (ninja's `rspfile`) is rewritten in place: it is the edge's. Any
/// other (a checked-in `@flags.rsp`, one several commands share) gets a
/// private copy in `.ninja_rs_rsp`, named by its contents, and the
/// references to it (in the command, in other response files) point
/// there: the original is never modified, and a concurrent command never
/// reads it half written.
fn local_rsp_files(
    arguments: &[String],
    rsp_files: &[(PathBuf, Vec<u8>)],
    task: &Task,
    build_dir: &Path,
    windows: bool,
) -> (Vec<String>, Vec<(PathBuf, Vec<u8>)>) {
    let key = |path: &Path| {
        let s = crate::scanner::normalize(path).to_string_lossy().replace('\\', "/");
        if windows { s.to_ascii_lowercase() } else { s }
    };
    let own = task.rspfile.as_ref().map(|(r, _)| key(&build_dir.join(r)));
    let private_dir = build_dir.join(RSP_COPIES);
    let copies: HashMap<String, PathBuf> = rsp_files
        .iter()
        .filter(|(path, _)| Some(key(path)) != own)
        .map(|(path, bytes)| (key(path), private_dir.join(format!("{}.rsp", &rbe_client::digest_of(bytes).hash[..32]))))
        .collect();
    if copies.is_empty() {
        return (arguments.to_vec(), rsp_files.to_vec());
    }
    let redirect = |args: &[String]| -> Vec<String> {
        args.iter()
            .map(|a| match a.strip_prefix('@').and_then(|f| copies.get(&key(&build_dir.join(f)))) {
                Some(copy) => format!("@{}", copy.display()),
                None => a.clone(),
            })
            .collect()
    };
    let files = rsp_files
        .iter()
        .map(|(path, bytes)| {
            let text = String::from_utf8_lossy(bytes);
            let content = crate::cmdline::join_rsp(&redirect(&crate::cmdline::split_rsp(&text, windows)), windows).into_bytes();
            (copies.get(&key(path)).cloned().unwrap_or_else(|| path.clone()), content)
        })
        .collect();
    (redirect(arguments), files)
}

/// Maps a local run of the remote command back, as a download does.
struct LocalMapBack {
    mapper: crate::paths::PathMapper,
    tree_index: crate::paths::TreeIndex,
    build_dir: PathBuf,
}

impl MapBack for LocalMapBack {
    fn map_back(&self, task: &Task, result: &mut TaskResult) {
        // Its absolute paths are already the host's.
        let map = |path: &str| self.mapper.map_back_local(path, &self.tree_index);
        if let Some(depfile) = &task.depfile
            && let Ok(data) = std::fs::read(depfile)
        {
            let _ = std::fs::write(depfile, map_depfile(&data, &map));
        }
        if task.deps.as_deref() == Some("msvc") {
            result.output = map_show_includes(&result.output, task, &map, &self.build_dir);
        }
    }
}

/// A depfile in remote form (paths of the remote action) with local paths
/// (`map` gives a path's).
fn map_depfile(data: &[u8], map: &dyn Fn(&str) -> String) -> Vec<u8> {
    let Ok(d) = parse_depfile(data) else { return data.to_vec() };
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

/// `/showIncludes` lines in remote form with local paths (`map` gives a
/// path's).
fn map_show_includes(stdout: &[u8], task: &Task, map: &dyn Fn(&str) -> String, build_dir: &std::path::Path) -> Vec<u8> {
    let prefix = if task.msvc_deps_prefix.is_empty() { DEFAULT_DEPS_PREFIX } else { task.msvc_deps_prefix.as_str() };
    let text = String::from_utf8_lossy(stdout);
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        match filter_show_includes(body, prefix) {
            Some(path) if !path.is_empty() => {
                let mapped = map(path);
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

/// Runs file system work (writing outputs, hashing, the disk cache) off
/// the async workers, which then go on serving transfers (HTTP/2 flow
/// control) and build events. Inline on a single-threaded runtime.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Paths a task downloads for every task needing them (see
/// `Inner::materializing`); those it didn't write are released as failed.
struct PathClaims<'a> {
    map: &'a ClaimMap,
    senders: std::collections::HashMap<String, tokio::sync::watch::Sender<Option<bool>>>,
}

impl PathClaims<'_> {
    fn done(&mut self, path: &str) {
        if let Some(tx) = self.senders.remove(path) {
            self.map.lock().unwrap().remove(path);
            let _ = tx.send(Some(true));
        }
    }
}

impl Drop for PathClaims<'_> {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap();
        for (path, tx) in self.senders.drain() {
            map.remove(&path);
            let _ = tx.send(Some(false));
        }
    }
}

/// Fetches `wanted` (each with its path, as `ClaimMap` keys them) with
/// `fetch`, given those this task claimed (the paths no other task is
/// fetching) and their claims, which it marks done. It waits for the
/// others, and should one of those fail, claims those still missing
/// (`missing`), and so on until all are there: never two tasks writing one
/// file (on Windows, the second rename fails while a command has the file
/// open).
async fn fetch_claimed<'m, 'w, T, Fut>(
    map: &'m ClaimMap,
    wanted: Vec<&'w T>,
    path: impl Fn(&T) -> &str,
    missing: impl Fn(&T) -> bool,
    mut fetch: impl FnMut(Vec<&'w T>, PathClaims<'m>) -> Fut,
) -> Result<(), MaterializeError>
where
    Fut: std::future::Future<Output = Result<(), MaterializeError>>,
{
    let mut left = wanted;
    while !left.is_empty() {
        let (claims, waits) = {
            let mut fetching = map.lock().unwrap();
            let (mut senders, mut waits) = (std::collections::HashMap::new(), Vec::new());
            for w in &left {
                let p = path(w);
                match fetching.get(p) {
                    Some(rx) => waits.push(rx.clone()),
                    None if !senders.contains_key(p) => {
                        let (tx, rx) = tokio::sync::watch::channel(None);
                        fetching.insert(p.to_string(), rx);
                        senders.insert(p.to_string(), tx);
                    }
                    None => {}
                }
            }
            (PathClaims { map, senders }, waits)
        };
        let mine: Vec<&T> = left.iter().copied().filter(|w| claims.senders.contains_key(path(w))).collect();
        // Those it doesn't write are released as failed when it ends.
        if mine.is_empty() {
            drop(claims);
        } else {
            fetch(mine, claims).await?;
        }
        let mut failed = false;
        for mut rx in waits {
            failed |= !matches!(rx.wait_for(|v| v.is_some()).await.map(|v| *v), Ok(Some(true)));
        }
        if !failed {
            break;
        }
        left = blocking(|| left.into_iter().filter(|w| missing(w)).collect());
    }
    Ok(())
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
        // Prepared again while an input changes as it's uploaded, up to a
        // point: one changing all the time (written by something else as the
        // build goes) makes the action one that can't be sent, not a remote
        // failure.
        let mut prepared = 0;
        let outcome = loop {
            match self.clone().try_execute(&task, race.as_ref(), &mut command).await {
                Err(Failure::InputChanged(path)) => {
                    prepared += 1;
                    let why = format!("input changed during the build: {}", path.display());
                    if prepared >= PREPARES {
                        break Err(Failure::Prepare(why));
                    }
                    if self.config.verbose {
                        message(&format!("{why}; preparing the action again"));
                    }
                    self.input_changed(&path);
                }
                outcome => break outcome,
            }
        };
        match outcome {
            Ok(r) => RemoteOutcome::Done(r),
            Err(Failure::Lost) => RemoteOutcome::Lost,
            Err(Failure::InputChanged(_)) => unreachable!("prepared again"),
            Err(Failure::Prepare(reason)) => {
                if self.config.strict {
                    return failed(format!("ninja: the action can't run remotely (--remote_strict): {reason}\n"));
                }
                self.fallbacks.unsent.add(&reason);
                RemoteOutcome::Fallback(reason, None)
            }
            Err(Failure::RunLocally(prepared)) => {
                let command = local_command(&prepared, &task, &self.shared.build_dir, self.shared.windows);
                RemoteOutcome::RunLocally(Box::new(Capture { inner: self, prepared, task }), command)
            }
            Err(Failure::Infra(e)) => {
                let reason = format!("remote execution failed: {}", rbe_client::fault::describe(e.as_ref()));
                // A certificate refused (the server's, or this client's)
                // fails every remote action alike, and must be seen to:
                // not hidden behind local runs.
                if !self.config.local_fallback || self.config.strict || certificate_refused(&e) {
                    return failed(format!("ninja: {reason}\n"));
                }
                match self.allow_fallback(&reason) {
                    Ok(()) => RemoteOutcome::Fallback(reason, command),
                    Err(why) => failed(format!("ninja: {reason}\nninja: not running it locally: {why}\n")),
                }
            }
        }
    }

    /// An input found changed as it was uploaded: what's known of it goes
    /// (its digest, kept with a stamp that may not show the change; the
    /// toolchain trees holding it), for the action to be prepared again.
    fn input_changed(&self, path: &Path) {
        let store = &self.shared.store;
        if let Err(e) = crate::action::rehash(path, &store.digests)
            && self.config.verbose
        {
            message(&format!("hashing {} again: {e:#}", path.display()));
        }
        store.forget_toolchain_trees_holding(path);
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
        *command = local_command(&prepared, task, &self.shared.build_dir, self.shared.windows);
        if let (Some(race), Some(c)) = (race, command.clone()) {
            race.set_command(c);
        }
        let digest = prepared.action_digest.clone();
        if let Some(log) = &self.config.execution_log {
            log_action(log, task, &meta, &prepared);
        }

        // The disk cache first. A result this action can't use (see
        // `check_result`) is removed: a miss.
        let disk_hit = match &self.disk {
            Some(disk) => {
                let (disk, d) = (disk.clone(), digest.clone());
                let hit = tokio::task::spawn_blocking(move || disk.get_action_result_and_absent(&d)).await.ok().flatten().filter(|(r, _)| r.exit_code == 0);
                let unusable = hit.as_ref().and_then(|(r, _)| check_result(&prepared, r).err());
                if let Some(e) = unusable {
                    if self.config.verbose {
                        message(&format!("disk cache entry {} ignored: {e}", digest.hash));
                    }
                    if let Some(disk) = &self.disk {
                        disk.remove_action_result(&digest);
                    }
                    None
                } else {
                    hit
                }
            }
            None => None,
        };
        // A result whose outputs stayed in the remote CAS (minimal downloads)
        // is trusted without asking the remote, as Bazel does with action
        // rewinding: should they be gone, the action reading them reports
        // them lost, the build rewinds this one, and the result is then
        // stale (`lost`). Unless this build needs them downloaded.
        let disk_hit = match disk_hit {
            Some((result, absent)) if absent.is_empty() => Some((result, false)),
            Some((result, absent)) => {
                let lost = {
                    let lost = self.lost.lock().unwrap();
                    absent.iter().any(|d| lost.contains(&d.hash))
                };
                let needed: std::collections::HashSet<&str> = result
                    .output_files
                    .iter()
                    .filter(|f| self.must_download(task, &prepared, &f.path))
                    .filter_map(|f| f.digest.as_ref())
                    .map(|d| d.hash.as_str())
                    .collect();
                if lost {
                    if let Some(disk) = &self.disk {
                        disk.remove_action_result(&digest);
                    }
                    None
                } else if self.client.is_some() && self.config.download_minimal && !absent.iter().any(|d| needed.contains(d.hash.as_str())) {
                    Some((result, true))
                } else {
                    None
                }
            }
            None => None,
        };
        if let Some((result, remote)) = disk_hit
            && let Some(done) = self.restore_disk_hit(task, race, &prepared, &meta, result, remote).await?
        {
            return Ok(done);
        }
        let Some(client) = &self.client else { return Err(Failure::RunLocally(Box::new(prepared))) };
        let client = client.for_action(&meta, &prepared.action_digest.hash);

        // Then the remote action cache. With minimal downloads, a hit whose
        // outputs left the CAS is a miss: they wouldn't be downloaded now,
        // and rewinding would only find the same result again.
        let minimal = self.config.download_minimal;
        let mut cached = client.get_action_result(&digest).await.map_err(Failure::Infra)?.filter(|r| r.exit_code == 0);
        let mut stale = false;
        // A result this action can't use is a miss, and Execute skips it.
        if let Some(r) = &cached
            && let Err(e) = check_result(&prepared, r)
        {
            if self.config.verbose {
                message(&format!("remote cache entry {} ignored: {e}", digest.hash));
            }
            cached = None;
            stale = true;
        }
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

        // The result, and its outputs fetched, before steps on the host run
        // (see below): should that fail, nothing ran there, and the local
        // run that follows runs them once.
        let mut output = Vec::new();
        let steps_first = !prepared.local_before.is_empty();
        let (mut result, mut hit) = match cached {
            Some(r) => (r, true),
            None => match self.execute_remotely(&client, task, &prepared, stale, &mut output).await? {
                Executed::Done(r, hit) => (*r, hit),
                Executed::InputsLost(r) => return Ok(r),
            },
        };
        let mut retried = false;
        let mut staged = loop {
            // An execution's result must be usable too: nothing is written
            // from one that isn't (the action runs locally instead).
            check_result(&prepared, &result).map_err(|e| Failure::Infra(anyhow::anyhow!("unusable result for {}: {e}", digest.hash)))?;
            // Its outputs are in the remote CAS (checked for a hit in minimal
            // mode, uploaded by an execution).
            if result.exit_code == 0 {
                let mut lost = self.lost.lock().unwrap();
                for d in result.output_files.iter().filter_map(|f| f.digest.as_ref()) {
                    lost.remove(&d.hash);
                }
            }
            match self.fetch_outputs(Some(&client), task, &prepared, &result, steps_first).await {
                // A hit whose outputs the CAS no longer has (evicted since it
                // was stored, or checked) is a miss: the action runs again,
                // while a raced local run goes on.
                Err(e) if hit && !retried && is_not_found(&e) => {
                    if self.config.verbose {
                        message(&format!("remote cache entry {} ignored: {e:#}", digest.hash));
                    }
                    if self.config.cache_only {
                        return Err(Failure::RunLocally(Box::new(prepared)));
                    }
                    retried = true;
                    (result, hit) = match self.execute_remotely(&client, task, &prepared, true, &mut output).await? {
                        Executed::Done(r, hit) => (*r, hit),
                        Executed::InputsLost(r) => return Ok(r),
                    };
                }
                staged => break staged.map_err(Failure::Infra)?,
            }
        };
        // Racing: its outputs are claimed once fetched, before anything is
        // written on the host. (A command with steps there isn't raced.)
        if race.is_some() {
            claim(race).await?;
            self.fetch_changed(Some(&client), &mut staged, task, &prepared).await.map_err(Failure::Infra)?;
        }
        // Steps on the host before the remote one (deleting an archive
        // before it's made again).
        if let Some(failed) = self.run_local_steps(task, &prepared.local_before, &mut output).await {
            return Ok(failed);
        }
        let keep = self.disk.is_some() && result.exit_code == 0;
        let kind = if hit { ExecKind::RemoteCacheHit } else { ExecKind::Remote };
        match blocking(|| self.write_staged(staged, task, &prepared, &result, keep)) {
            Ok(out) => output.extend(out),
            Err(e) if !prepared.local_before.is_empty() => return Ok(unwritten_after_host_steps(task, &e, output, kind)),
            Err(e) => return Err(Failure::Infra(e)),
        }
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
            // On standard error with the other --remote_verbose lines, not
            // in the command's output.
            let what = if hit { "remote cache hit" } else { "executed remotely" };
            message(&format!("{what} {} ({} inputs{target}): {}", digest.hash, prepared.input_files, task.outputs.join(" ")));
            for w in &prepared.warnings {
                message(&format!("warning: {w}"));
            }
        }
        let status = if result.exit_code == 0 { ExitStatus::Success } else { ExitStatus::Failure(result.exit_code) };
        // The outputs are in the CAS now: build events can point there.
        let cas_outputs = cas_outputs(task, &prepared, &result);
        Ok(TaskResult { edge: task.edge, status, output, kind, cas_outputs, lost_inputs: Vec::new() })
    }

    /// Restores a disk cache hit (`remote`: with outputs left in the remote
    /// CAS, recorded). None when the disk cache can't give its blobs back
    /// (removed, damaged): a miss, and the entry goes, for the result of
    /// running the action to replace.
    async fn restore_disk_hit(
        &self,
        task: &Task,
        race: Option<&Arc<Race>>,
        prepared: &Prepared,
        meta: &ActionMeta,
        result: re::ActionResult,
        remote: bool,
    ) -> Result<Option<TaskResult>, Failure> {
        let digest = &prepared.action_digest;
        // Outputs left remote are recorded as a remote hit's would be
        // (thin archives' members read from the remote CAS). The blobs are
        // fetched first: a miss then has run nothing on the host.
        let client = if remote { self.client.as_ref().map(|c| c.for_action(meta, &digest.hash)) } else { None };
        let mut staged = match self.fetch_outputs(client.as_ref(), task, prepared, &result, !prepared.local_before.is_empty()).await {
            Ok(staged) => staged,
            Err(e) if e.is::<DiskMiss>() => {
                if self.config.verbose {
                    message(&format!("disk cache entry {} ignored: {e}", digest.hash));
                }
                if let Some(disk) = &self.disk {
                    disk.remove_action_result(digest);
                }
                return Ok(None);
            }
            Err(e) => return Err(Failure::Infra(e)),
        };
        if race.is_some() {
            claim(race).await?;
            self.fetch_changed(client.as_ref(), &mut staged, task, prepared).await.map_err(Failure::Infra)?;
        }
        let mut output = Vec::new();
        if let Some(failed) = self.run_local_steps(task, &prepared.local_before, &mut output).await {
            return Ok(Some(failed));
        }
        match blocking(|| self.write_staged(staged, task, prepared, &result, false)) {
            Ok(out) => output.extend(out),
            Err(e) if !prepared.local_before.is_empty() => return Ok(Some(unwritten_after_host_steps(task, &e, output, ExecKind::DiskCacheHit))),
            Err(e) => return Err(Failure::Infra(e)),
        }
        if let Some(mut failed) = self.run_local_steps(task, &prepared.local_after, &mut output).await {
            failed.kind = ExecKind::DiskCacheHit;
            return Ok(Some(failed));
        }
        if self.config.verbose {
            message(&format!("disk cache hit {}: {}", digest.hash, task.outputs.join(" ")));
        }
        // Outputs in the remote CAS: build events can point there.
        let cas_outputs = if remote { cas_outputs(task, prepared, &result) } else { Vec::new() };
        Ok(Some(TaskResult { edge: task.edge, status: ExitStatus::Success, output, kind: ExecKind::DiskCacheHit, cas_outputs, lost_inputs: Vec::new() }))
    }

    /// Uploads the action's inputs and executes it (`skip_cache`: not from
    /// the action cache), past a cached result it can't use. `output`
    /// holds what ran on the host so far.
    async fn execute_remotely(&self, client: &Client, task: &Task, prepared: &Prepared, skip_cache: bool, output: &mut Vec<u8>) -> Result<Executed, Failure> {
        let digest = &prepared.action_digest;
        let minimal = self.config.download_minimal;
        let to_upload = self.shared.store.filter_unknown(prepared_blobs(prepared));
        if let Err(e) = client.upload_blobs(&to_upload).await {
            // Inputs that only existed remotely are gone: rewind.
            if let Some(missing) = e.downcast_ref::<rbe_client::MissingRemoteBlobs>() {
                self.lost.lock().unwrap().extend(prepared.remote_inputs.iter().filter(|(p, _, _)| missing.0.contains(p)).map(|(_, _, d)| d.hash.clone()));
                let lost: Vec<String> = prepared.remote_inputs.iter().filter(|(p, _, _)| missing.0.contains(p)).map(|(_, i, _)| i.clone()).collect();
                if let Some(outputs) = &self.config.remote_outputs {
                    for i in &lost {
                        outputs.forget(i);
                    }
                }
                return Ok(Executed::InputsLost(lost_inputs_result(task, lost, std::mem::take(output))));
            }
            return Err(upload_failure(e));
        }
        self.shared.store.mark_uploaded(to_upload.0.keys());
        let mut resp = client.execute(digest, skip_cache).await.map_err(Failure::Infra)?;
        // FAILED_PRECONDITION: inputs missing from the CAS, which may
        // be remote-only ones believed uploaded.
        if resp.status.as_ref().is_some_and(|st| st.code == tonic_code::FAILED_PRECONDITION) {
            if !prepared.remote_inputs.is_empty() {
                let inputs = prepared.remote_inputs.iter().map(|(_, i, d)| (i.clone(), d.clone())).collect();
                let lost = self.lost_inputs(inputs).await;
                if !lost.is_empty() {
                    return Ok(Executed::InputsLost(lost_inputs_result(task, lost, std::mem::take(output))));
                }
            }
            // Blobs believed uploaded were evicted: upload again, once.
            let all = prepared_blobs(prepared);
            self.shared.store.forget_uploaded(all.0.keys());
            client.upload_blobs(&all).await.map_err(upload_failure)?;
            self.shared.store.mark_uploaded(all.0.keys());
            resp = client.execute(digest, skip_cache).await.map_err(Failure::Infra)?;
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
            && !outputs_in_cas(client, r).await.map_err(Failure::Infra)?
        {
            resp = client.execute(digest, true).await.map_err(Failure::Infra)?;
            if let Some(st) = &resp.status
                && st.code != 0
            {
                return Err(Failure::Infra(anyhow::anyhow!("Execute returned status {}: {}", st.code, st.message)));
            }
        }
        // So does one this action can't use.
        if resp.cached_result
            && let Some(r) = &resp.result
            && check_result(prepared, r).is_err()
        {
            resp = client.execute(digest, true).await.map_err(Failure::Infra)?;
            if let Some(st) = &resp.status
                && st.code != 0
            {
                return Err(Failure::Infra(anyhow::anyhow!("Execute returned status {}: {}", st.code, st.message)));
            }
        }
        let hit = resp.cached_result;
        Ok(Executed::Done(Box::new(resp.result.ok_or_else(|| Failure::Infra(anyhow::anyhow!("Execute returned no result")))?), hit))
    }

    /// Whether this build downloads an output of `task` (`path` relative to
    /// the working directory): all of them, or in minimal mode the
    /// deliverables', those named on the command line, the depfile (which
    /// the build reads), and those of a command whose steps on the host
    /// after the remote one may read them (a post-build copy).
    fn must_download(&self, task: &Task, p: &Prepared, path: &str) -> bool {
        !self.config.download_minimal
            || task.binding("rbe_deliverable") == Some("1")
            || p.depfile.as_ref().is_some_and(|(rel, _)| rel == path)
            || !p.local_after.is_empty()
            || self.config.requested.contains(&crate::remote_outputs::key(path))
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
    /// output: as the runner's local commands, which a stop stops and waits
    /// for (see `host_stop`). Returns the result of the task if a step fails.
    async fn run_local_steps(&self, task: &Task, steps: &[Vec<String>], output: &mut Vec<u8>) -> Option<TaskResult> {
        let stop = ninja_core::runner::host_stop();
        for argv in steps {
            let (status, out) = ninja_core::subprocess::run_argv(argv, &self.shared.build_dir, &[], &stop).await;
            // A step on the host may write what the build doesn't name.
            self.shared.store.host_wrote();
            output.extend_from_slice(&out);
            if status.success() {
                continue;
            }
            return Some(TaskResult {
                edge: task.edge,
                status,
                output: std::mem::take(output),
                kind: ExecKind::Remote,
                cas_outputs: Vec::new(),
                lost_inputs: Vec::new(),
            });
        }
        None
    }

    /// Fetches the outputs of an action result, from the disk cache or the
    /// remote one, into files next to them (but those already up to date,
    /// so that `restat` works, and those left remote): nothing is written
    /// on the host until `write_staged`. `steps_first`: steps on the host
    /// run before then, and may change what's there (delete an output):
    /// those up to date are fetched too.
    async fn fetch_outputs(&self, client: Option<&Client>, task: &Task, p: &Prepared, result: &re::ActionResult, steps_first: bool) -> Result<Staged> {
        let (stdout, stderr) = match client {
            Some(c) => (
                c.output_bytes(&result.stdout_raw, result.stdout_digest.as_ref()).await?,
                c.output_bytes(&result.stderr_raw, result.stderr_digest.as_ref()).await?,
            ),
            // Disk cache entries hold their output inline.
            None => (result.stdout_raw.clone(), result.stderr_raw.clone()),
        };
        // Each output goes where the action asked for it (`check_result`
        // made sure it did), never where the result says.
        let requested = requested_outputs(p);
        let local_of = |path: &str| -> Result<PathBuf> {
            requested.get(path).map(|l| l.to_path_buf()).with_context(|| format!("the result names an output that wasn't requested: {path:?}"))
        };
        let mut files: Vec<(PathBuf, re::OutputFile)> = Vec::new();
        for f in &result.output_files {
            files.push((local_of(&f.path)?, f.clone()));
        }
        // Output directories (unusual for compiles).
        for d in &result.output_directories {
            let Some(client) = client else { bail!("output directories need a remote cache") };
            let base = local_of(&d.path)?;
            let tree_digest = d.tree_digest.clone().unwrap_or_default();
            let tree = client.fetch_tree(&tree_digest).await?;
            for (rel, f) in rbe_client::tree::tree_files(&tree)? {
                if !f.digest.as_ref().is_some_and(rbe_client::valid_digest) {
                    bail!("output directory {}: {} has no well-formed digest", d.path, rel.display());
                }
                let local = base.join(rel);
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
        let minimal = client.is_some() && result.output_directories.is_empty();
        let mut staged =
            Staged { stdout, stderr, files: Vec::new(), fetched: Fetched::default(), minimal, thin: Default::default(), download_anyway: Default::default() };
        // A thin archive left remote is recorded with its members, read with
        // ranged reads, so that the actions reading it get them without
        // downloading it. When that fails, it's downloaded. Archives are
        // recognized by their archiver's flags, or else by their name.
        let thin_command = crate::archive::creates_thin(&p.command.arguments);
        if let Some(c) = client {
            for (local, f) in &files {
                let d = f.digest.clone().unwrap_or_default();
                let named_like_archive = [".a", ".lib"].iter().any(|ext| f.path.to_ascii_lowercase().ends_with(ext));
                if !self.may_stay_remote(&staged, task, p, f)
                    || !(thin_command || named_like_archive)
                    || (!steps_first && blocking(|| same_content(local, &d, &self.shared.store)))
                {
                    continue;
                }
                match self.remote_thin_members(c, &d).await {
                    Ok(Some(members)) => {
                        staged.thin.insert(f.path.clone(), Arc::new(members));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if self.config.verbose {
                            message(&format!("reading the members of {}: {e:#}; downloading it", f.path));
                        }
                        staged.download_anyway.insert(f.path.clone());
                    }
                }
            }
        }
        let needed = blocking(|| {
            let mut needed = Vec::new();
            for (local, f) in &files {
                let d = f.digest.clone().unwrap_or_default();
                if !steps_first && Some(local) != depfile_local.as_ref() && same_content(local, &d, &self.shared.store) {
                    continue;
                }
                if self.stays_remote(&staged, task, p, f) {
                    continue;
                }
                if f.contents.is_empty() {
                    needed.push((d, local.clone()));
                }
            }
            needed
        });
        staged.fetched = self.fetch_blobs(client, &needed).await?;
        staged.files = files;
        Ok(staged)
    }

    /// Racing, once the outputs are claimed: fetches those whose blobs
    /// `fetch_outputs` left out, their files having the result's contents
    /// then, that a local run changed meanwhile (wrote again in place, and
    /// was stopped halfway).
    async fn fetch_changed(&self, client: Option<&Client>, staged: &mut Staged, task: &Task, p: &Prepared) -> Result<()> {
        let needed: Vec<(re::Digest, PathBuf)> = blocking(|| {
            staged
                .files
                .iter()
                .filter(|(local, f)| {
                    let d = f.digest.clone().unwrap_or_default();
                    f.contents.is_empty() && !staged.fetched.has(&d) && !self.stays_remote(staged, task, p, f) && !same_content(local, &d, &self.shared.store)
                })
                .map(|(local, f)| (f.digest.clone().unwrap_or_default(), local.clone()))
                .collect()
        });
        if !needed.is_empty() {
            let more = self.fetch_blobs(client, &needed).await?;
            staged.fetched.merge(more);
        }
        Ok(())
    }

    /// Whether this build may leave an output of a result remote (minimal
    /// downloads; see `must_download`).
    fn may_stay_remote(&self, staged: &Staged, task: &Task, p: &Prepared, f: &re::OutputFile) -> bool {
        staged.minimal && !self.must_download(task, p, &f.path)
    }

    /// Whether it does: a thin archive whose members couldn't be read is
    /// downloaded.
    fn stays_remote(&self, staged: &Staged, task: &Task, p: &Prepared, f: &re::OutputFile) -> bool {
        self.may_stay_remote(staged, task, p, f) && !staged.download_anyway.contains(&f.path)
    }

    /// Writes the outputs `fetch_outputs` fetched, and records those left
    /// remote. Returns the combined stdout/stderr. With `keep`, the result
    /// and its outputs are stored in the disk cache.
    fn write_staged(&self, mut staged: Staged, task: &Task, p: &Prepared, result: &re::ActionResult, keep: bool) -> Result<Vec<u8>> {
        let mut fetched = std::mem::take(&mut staged.fetched);
        let staged = &staged;
        let (stdout, stderr, files) = (&staged.stdout, &staged.stderr, &staged.files);
        let mut left_remote: std::collections::HashSet<&str> = Default::default();
        let stays_remote = |_: &PathBuf, f: &re::OutputFile| self.stays_remote(staged, task, p, f);
        self.write_outputs(p, task.restat, files, &mut fetched, &stays_remote, &staged.thin, &mut left_remote)?;
        // The disk cache keeps the result with the outputs it can hold;
        // those left remote stay in the remote CAS (see `try_execute`).
        // Without them all (the disk can't take them), it doesn't.
        if keep
            && result.output_directories.is_empty()
            && let Some(disk) = &self.disk
        {
            disk_store(&self.disk_warned, || {
                // Fetched blobs are stored already; the others are the
                // local files they matched, or inline.
                for (local, f) in files {
                    let d = f.digest.clone().unwrap_or_default();
                    if left_remote.contains(f.path.as_str()) {
                        continue;
                    }
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
                Ok(disk.put_action_result(&p.action_digest, &stored)?)
            });
        }
        let mut out = stdout.clone();
        if task.deps.as_deref() == Some("msvc") {
            out = self.map_show_includes(&out, task, p);
        }
        out.extend_from_slice(stderr);
        Ok(out)
    }

    /// Writes the outputs of a result that aren't up to date already, and
    /// records those left remote (in `left_remote` too), side outputs
    /// with the action's first output. `restat`: an output left as it was
    /// keeps its time, so that what depends on it doesn't run again.
    #[allow(clippy::too_many_arguments)]
    fn write_outputs<'f>(
        &self,
        p: &Prepared,
        restat: bool,
        files: &'f [(PathBuf, re::OutputFile)],
        fetched: &mut Fetched,
        stays_remote: &dyn Fn(&PathBuf, &re::OutputFile) -> bool,
        thin: &std::collections::HashMap<String, Arc<Vec<String>>>,
        left_remote: &mut std::collections::HashSet<&'f str>,
    ) -> Result<()> {
        let depfile_local = p.depfile.as_ref().map(|(_, l)| l.clone());
        // The file system's time, for outputs left as they were and records
        // (its clock, not the host's: a network file system's may differ).
        let now = |dir: &Path| self.fs_clock.now(dir);
        // The records this result replaces, side outputs it lacks included.
        let previous: HashMap<&str, Entry> = match &self.config.remote_outputs {
            Some(outputs) => {
                let previous = files.iter().filter_map(|(_, f)| outputs.get(&f.path).map(|e| (f.path.as_str(), e))).collect();
                for (rel, _) in &p.outputs {
                    outputs.forget_output(rel);
                }
                previous
            }
            None => HashMap::new(),
        };
        let owner = p.outputs.first().map(|(rel, _)| rel.clone());
        for (local, f) in files {
            self.sweep(local);
            let d = f.digest.clone().unwrap_or_default();
            let is_depfile = Some(local) == depfile_local.as_ref();
            if !is_depfile && same_content(local, &d, &self.shared.store) {
                if let Some(outputs) = &self.config.remote_outputs {
                    outputs.forget(&f.path);
                }
                // Left as is, but made newer than the inputs, as if
                // written: an output older than an input would be dirty
                // forever. Unless restat compares its time, not the inputs'.
                if !restat && let Ok(file) = std::fs::File::options().write(true).open(local) {
                    let _ = file.set_modified(now(local.parent().unwrap_or(Path::new("."))));
                }
                continue;
            }
            if stays_remote(local, f) {
                // A stale local copy must not be what local commands read.
                if local.exists() {
                    std::fs::remove_file(local).with_context(|| format!("removing stale {}", local.display()))?;
                }
                if let Some(outputs) = &self.config.remote_outputs {
                    let mtime = match previous.get(f.path.as_str()) {
                        Some(e) if restat && e.digest == d => e.mtime,
                        _ => ninja_core::disk::system_time_to_ns(now(local.parent().unwrap_or(Path::new(".")))),
                    };
                    let side = p.side_outputs.iter().any(|(rel, _)| *rel == f.path);
                    outputs.record(Entry {
                        path: f.path.clone(),
                        digest: d,
                        executable: f.is_executable,
                        mtime,
                        members: thin.get(&f.path).cloned(),
                        owner: owner.clone().filter(|_| side),
                    });
                }
                left_remote.insert(&f.path);
                continue;
            }
            if let Some(outputs) = &self.config.remote_outputs {
                outputs.forget(&f.path);
            }
            if !f.contents.is_empty() {
                // Inlined contents must be the digest they come with, as
                // fetched blobs are (rbe-client checks those).
                rbe_client::verify(&d, &f.contents).with_context(|| format!("inlined contents of {}", local.display()))?;
                let data = if is_depfile { self.map_depfile(&f.contents, p) } else { f.contents.clone() };
                write_output(local, &data, f.is_executable)?;
            } else if is_depfile {
                write_output(local, &self.map_depfile(&fetched.read(&d)?, p), f.is_executable)?;
            } else {
                fetched.write(local, &d, f.is_executable).with_context(|| format!("writing {}", local.display()))?;
            }
        }
        Ok(())
    }

    async fn materialize_paths(&self, paths: &[String]) -> Result<(), MaterializeError> {
        let Some(outputs) = &self.config.remote_outputs else { return Ok(()) };
        if outputs.is_empty() {
            return Ok(());
        }
        // A local command reading a thin archive reads its members too (but
        // those outside the build directory, read where they are).
        let wanted = blocking(|| -> Result<Vec<(PathBuf, Entry, String)>, MaterializeError> {
            let members = crate::archive::expand(paths, &self.shared.build_dir, Some(outputs), &self.shared.store, crate::archive::Outside::Skip)
                .map_err(MaterializeError::from)?;
            let mut wanted = Vec::new();
            for p in paths.iter().chain(&members) {
                let local = self.shared.build_dir.join(p);
                if local.exists() {
                    continue;
                }
                if let Some(e) = outputs.get(p) {
                    wanted.push((local, e, p.clone()));
                }
            }
            Ok(wanted)
        })?;
        if wanted.is_empty() {
            return Ok(());
        }
        // Each file downloaded by one task, for every task needing it.
        fetch_claimed(
            &self.materializing,
            wanted.iter().collect(),
            |(_, e, _)| e.path.as_str(),
            |(local, _, _)| !local.exists(),
            |mine, claims| self.download_remote_outputs(mine, outputs, claims),
        )
        .await
    }

    /// Downloads and writes remote-only outputs (local path, record, task
    /// input), marking each claimed one done.
    async fn download_remote_outputs(
        &self,
        wanted: Vec<&(PathBuf, Entry, String)>,
        outputs: &RemoteOutputs,
        mut claims: PathClaims<'_>,
    ) -> Result<(), MaterializeError> {
        if wanted.is_empty() {
            return Ok(());
        }
        let needed: Vec<(re::Digest, PathBuf)> = wanted.iter().map(|(local, e, _)| (e.digest.clone(), local.clone())).collect();
        let mut fetched = match self.fetch_blobs(self.client.as_ref(), &needed).await {
            Ok(b) => b,
            Err(e) => {
                // Those gone from the remote CAS are rebuilt (rewinding).
                let lost = self.lost_inputs(wanted.iter().map(|(_, e, p)| (p.to_string(), e.digest.clone())).collect()).await;
                return Err(MaterializeError { message: format!("downloading outputs of remote actions: {e:#}"), lost });
            }
        };
        blocking(|| -> Result<(), MaterializeError> {
            for (local, e, _) in &wanted {
                fetched.write(local, &e.digest, e.executable).map_err(|err| MaterializeError::from(format!("writing {}: {err:#}", local.display())))?;
                crate::remote_outputs::set_mtime(local, e.mtime)
                    .map_err(|err| MaterializeError::from(format!("setting the time of {}: {err}", local.display())))?;
                outputs.forget(&e.path);
                claims.done(&e.path);
            }
            Ok(())
        })
    }

    /// The members of a blob if it's a thin archive: from the disk cache
    /// when it has it whole, reading its head and member list only, else
    /// with ranged reads that skip the symbol table (small archives are
    /// read whole).
    async fn remote_thin_members(&self, client: &Client, d: &re::Digest) -> Result<Option<Vec<String>>> {
        const READ_WHOLE: i64 = 64 * 1024;
        if let Some(file) = self.disk.as_ref().and_then(|disk| disk.blob_file(d))
            && let Ok(members) = blocking(|| crate::archive::read_members(&file))
        {
            return Ok(members);
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
        self.lost.lock().unwrap().extend(missing.iter().cloned());
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

    /// Rewrites a depfile written by the remote compiler to local paths.
    fn map_depfile(&self, data: &[u8], p: &Prepared) -> Vec<u8> {
        map_depfile(data, &|path| p.mapper.map_back(path, &p.tree_index))
    }
    fn map_show_includes(&self, stdout: &[u8], task: &Task, p: &Prepared) -> Vec<u8> {
        map_show_includes(stdout, task, &|path| p.mapper.map_back(path, &p.tree_index), &self.shared.build_dir)
    }
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
    // One write per line: other invocations append to the same file.
    let _ = log.lock().unwrap().write_all(format!("{entry}\n").as_bytes());
}

/// gRPC status codes in `google.rpc.Status`.
mod tonic_code {
    pub const NOT_FOUND: i32 = 5;
    pub const FAILED_PRECONDITION: i32 = 9;
}

/// The local path of each output an action may return (path relative to
/// the working directory, as the action asked for it): its outputs, side
/// outputs and depfile.
fn requested_outputs(p: &Prepared) -> HashMap<&str, &Path> {
    p.outputs.iter().chain(&p.side_outputs).chain(&p.depfile).map(|(rel, local)| (rel.as_str(), local.as_path())).collect()
}

/// Whether a result, from a cache or an execution, is one this action can
/// use. A result names its outputs: one it wasn't asked for would be
/// written wherever it says (an absolute path, `..`), by a server or by
/// whoever can write to a shared cache. So do its digests, which become
/// disk cache paths: each must be a well-formed digest, and each output
/// named once, with one. Symlinks aren't supported. A successful result
/// must hold every declared output, or the old local file would stay and
/// the edge look built (unless a local step after the remote one may
/// write it).
fn check_result(p: &Prepared, result: &re::ActionResult) -> Result<(), String> {
    let requested = requested_outputs(p);
    let mut seen = std::collections::HashSet::new();
    for path in result.output_files.iter().map(|f| &f.path).chain(result.output_directories.iter().map(|d| &d.path)) {
        if !requested.contains_key(path.as_str()) {
            return Err(format!("it names an output that wasn't requested: {path:?}"));
        }
        if !seen.insert(path.as_str()) {
            return Err(format!("it names the output {path} twice"));
        }
    }
    let digests = result
        .output_files
        .iter()
        .map(|f| (f.path.as_str(), f.digest.as_ref()))
        .chain(result.output_directories.iter().map(|d| (d.path.as_str(), d.tree_digest.as_ref())));
    for (path, d) in digests {
        if !d.is_some_and(rbe_client::valid_digest) {
            return Err(format!("its output {path} has no well-formed digest"));
        }
    }
    if let Some(d) = result.stdout_digest.iter().chain(&result.stderr_digest).find(|d| !rbe_client::valid_digest(d)) {
        return Err(format!("its output streams have a malformed digest {:?}", d.hash));
    }
    #[allow(deprecated)]
    let symlink = result.output_symlinks.iter().chain(&result.output_file_symlinks).chain(&result.output_directory_symlinks).next();
    if let Some(l) = symlink {
        return Err(format!("its output {:?} is a symlink, which isn't supported", l.path));
    }
    if result.exit_code == 0 && p.local_after.is_empty() {
        for (rel, _) in &p.outputs {
            if !result.output_files.iter().any(|f| f.path == *rel) && !result.output_directories.iter().any(|d| d.path == *rel) {
                return Err(format!("it lacks the output {rel}"));
            }
        }
    }
    Ok(())
}

/// Whether the CAS still holds all of an action result's outputs.
async fn outputs_in_cas(client: &Client, result: &re::ActionResult) -> Result<bool> {
    let digests: Vec<re::Digest> = result.output_files.iter().filter_map(|f| f.digest.clone()).filter(|d| d.size_bytes > 0).collect();
    if digests.is_empty() {
        return Ok(true);
    }
    Ok(client.find_missing(digests).await?.is_empty())
}

/// A result's outputs, as the task names them, with their digests.
fn cas_outputs(task: &Task, prepared: &Prepared, result: &re::ActionResult) -> Vec<CasOutput> {
    result
        .output_files
        .iter()
        .filter_map(|f| {
            let i = prepared.outputs.iter().position(|(rel, _)| *rel == f.path)?;
            let d = f.digest.as_ref()?;
            Some(CasOutput { path: task.outputs.get(i)?.clone(), hash: d.hash.clone(), size: d.size_bytes })
        })
        .collect()
}

/// The result of a task whose outputs couldn't be written once its steps on
/// the host before the remote one ran: a failure, as a local run would run
/// those again.
fn unwritten_after_host_steps(task: &Task, e: &anyhow::Error, mut output: Vec<u8>, kind: ExecKind) -> TaskResult {
    output.extend_from_slice(format!("ninja: {e:#} (not run locally: its steps on the host ran)\n").as_bytes());
    TaskResult { edge: task.edge, status: ExitStatus::Failure(1), output, kind, cas_outputs: Vec::new(), lost_inputs: Vec::new() }
}

/// The result of a task whose inputs were lost from the remote CAS: the
/// engine rebuilds them and runs the task again.
fn lost_inputs_result(task: &Task, lost: Vec<String>, mut output: Vec<u8>) -> TaskResult {
    output.extend_from_slice(format!("ninja: gone from the remote cache: {}\n", lost.join(", ")).as_bytes());
    TaskResult { edge: task.edge, status: ExitStatus::Failure(1), output, kind: ExecKind::Remote, cas_outputs: Vec::new(), lost_inputs: lost }
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

/// A result's outputs fetched, not yet written (see `Inner::fetch_outputs`).
struct Staged {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Each output's local path, and what the result says of it.
    files: Vec<(PathBuf, re::OutputFile)>,
    fetched: Fetched,
    /// Outputs may stay remote (minimal downloads).
    minimal: bool,
    /// The members of thin archives left remote.
    thin: std::collections::HashMap<String, Arc<Vec<String>>>,
    /// Thin archives downloaded all the same.
    download_anyway: std::collections::HashSet<String>,
}

/// Whether a remote call failed for a certificate refused, connecting again:
/// the server's, or this client's (see `rbe_client::fault::certificate`).
fn certificate_refused(e: &anyhow::Error) -> bool {
    rbe_client::fault::certificate(e.as_ref()).is_some()
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

    /// Actions that couldn't be sent are counted by reason, whatever files
    /// its details name or command line they quote, which the summary
    /// leaves out: what each reason is.
    #[test]
    fn unsent_by_reason() {
        let f = Reasons::default();
        f.add("computed #include in a directory the build writes to: /b/gen/a.c");
        f.add("computed #include in a directory the build writes to: /b/gen/b.c\nmore");
        f.add("cannot find program 'tool'");
        let command = format!("command needs a shell: sleep 0.3; gcc {} -c a.c -o a.o", "-DX ".repeat(1000));
        f.add(&command);
        f.add(&command.replace("a.", "b."));
        assert_eq!(
            f.summary(),
            (5, "2: command needs a shell; 2: computed #include in a directory the build writes to; 1: cannot find program 'tool'".to_string())
        );
        for i in 0..4 {
            f.add(&format!("reason {i}"));
        }
        assert!(f.summary().1.ends_with("; 3 more reasons"), "{}", f.summary().1);
        // A long one without details, cut.
        let long = "x".repeat(5000);
        assert_eq!(Reasons::what(&long), format!("{}...", &long[..REASON_MAX]));
    }

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

    /// Tasks needing one remote-only file: one claims it, the others wait.
    /// Should its download fail, one of them claims it in turn while the
    /// other waits again: never two downloading it at once.
    #[test]
    fn waiters_claim_after_a_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let map: ClaimMap = Default::default();
        let file = "obj/a.o".to_string();
        let there = std::sync::Mutex::new(false);
        let (calls, active, most) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
        let go = tokio::sync::Notify::new();
        let task = || {
            fetch_claimed(
                &map,
                vec![&file],
                |f| f.as_str(),
                |_| !*there.lock().unwrap(),
                |mine, mut claims| {
                    let first = calls.fetch_add(1, Ordering::Relaxed) == 0;
                    let (active, most, go, there) = (&active, &most, &go, &there);
                    async move {
                        most.fetch_max(active.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
                        // The first download fails, once the others wait.
                        if first {
                            go.notified().await;
                            active.fetch_sub(1, Ordering::Relaxed);
                            return Err(MaterializeError::from("the download failed".to_string()));
                        }
                        // Long enough for every task to run meanwhile.
                        for _ in 0..5 {
                            tokio::task::yield_now().await;
                        }
                        *there.lock().unwrap() = true;
                        for f in mine {
                            claims.done(f);
                        }
                        active.fetch_sub(1, Ordering::Relaxed);
                        Ok(())
                    }
                },
            )
        };
        let (a, b, c, ()) = rt.block_on(async {
            tokio::join!(task(), task(), task(), async {
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                go.notify_one();
            })
        });
        assert!(a.is_err() && b.is_ok() && c.is_ok());
        assert!(*there.lock().unwrap());
        assert_eq!((calls.into_inner(), most.into_inner()), (2, 1), "downloads, and most at once");
        assert!(map.lock().unwrap().is_empty());
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

    /// Copies of response files unused for a while go, with temporary
    /// files of interrupted writes; recent ones and other files stay.
    #[test]
    fn sweeps_old_rsp_copies() {
        let dir = test_dirs::TestDir::new("ninja-rs-rsp-copies");
        let hash = "0123456789abcdef0123456789abcdef";
        let files = [format!("{hash}.rsp"), format!("{hash}.tmp.12.3"), format!("{}.rsp", "f".repeat(32)), "notes.rsp".into(), format!("{hash}.txt")];
        for f in &files {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let old = std::time::SystemTime::now() - 2 * RSP_COPIES_UNUSED;
        for f in [&files[0], &files[1], &files[3], &files[4]] {
            std::fs::File::options().write(true).open(dir.join(f)).unwrap().set_modified(old).unwrap();
        }
        sweep_rsp_copies(&dir, RSP_COPIES_UNUSED);
        let left: Vec<bool> = files.iter().map(|f| dir.join(f).exists()).collect();
        assert_eq!(left, [false, false, true, true, true]);
        // No directory: nothing to do.
        sweep_rsp_copies(&dir.join("missing"), RSP_COPIES_UNUSED);
    }

    #[test]
    fn make_escaping() {
        assert_eq!(escape_make("a b#c$d"), "a\\ b\\#c$$d");
    }
}
