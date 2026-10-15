//! The ninja-rs executor service.
//!
//! [`Service`] holds everything that should outlive a single build (the
//! remote connection and the caches in [`ninja_remote::Store`]) and runs
//! builds in-process. The CLI creates one and runs one build; a future daemon
//! would keep it alive across builds. There is no global state.

pub mod bes;
pub mod certs;
pub mod daemon;
pub mod exec_runner;
pub mod jobserver;
pub mod nested;
pub mod options;
pub mod rc;
pub mod tools;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ninja_core::build::{BuildConfig, Builder};
use ninja_core::build_log::{Access, BUILD_LOG_FILE, BuildLog, Holder, LOCK_FILE, LogLock};
use ninja_core::canon::canonicalize_path;
use ninja_core::deps_log::{DEPS_LOG_FILE, DepsLog};
use ninja_core::disk::{Disk, RealDisk, StatCounters};
use ninja_core::graph::{ROOT_SCOPE, State};
use ninja_core::parser::{ManifestParser, ParserOptions, PhonyCycleAction, RealFileReader};
use ninja_core::runner::{CommandRunner, DryRunRunner, RemoteBackend};
use ninja_core::status::{Status, StatusPrinter, Verbosity};
use ninja_core::subprocess::Interrupt;
use ninja_remote::remote_outputs::{REMOTE_OUTPUTS_FILE, RemoteDisk, RemoteOutputs};

use crate::bes::{Bes, InvocationIds, ReportingStatus};
use crate::exec_runner::ExecRunner;
use crate::options::Options;
use crate::tools::{ToolCtx, When};

/// Edge bindings of the RBE interface (docs/rbe-interface.md) that runners
/// need.
pub const RBE_BINDINGS: &[&str] = &["rbe_local", "rbe_mnemonic", "rbe_target", "rbe_config", "rbe_artifact", "rbe_deliverable"];

/// The version of the RBE interface this executor implements.
pub const RBE_VERSION: &str = "1";

/// This executor's version (`--version`, build events, remote requests, the
/// daemon's): the package's, with the commit it was built from when the
/// build knew it (`build.rs` asks git; Bazel stamps it with
/// `--config=stamp`), as `0.1.0+1a2b3c4d5e6f`.
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        // An unstamped Bazel build leaves the placeholder.
        match option_env!("NINJA_RS_COMMIT").filter(|c| !c.is_empty() && !c.starts_with('{')) {
            Some(commit) => format!("{}+{commit}", env!("CARGO_PKG_VERSION")),
            None => env!("CARGO_PKG_VERSION").to_string(),
        }
    })
}

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_INTERRUPTED: i32 = 130;

pub struct Service {
    pub store: Arc<ninja_remote::Store>,
    interrupt: Arc<Interrupt>,
    /// Remote connection, kept across builds (keyed by endpoint/instance).
    remote_client: Option<(String, ninja_remote::Client)>,
    /// The remote service found unreachable (keyed the same), and its
    /// endpoint, which builds do without (`--remote_local_fallback`) rather
    /// than wait for again, and so do the invocations their commands run
    /// (see `nested::UNREACHABLE_ENV`).
    remote_unreachable: Option<(String, String)>,
    /// The certificate files warned about as expiring soon, by the current
    /// invocation or those running it (see `nested::TLS_WARNED_ENV`).
    tls_warned: Vec<String>,
    /// Warnings about the CA of a `--tls_certificate` bundle that verified
    /// a server's certificate, expiring soon, as connections find them
    /// (see `options::LoadedTls::ca_check`): the file, and the warning.
    ca_warnings: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// The disk cache, kept across builds (keyed by its directory).
    disk_cache: Option<(String, Arc<ninja_remote::disk_cache::DiskCache>)>,
    digests_loaded: bool,
    remote_verbose: bool,
    /// `--remote_racing` for the current invocation, with its counts.
    racing: Option<exec_runner::Racing>,
    /// `-d stats` metrics of the current invocation: name, count, total time.
    metrics: Vec<(&'static str, u64, Duration)>,
    stat_counters: Option<Arc<StatCounters>>,
    stat_cache: bool,
    /// The current invocation's ids, shared by build events and remote
    /// execution requests.
    ids: InvocationIds,
    /// Set by a one-shot CLI process: skip freeing per-build data at the end
    /// of a build. A long-lived service must leave it unset.
    pub skip_teardown: bool,
    /// Where nested invocations send their build events (the CLI binds it
    /// before starting threads; see `nested`).
    pub relay: Option<nested::Relay>,
    /// The current invocation's build events, which the CLI ends itself
    /// when an interrupt ends the invocation outside a build's loop.
    pub events: Arc<std::sync::Mutex<Option<Arc<Bes>>>>,
    /// The lock on the logs the current invocation writes.
    logs_lock: Option<LogsLock>,
    /// The daemon the current build began with: told of the commands run
    /// on the host (see `daemon::Telling`).
    daemon: Option<Arc<ninja_daemon::Client>>,
}

/// The lock on the logs an invocation writes (see `Service::lock_logs`).
struct LogsLock {
    file: String,
    /// None when an invocation it runs for holds it (or the lock file
    /// can't be opened); not locked when the file system has no locks.
    lock: Option<LogLock>,
    access: Access,
}

impl Service {
    pub fn new(interrupt: Arc<Interrupt>) -> Self {
        Self {
            store: ninja_remote::Store::new(),
            interrupt,
            remote_client: None,
            remote_unreachable: None,
            tls_warned: Vec::new(),
            ca_warnings: Arc::default(),
            disk_cache: None,
            digests_loaded: false,
            remote_verbose: false,
            racing: None,
            metrics: Vec::new(),
            stat_counters: None,
            stat_cache: true,
            ids: InvocationIds::default(),
            skip_teardown: false,
            relay: None,
            events: Arc::default(),
            logs_lock: None,
            daemon: None,
        }
    }

    /// Runs one ninja invocation in the current directory. Returns the exit code.
    pub async fn run(&mut self, opts: &Options) -> i32 {
        self.metrics.clear();
        self.stat_counters = opts.stats.then(Default::default);
        self.stat_cache = !opts.no_stat_cache;
        let code = self.run_invocation(opts).await;
        if opts.stats {
            self.print_metrics();
        }
        code
    }

    /// Times `f` as one occurrence of the `-d stats` metric `name`.
    fn timed<T>(&mut self, name: &'static str, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let result = f();
        let elapsed = start.elapsed();
        match self.metrics.iter_mut().find(|m| m.0 == name) {
            Some(m) => {
                m.1 += 1;
                m.2 += elapsed;
            }
            None => self.metrics.push((name, 1, elapsed)),
        }
        result
    }

    /// Prints the metrics like ninja's `-d stats`.
    fn print_metrics(&self) {
        let mut rows: Vec<(&str, u64, Duration)> = self.metrics.clone();
        if let Some(c) = &self.stat_counters {
            let n = c.count.load(Ordering::Relaxed);
            rows.push(("node stat", n, Duration::from_nanos(c.nanos.load(Ordering::Relaxed))));
        }
        let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max("metric".len());
        let mut text = format!("{:<width$}\t{:<6}\t{:<9}\ttotal (ms)\n", "metric", "count", "avg (us)");
        for (name, count, total) in rows {
            let total_us = total.as_secs_f64() * 1e6;
            let avg = if count > 0 { total_us / count as f64 } else { 0.0 };
            text.push_str(&format!("{name:<width$}\t{count:<6}\t{avg:<9.1}\t{:.1}\n", total_us / 1000.0));
        }
        ninja_core::status::print_out(&text);
    }

    fn real_disk(&self) -> RealDisk {
        RealDisk::with_counters(self.stat_counters.clone())
    }

    async fn run_invocation(&mut self, opts: &Options) -> i32 {
        self.remote_verbose = opts.remote.verbose;
        // Tried again by each invocation (not by the manifest's rebuilds).
        self.remote_unreachable = None;
        self.tls_warned = nested::tls_warned();
        self.racing = opts.remote.racing.then(|| exec_runner::Racing { delay: Duration::from_millis(opts.remote.racing_delay_ms), stats: Arc::default() });
        self.ids = InvocationIds::new(&opts.bes);
        // Build events cover builds and `-t exec`, not the other tools.
        let bes = if opts.bes.enabled() && opts.tool.as_deref().is_none_or(|t| t == "exec") {
            // The certificates are checked before anything connects.
            let mut warnings = Vec::new();
            let (tls, check) = match opts.bes.backend.as_deref().filter(|b| options::uses_tls(b)) {
                Some(_) => match self.load_tls(opts, |w| warnings.push(format!("ninja: warning: {w}\n"))) {
                    Ok(tls) => tls,
                    Err(e) => {
                        ninja_core::status::print_err(&format!("ninja: error: {e}\n"));
                        return EXIT_FAILURE;
                    }
                },
                None => Default::default(),
            };
            let bytestream = opts.remote.endpoint().map(|e| bes::bytestream_prefix(e, &opts.remote.instance_name));
            let b = Bes::start(opts, tls, check, &self.ids, bytestream, self.store.clone(), self.relay.take()).await;
            for w in warnings {
                ninja_core::status::print_err(&w);
                b.message(&w);
            }
            Some(b)
        } else {
            None
        };
        let mut printer = StatusPrinter::new(opts.verbosity, opts.parallelism);
        let format = match &opts.status_format {
            Some(fmt) => printer.set_status_format(fmt),
            // An unknown `NINJA_STATUS` placeholder is fatal, as in ninja,
            // but before the build rather than at its first status line.
            None if opts.tool.is_none() => printer.check_progress_status_format(),
            None => Ok(()),
        };
        if let Err(e) = format {
            ninja_core::status::print_err(&format!("ninja: fatal: {e}\n"));
            return EXIT_FAILURE;
        }
        *self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = bes.clone();
        let mut status = ReportingStatus { printer: &mut printer, bes: bes.clone() };
        let mut code = self.run_build_invocation(opts, &mut status).await;
        self.logs_lock = None;
        // Interrupted, not a command exiting with 130. A certificate refused
        // meanwhile fails the invocation, whatever the build did.
        // Connections made again during the build may have found a CA of a
        // bundle expiring.
        self.warn_ca(|w| {
            ninja_core::status::print_err(&format!("ninja: warning: {w}\n"));
            if let Some(b) = &bes {
                b.message(&format!("ninja: warning: {w}\n"));
            }
        });
        if let Some(b) = bes
            && b.finish(code, code != EXIT_SUCCESS && self.interrupt.is_set()).await
            && code == EXIT_SUCCESS
        {
            code = EXIT_FAILURE;
        }
        self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        code
    }

    /// The TLS settings, for an endpoint connecting with TLS (see
    /// `TlsFlags::load`), and the check of the CA of a bundle that verifies
    /// the server's certificate (see `LoadedTls::ca_check`). A certificate
    /// expiring soon is warned about (`warn`) once per invocation, and not
    /// by the invocations it runs.
    fn load_tls(&mut self, opts: &Options, mut warn: impl FnMut(&str)) -> Result<(ninja_remote::Tls, rbe_client::CaCheck), String> {
        let loaded = opts.tls.load()?;
        let check = loaded.ca_check(self.ca_warnings.clone());
        for (file, warning) in loaded.expiring {
            if !self.tls_warned.contains(&file) {
                warn(&warning);
                self.tls_warned.push(file);
            }
        }
        Ok((loaded.tls, check))
    }

    /// Warns (`warn`) about the CAs of bundles expiring soon that
    /// connections made since found verifying their server's certificate,
    /// as `load_tls` does about the certificates it reads.
    fn warn_ca(&mut self, mut warn: impl FnMut(&str)) {
        let found = std::mem::take(&mut *self.ca_warnings.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        for (file, warning) in found {
            if !self.tls_warned.contains(&file) {
                warn(&warning);
                self.tls_warned.push(file);
            }
        }
    }

    async fn run_build_invocation(&mut self, opts: &Options, status: &mut ReportingStatus<'_>) -> i32 {
        if let Some(tool) = &opts.tool
            && tool == "list"
        {
            ninja_core::status::print_out(&tools::list_tools());
            return 0;
        }
        if let Some(t) = &opts.tool
            && tools::find_tool(t).is_some_and(|i| i.when == When::AfterFlags)
        {
            return match t.as_str() {
                "daemon" => daemon::tool(&opts.tool_args),
                "exec" => {
                    if let Some(b) = status.bes.clone()
                        && let Err(e) = b.connected().await
                    {
                        status.error(&e);
                        return EXIT_FAILURE;
                    }
                    self.warn_ca(|w| status.warning(w));
                    exec(opts, status, &self.interrupt, &self.tls_warned).await
                }
                "wincodepage" => tools::tool_wincodepage(&opts.tool_args),
                _ => tools::tool_restat_after_flags(&opts.tool_args),
            };
        }
        let tool = match &opts.tool {
            Some(t) => match tools::find_tool(t) {
                Some(info) => Some(info),
                None => {
                    status.error(&format!("unknown tool '{t}'"));
                    return EXIT_FAILURE;
                }
            },
            None => None,
        };

        // Limit number of rebuilds, to prevent infinite loops.
        const CYCLE_LIMIT: usize = 100;
        for _cycle in 0..CYCLE_LIMIT {
            let mut state = State::new();
            let parser_opts = ParserOptions {
                phony_cycle_action: if opts.phonycycle_err { PhonyCycleAction::Error } else { PhonyCycleAction::Warn },
                quiet: opts.verbosity == Verbosity::Quiet,
            };
            let reader = RealFileReader;
            let parsed = self.timed(".ninja parse", || ManifestParser::new(&mut state, &reader, parser_opts).load(&opts.input_file));
            if let Err(e) = parsed {
                match e.strip_prefix(ninja_core::parser::FATAL) {
                    Some(fatal) => {
                        ninja_core::status::print_err(&format!("ninja: fatal: {fatal}\n"));
                        if let Some(b) = &status.bes {
                            b.message(&format!("ninja: fatal: {fatal}\n"));
                        }
                    }
                    None => status.error(&e),
                }
                return EXIT_FAILURE;
            }

            // A nested build without remote execution (CMake's compiler
            // checks, as a nested configure runs them) has nothing to add
            // to the outer invocation's events.
            if let Some(b) = &status.bes
                && state.lookup_variable(ROOT_SCOPE, "rbe_version").is_empty()
            {
                b.leave_relay();
            }
            let build_dir = state.lookup_variable(ROOT_SCOPE, "builddir");
            let log_path = if build_dir.is_empty() { BUILD_LOG_FILE.to_string() } else { format!("{build_dir}/{BUILD_LOG_FILE}") };
            let deps_path = if build_dir.is_empty() { DEPS_LOG_FILE.to_string() } else { format!("{build_dir}/{DEPS_LOG_FILE}") };
            let mut build_log = BuildLog::new();
            let mut deps_log = DepsLog::new();

            // Cleaned outputs must not stay recorded as remote ones; the
            // others' records stay.
            let remote_outputs = tool
                .is_some_and(|t| t.name == "clean" || t.name == "cleandead")
                .then(|| RemoteOutputs::load(std::path::Path::new(&in_build_dir(&build_dir, REMOTE_OUTPUTS_FILE))));
            if let Some(t) = tool
                && t.when == When::AfterLoad
            {
                let mut ctx = ToolCtx {
                    state: &mut state,
                    build_log: &mut build_log,
                    deps_log: &mut deps_log,
                    build_log_path: log_path,
                    access: Access::Exclusive,
                    remote_outputs: remote_outputs.as_ref(),
                    dry_run: opts.dry_run,
                    verbose: opts.verbosity == Verbosity::Verbose,
                    quiet: opts.verbosity == Verbosity::Quiet,
                };
                let code = tools::run_tool(t.name, &opts.tool_args, &mut ctx);
                save_remote_outputs(remote_outputs.as_ref(), status);
                return self.finish(code, state, build_log, deps_log);
            }

            if !build_dir.is_empty() && !opts.dry_run && std::fs::create_dir_all(&build_dir).is_err() {
                status.error(&format!("creating build directory {build_dir}"));
                return EXIT_FAILURE;
            }
            // Before they're read: they're this invocation's to write.
            if (tool.is_none() && !opts.dry_run) || tool.is_some_and(|t| t.name == "recompact") {
                self.lock_logs(&build_dir, status).await;
            }
            let build_log_loaded = self.timed(".ninja_log load", || build_log.load(&log_path));
            let deps_log_loaded = self.timed(".ninja_deps load", || deps_log.load(&deps_path));
            for (name, r) in [("build log", build_log_loaded), ("deps log", deps_log_loaded)] {
                match r {
                    Ok(Some(w)) => status.warning(&w),
                    Ok(None) => {}
                    Err(e) => {
                        status.error(&format!("loading {name}: {e}"));
                        return EXIT_FAILURE;
                    }
                }
            }
            let access = self.logs_lock.as_ref().map_or(Access::Exclusive, |l| l.access);
            if let Some(t) = tool
                && t.when == When::AfterLogs
            {
                let mut ctx = ToolCtx {
                    state: &mut state,
                    build_log: &mut build_log,
                    deps_log: &mut deps_log,
                    build_log_path: log_path,
                    access,
                    remote_outputs: remote_outputs.as_ref(),
                    dry_run: opts.dry_run,
                    verbose: opts.verbosity == Verbosity::Verbose,
                    quiet: opts.verbosity == Verbosity::Quiet,
                };
                let code = tools::run_tool(t.name, &opts.tool_args, &mut ctx);
                save_remote_outputs(remote_outputs.as_ref(), status);
                return self.finish(code, state, build_log, deps_log);
            }
            if !opts.dry_run {
                let st = &state;
                let live = |p: &str| is_live(st, p);
                if let Err(e) = build_log.open_for_write(&log_path, &live, access) {
                    status.error(&format!("opening build log: {e}"));
                    return EXIT_FAILURE;
                }
                if let Err(e) = deps_log.open_for_write(&deps_path, &live, access) {
                    status.error(&format!("opening deps log: {e}"));
                    return EXIT_FAILURE;
                }
            }

            let targets = Arc::new(self.load_targets(&state, status));
            // The build's targets are known: the invocation's build events
            // can start (before any regeneration reports its own).
            if let Some(b) = &status.bes {
                b.announce(&bes::patterns(&state, &opts.targets, &targets));
            }
            // Outputs of remote actions that weren't downloaded, from earlier
            // builds (whatever the download mode). A build sharing the logs
            // (see `Access::Shared`) neither rewrites their file nor removes
            // it: the build holding the lock appends to it.
            let remote_outputs = (opts.remote.endpoint().is_some() && !opts.dry_run).then(|| {
                let file = in_build_dir(&build_dir, REMOTE_OUTPUTS_FILE);
                Arc::new(match access {
                    Access::Exclusive => RemoteOutputs::load_compacting(std::path::Path::new(&file)),
                    Access::Shared => RemoteOutputs::load(std::path::Path::new(&file)),
                })
            });
            let requested = requested_paths(&state, &deps_log, &opts.targets);
            // The build event service's connection, made meanwhile, before
            // any command runs: a certificate refused ends the invocation.
            let events = status.bes.clone();
            let (backend, connected) = tokio::join!(self.remote_backend(&state, opts, targets.clone(), remote_outputs.clone(), &requested, status), async {
                match &events {
                    Some(b) => b.connected().await,
                    None => Ok(()),
                }
            });
            if let Err(e) = &connected {
                status.error(e);
            }
            self.warn_ca(|w| status.warning(w));
            let backend = match backend {
                Ok(r) if connected.is_ok() => r,
                Ok(_) => return EXIT_FAILURE,
                Err(e) => {
                    status.error(&e);
                    return EXIT_FAILURE;
                }
            };
            // Without a remote service (none, the disk cache alone, unreachable,
            // or the manifest left RBE), records are moot: the outputs are built again,
            // locally, and records kept would outlive them.
            let remote = backend.as_ref().is_some_and(|b| b.transfers().is_some());
            let remote_outputs = remote_outputs.filter(|_| remote);
            if !remote && !opts.dry_run && access == Access::Exclusive {
                let _ = std::fs::remove_file(in_build_dir(&build_dir, REMOTE_OUTPUTS_FILE));
            }
            if let (Some(b), Some(outputs)) = (&backend, &remote_outputs)
                && !outputs.is_empty()
            {
                match b.forget_evicted_outputs().await {
                    Ok(0) => {}
                    Ok(n) => status.info(&format!("{n} outputs of remote actions are gone from the remote cache: building them again.")),
                    Err(e) => status.warning(&format!("checking outputs of remote actions: {e:#}")),
                }
            }
            let remote: Option<Arc<dyn RemoteBackend>> = backend.clone().map(|b| b as Arc<dyn RemoteBackend>);
            let keep_digests = remote.is_some() || status.bes.is_some();
            if keep_digests && !self.digests_loaded {
                // File digests persist across invocations (keyed by path,
                // size and mtime) so that toolchains, and the artifacts
                // build events report, are hashed once.
                let f = if build_dir.is_empty() {
                    ninja_remote::store::DIGESTS_FILE.to_string()
                } else {
                    format!("{build_dir}/{}", ninja_remote::store::DIGESTS_FILE)
                };
                self.store.load_digests(std::path::Path::new(&f));
                self.digests_loaded = true;
            }

            let config = BuildConfig {
                verbosity: opts.verbosity,
                dry_run: opts.dry_run,
                parallelism: opts.parallelism,
                failures_allowed: opts.failures_allowed,
                explain: opts.explain,
                keep_rsp: opts.keep_rsp,
                keep_depfile: opts.keep_depfile,
                task_bindings: RBE_BINDINGS.iter().map(|s| s.to_string()).collect(),
                // Absolute: `/showIncludes` paths are made relative to it,
                // as ninja does, so that a generated header is the node its
                // edge produces.
                cwd: std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".into()),
            };

            // Attempt to rebuild the manifest before building anything else.
            let regenerated = self
                .rebuild_manifest(
                    &opts.input_file,
                    &mut state,
                    &mut build_log,
                    &mut deps_log,
                    &config,
                    status,
                    remote.clone(),
                    &backend,
                    remote_outputs.as_deref(),
                )
                .await;
            match regenerated {
                Ok(true) => {
                    // In dry_run mode the regeneration will succeed without
                    // changing the manifest forever. Better to return immediately.
                    if opts.dry_run {
                        return EXIT_SUCCESS;
                    }
                    // The next pass reads the records the regeneration made.
                    if let Some(outputs) = &remote_outputs
                        && let Err(e) = outputs.save()
                    {
                        status.warning(&format!("saving {REMOTE_OUTPUTS_FILE}: {e}"));
                    }
                    if self.finish_uploads(&backend, status).await {
                        return EXIT_INTERRUPTED;
                    }
                    // Start the build over with the new manifest.
                    continue;
                }
                Ok(false) => {}
                Err((e, code)) => {
                    status.error(&format!("rebuilding '{}': {e}", opts.input_file));
                    self.finish_uploads(&backend, status).await;
                    return code;
                }
            }

            let digests_file =
                if build_dir.is_empty() { ninja_remote::store::DIGESTS_FILE.to_string() } else { format!("{build_dir}/{}", ninja_remote::store::DIGESTS_FILE) };
            let jobserver_settings = jobserver::Settings {
                parallelism: opts.parallelism,
                explicit_parallelism: opts.explicit_parallelism,
                // Like ninja: any other value doesn't turn the flag off.
                pool: opts.jobserver_pool || state.lookup_variable(ROOT_SCOPE, "enable_jobserver_pool") == "1",
                dry_run: opts.dry_run,
                verbosity: opts.verbosity,
            };
            let code = self
                .run_build(
                    &opts.targets,
                    &mut state,
                    &mut build_log,
                    &mut deps_log,
                    &config,
                    status,
                    remote,
                    targets,
                    backend.clone(),
                    remote_outputs.clone(),
                    &requested,
                    &jobserver_settings,
                )
                .await;
            if let Some(outputs) = &remote_outputs
                && let Err(e) = outputs.save()
            {
                status.warning(&format!("saving {REMOTE_OUTPUTS_FILE}: {e}"));
            }
            if keep_digests {
                // The digests of artifacts build events are still hashing
                // are saved too (but after an interruption: not waited for).
                if let Some(b) = &status.bes {
                    if self.interrupt.is_set() {
                        b.interrupted();
                    }
                    b.settle().await;
                }
                if let Err(e) = self.store.save_digests(std::path::Path::new(&digests_file)) {
                    status.warning(&format!("saving {digests_file}: {e}"));
                }
            }
            // Local results still going to the remote cache, once the records
            // and digests are saved.
            let code = if self.finish_uploads(&backend, status).await && code == EXIT_SUCCESS { EXIT_INTERRUPTED } else { code };
            let (remote_n, hits) = status.printer.remote_counts();
            if remote_n > 0 && opts.verbosity != Verbosity::Quiet {
                status.info(&format!("{remote_n} remote actions ({hits} remote cache hits)."));
            }
            if let Some(racing) = &self.racing
                && opts.verbosity != Verbosity::Quiet
            {
                let (local, remote) =
                    (racing.stats.local.load(std::sync::atomic::Ordering::Relaxed), racing.stats.remote.load(std::sync::atomic::Ordering::Relaxed));
                if local + remote > 0 {
                    status.info(&format!("racing: {local} actions finished first locally, {remote} remotely."));
                }
            }
            if let Some(t) = backend.as_ref().and_then(|b| b.transfers())
                && opts.remote.verbose
                && t.read + t.sent > 0
            {
                let mib = |b: u64| format!("{:.1} MiB", b as f64 / (1 << 20) as f64);
                // On stderr, as the other `--remote_verbose` lines.
                ninja_core::status::message(&format!(
                    "remote transfers: {} downloaded ({} on the wire), {} uploaded ({} on the wire).",
                    mib(t.read),
                    mib(t.wire_read),
                    mib(t.sent),
                    mib(t.wire_sent)
                ));
            }
            // Said once for the client: blobs are uploaded whole since.
            if let Some(why) = self.remote_client.as_ref().and_then(|(_, c)| c.splicing_refused()) {
                status.warning(&format!("the remote service refused to splice blobs, which are uploaded whole: {why}"));
            }
            if let Some(e) = backend.as_ref().and_then(|b| b.daemon_error()) {
                status.warning(&format!("the daemon failed ({e}): the build did its work itself"));
            }
            // Remote actions that ran locally, which is otherwise silent.
            if let Some(b) = &backend
                && let Some((summary, after_failures)) = b.fallback_summary()
                && opts.verbosity != Verbosity::Quiet
            {
                if after_failures {
                    status.warning(&summary);
                } else {
                    status.info(&summary);
                }
            }
            // Local results that weren't cached, which is otherwise silent too.
            if let Some(summary) = backend.as_ref().and_then(|b| b.uncached_summary())
                && opts.verbosity != Verbosity::Quiet
            {
                status.info(&summary);
            }
            let disk_hits = status.printer.disk_cache_hits();
            if disk_hits > 0 && opts.verbosity != Verbosity::Quiet {
                status.info(&format!("{disk_hits} disk cache hits."));
            }
            self.evict_disk_cache(opts, status);
            return self.finish(code, state, build_log, deps_log);
        }
        status.error(&format!("manifest '{}' still dirty after {CYCLE_LIMIT} tries", opts.input_file));
        EXIT_FAILURE
    }

    /// Waits for the local results the backend sends to the remote cache
    /// (see `Backend::finish_uploads`), saying so when that takes a while.
    /// An interrupt, before the wait or during it, gives them up: whether
    /// some were (said then; one landing as the build ended too, which then
    /// exits interrupted, not as if they were stored).
    async fn finish_uploads(&self, backend: &Option<Arc<ninja_remote::Backend>>, status: &mut dyn Status) -> bool {
        let Some(b) = backend else { return false };
        // The wait acts on interrupts: they don't end the invocation at once.
        let _handling = self.interrupt.handle();
        let before = self.interrupt.is_set();
        let interrupt = &self.interrupt;
        let stop = async move {
            if !before {
                interrupt.wait().await;
            }
        };
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let left = b.finish_uploads(stop, |n| status.info(&format!("waiting for {n} upload{} to the remote cache.", plural(n)))).await;
        if left == 0 {
            return false;
        }
        status.info(&format!("interrupted: {left} local result{} not stored in the remote cache.", plural(left)));
        true
    }

    /// Takes the lock on the logs in `build_dir` for this invocation (see
    /// `LogLock`; ninja has none), unless it holds it already. A build
    /// holding it is waited for: ninja users start builds in one directory
    /// at once by mistake (an IDE's and a terminal's), and the second then
    /// finds the work done. An invocation this one runs for (a generator
    /// running `-t restat` while the build running it waits, a build of
    /// another configuration that one of its commands runs) lends it
    /// instead, and appends to the logs only (see `Access::Shared`). A lock
    /// that can't be taken (a file system without locks) is a warning.
    async fn lock_logs(&mut self, build_dir: &str, status: &mut dyn Status) {
        let file = in_build_dir(build_dir, LOCK_FILE);
        if self.logs_lock.as_ref().is_some_and(|l| l.file == file) {
            return;
        }
        let mut waiting = false;
        let (lock, access) = loop {
            match try_lock_logs(&file) {
                Ok(LogsAccess::Locked(lock)) => break (Some(lock), Access::Exclusive),
                Ok(LogsAccess::Inherited) => break (None, Access::Shared),
                Ok(LogsAccess::Busy(holder)) => {
                    if !waiting {
                        status.info(&waiting_message(holder.as_ref()));
                        waiting = true;
                    }
                    // An interrupt meanwhile ends the invocation (see
                    // `Interrupt::unhandled`).
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => {
                    status.warning(&format!("can't lock the logs ({file}: {e}): a build running at once could undo this one's records"));
                    // Not locked, but builds this one runs know it is
                    // theirs to append to.
                    break (LogLock::open(&file).ok(), Access::Exclusive);
                }
            }
        };
        self.logs_lock = Some(LogsLock { file, lock, access });
    }

    /// Ends an invocation. With `skip_teardown` the process exits next, so
    /// freeing the graph and the logs' tables is wasted work (ninja skips it
    /// too): log records are already written, and the OS closes the handles.
    fn finish(&self, code: i32, state: State, build_log: BuildLog, deps_log: DepsLog) -> i32 {
        if self.skip_teardown {
            std::mem::forget(state);
            std::mem::forget(build_log);
            std::mem::forget(deps_log);
        }
        code
    }

    /// Creates the remote backend when the manifest opts in (`rbe_version`)
    /// and an executor was given.
    async fn remote_backend(
        &mut self,
        state: &State,
        opts: &Options,
        targets: Arc<ninja_remote::targets::Targets>,
        remote_outputs: Option<Arc<RemoteOutputs>>,
        requested: &[String],
        status: &mut dyn Status,
    ) -> Result<Option<Arc<ninja_remote::Backend>>, String> {
        self.daemon = None;
        if opts.remote.endpoint().is_none() && opts.remote.disk_cache.is_none() {
            return Ok(None);
        }
        if opts.dry_run {
            return Ok(None);
        }
        match state.lookup_variable(ROOT_SCOPE, "rbe_version").as_str() {
            RBE_VERSION => {}
            "" => {
                if opts.remote.verbose {
                    ninja_core::status::message("remote execution disabled: the manifest does not set rbe_version");
                }
                return Ok(None);
            }
            other => {
                status.warning(&format!("remote execution disabled: the manifest uses RBE interface version {other}, this executor supports {RBE_VERSION}"));
                return Ok(None);
            }
        }
        let disk = self.open_disk_cache(opts, status);
        // With --remote_local_fallback, a remote service that can't be
        // reached is done without: the build runs locally (with the disk
        // cache, if any), as its actions would once remote ones failed.
        let fallback = opts.remote.local_fallback && !opts.remote.strict;
        let key = format!("{}|{}|{:?}|{:?}", opts.remote.endpoint().cloned().unwrap_or_default(), opts.remote.instance_name, opts.remote.headers, opts.tls);
        let client = match opts.remote.endpoint() {
            Some(_) if fallback && self.remote_unreachable.as_ref().is_some_and(|(k, _)| *k == key) => None,
            // Not tried again: the same rules apply.
            Some(endpoint) if nested::unreachable_endpoints().contains(endpoint) => {
                self.unreachable(&key, endpoint, "the invocation running this one found it unreachable", opts, status)?;
                None
            }
            Some(endpoint) => {
                if self.remote_client.as_ref().is_none_or(|(k, _)| *k != key) {
                    // Certificates are checked before connecting: one that
                    // expired fails the build, unreachable or not.
                    let (tls, check) = if options::uses_tls(endpoint) { self.load_tls(opts, |w| status.warning(w))? } else { Default::default() };
                    let connect = tls.connect_options(endpoint, &opts.remote.instance_name, &opts.remote.headers);
                    match ninja_remote::Client::connect_checked(connect, opts.remote.retries, check).await {
                        Ok(client) => self.remote_client = Some((key.clone(), client.with_max_concurrency(opts.remote.max_concurrency))),
                        // No connection, or no answer: done without. A
                        // certificate refused, or the settings, aren't.
                        Err(e) if rbe_client::fault::fault(e.as_ref()).unreachable() => {
                            self.unreachable(&key, endpoint, &rbe_client::fault::describe(e.as_ref()), opts, status)?
                        }
                        Err(e) => {
                            let e = rbe_client::fault::describe(e.as_ref());
                            return Err(format!("remote execution: {e}{}", options::tls_hint(&e, endpoint, &opts.tls)));
                        }
                    }
                }
                // Requests carry the invocation's ids, which build events
                // report too.
                self.remote_client.as_ref().filter(|(k, _)| *k == key).map(|(_, client)| {
                    client
                        .clone()
                        .with_request_metadata(rbe_client::proto::RequestMetadata {
                            tool_details: Some(rbe_client::proto::ToolDetails { tool_name: "ninja-rs".into(), tool_version: version().into() }),
                            tool_invocation_id: self.ids.invocation_id.clone(),
                            correlated_invocations_id: self.ids.build_request_id.clone(),
                            ..Default::default()
                        })
                        .with_compression(opts.remote.cache_compression)
                        .with_retries(opts.remote.retries, opts.remote.timeout)
                })
            }
            None => None,
        };
        if client.is_none() && disk.is_none() {
            return Ok(None);
        }
        let build_dir = std::env::current_dir().map_err(|e| e.to_string())?;
        let volatile = volatile_dirs(state, &build_dir);
        let written = written_dirs(state, &build_dir);
        let file_var = |name: &str| Some(state.lookup_variable(ROOT_SCOPE, name)).filter(|v| !v.is_empty());
        let daemon = if opts.remote.daemon {
            let begin = ninja_daemon::proto::Begin {
                build_dir: build_dir.clone(),
                env: std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect(),
                profile_dir: disk.as_ref().map(|d| d.profile_dir()),
                digests_file: Some(build_dir.join(in_build_dir(&state.lookup_variable(ROOT_SCOPE, "builddir"), ninja_remote::store::DIGESTS_FILE))),
                volatile: volatile.clone(),
            };
            let started = tokio::task::spawn_blocking(move || {
                let client = daemon::connect()?;
                client.begin(begin)?;
                std::io::Result::Ok(client)
            })
            .await
            .map_err(|e| e.to_string())?;
            match started {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    status.warning(&format!("the daemon: {e}; building without it"));
                    None
                }
            }
        } else {
            None
        };
        self.daemon = daemon.clone();
        // Opened for appending, so that nested invocations' lines (see
        // `nested`) and this one's don't overwrite each other.
        let execution_log = match &opts.remote.execution_log {
            Some(f) => {
                if !opts.remote.execution_log_append {
                    std::fs::File::create(f).map_err(|e| format!("creating {f}: {e}"))?;
                }
                let file = std::fs::File::options().create(true).append(true).open(f).map_err(|e| format!("opening {f}: {e}"))?;
                Some(Arc::new(std::sync::Mutex::new(file)))
            }
            None => None,
        };
        let requested: Arc<std::collections::HashSet<String>> = Arc::new(requested.iter().map(|p| ninja_remote::remote_outputs::key(p)).collect());
        let config = |remote_outputs: Option<Arc<RemoteOutputs>>| ninja_remote::BackendConfig {
            build_dir: build_dir.clone(),
            volatile: outermost(&volatile),
            written: written.clone(),
            toolchains_file: file_var("rbe_toolchains"),
            targets: targets.clone(),
            exec_root: opts.remote.exec_root.clone().map(Into::into),
            jobs: opts.remote.jobs,
            mnemonic_suffixes: opts.remote.mnemonic_suffixes.clone(),
            local_fallback: opts.remote.local_fallback,
            strict: opts.remote.strict,
            max_fallbacks: opts.remote.max_fallbacks,
            max_fallback_ratio: opts.remote.max_fallback_ratio,
            cache_only: opts.remote.cache_only(),
            upload_local_results: opts.remote.upload_local_results,
            verbose: opts.remote.verbose,
            remote_outputs,
            download_minimal: opts.remote.download_minimal,
            requested: requested.clone(),
            execution_log: execution_log.clone(),
            include_scanner: opts.remote.include_scanner,
            daemon: daemon.clone(),
            action_env: opts.remote.action_env(),
        };
        let with_remote = client.is_some();
        // Records of remote outputs only go with a remote service.
        let remote_outputs = remote_outputs.filter(|_| with_remote);
        let backend = match ninja_remote::Backend::new(config(remote_outputs), client, disk.clone(), self.store.clone()).await {
            Ok(b) => b,
            // The service didn't answer its first call (GetCapabilities),
            // the connection was gone, or it said it was unavailable (after
            // the retries): unreachable.
            Err(e) if with_remote && (rbe_client::fault::fault(e.as_ref()).unreachable() || rbe_client::fault::unavailable(e.as_ref())) => {
                let endpoint = opts.remote.endpoint().cloned().unwrap_or_default();
                self.unreachable(&key, &endpoint, &rbe_client::fault::describe(e.as_ref()), opts, status)?;
                if disk.is_none() {
                    return Ok(None);
                }
                ninja_remote::Backend::new(config(None), None, disk, self.store.clone()).await.map_err(|e| format!("remote execution: {e:#}"))?
            }
            Err(e) => {
                let e = rbe_client::fault::describe(e.as_ref());
                let hint = opts.remote.endpoint().map(|ep| options::tls_hint(&e, ep, &opts.tls)).unwrap_or_default();
                return Err(format!("remote execution: {e}{hint}"));
            }
        };
        if opts.remote.cache_only() && opts.remote.upload_local_results && !backend.uploads_local_results() {
            status.warning("the remote cache doesn't accept results from clients: local results aren't stored");
        }
        Ok(Some(Arc::new(backend)))
    }

    /// The remote service `key` names (at `endpoint`) couldn't be reached
    /// (`error`): with `--remote_local_fallback` (and not
    /// `--remote_strict`), a warning, and builds do without it; else the
    /// build's error.
    fn unreachable(&mut self, key: &str, endpoint: &str, error: &str, opts: &Options, status: &mut dyn Status) -> Result<(), String> {
        let hint = options::tls_hint(error, endpoint, &opts.tls) + &options::plaintext_hint(error, endpoint);
        if !opts.remote.local_fallback || opts.remote.strict {
            let local = if opts.remote.strict { "" } else { " (with --remote_local_fallback, the build would run locally)" };
            return Err(format!("the remote service {endpoint} is unreachable{local}: {error}{hint}"));
        }
        status.warning(&format!("the remote service {endpoint} is unreachable, building locally: {error}{hint}"));
        self.remote_unreachable = Some((key.to_string(), endpoint.to_string()));
        Ok(())
    }

    /// The `--disk_cache`, opened once per directory. A cache that can't be
    /// opened is a warning: the build goes on without it.
    fn open_disk_cache(&mut self, opts: &Options, status: &mut dyn Status) -> Option<Arc<ninja_remote::disk_cache::DiskCache>> {
        let dir = opts.remote.disk_cache.as_ref()?;
        if let Some((d, cache)) = &self.disk_cache
            && d == dir
        {
            return Some(cache.clone());
        }
        match ninja_remote::disk_cache::DiskCache::open(std::path::Path::new(dir)) {
            Ok(cache) => {
                let cache = Arc::new(cache);
                self.disk_cache = Some((dir.clone(), cache.clone()));
                Some(cache)
            }
            Err(e) => {
                status.warning(&format!("disk cache {dir} disabled: {e}"));
                None
            }
        }
    }

    /// Keeps the disk cache within `--disk_cache_max_size` (down to 90% of
    /// it, so that eviction doesn't run after every build), once a build
    /// wrote to it. The cache is only walked when its size estimate passes
    /// the limit.
    fn evict_disk_cache(&self, opts: &Options, status: &mut dyn Status) {
        let Some((_, cache)) = &self.disk_cache else { return };
        if cache.bytes_written() == 0 {
            return;
        }
        let max = opts.remote.disk_cache_max_size;
        match cache.collect_garbage(max, max / 10 * 9) {
            Ok(Some((files, bytes))) if files > 0 && opts.remote.verbose => {
                ninja_core::status::message(&format!("disk cache: evicted {files} entries ({} MiB).", bytes >> 20));
            }
            Ok(_) => {}
            Err(e) => status.warning(&format!("disk cache eviction: {e}")),
        }
    }

    /// The `rbe_targets` file of an RBE manifest (empty otherwise, or when it
    /// can't be read).
    fn load_targets(&self, state: &State, status: &mut dyn Status) -> ninja_remote::targets::Targets {
        let file = state.lookup_variable(ROOT_SCOPE, "rbe_targets");
        if file.is_empty() || state.lookup_variable(ROOT_SCOPE, "rbe_version") != RBE_VERSION {
            return Default::default();
        }
        ninja_remote::targets::Targets::load(std::path::Path::new(&file)).unwrap_or_else(|e| {
            status.warning(&e);
            Default::default()
        })
    }

    fn make_runner(
        &self,
        config: &BuildConfig,
        remote: Option<Arc<dyn RemoteBackend>>,
        verbose_remote: bool,
        jobserver: Option<&jobserver::Jobserver>,
    ) -> Box<dyn CommandRunner> {
        if config.dry_run {
            return Box::new(DryRunRunner::default());
        }
        let client = jobserver.and_then(|j| j.client.clone());
        // With a jobserver, the pool's size bounds how many local tasks
        // wait for a token.
        let parallelism = match &client {
            Some(_) => jobserver.and_then(|j| j.jobs).unwrap_or(config.parallelism),
            None => config.parallelism,
        };
        let racing = self.racing.clone().filter(|_| remote.is_some());
        let mut r = ExecRunner::new(parallelism, remote, self.interrupt.clone());
        r.verbose_remote = verbose_remote;
        r.racing = racing;
        if let Some(c) = client {
            r.use_jobserver(c);
        }
        let mut env = Vec::new();
        if let Some(flags) = jobserver.and_then(|j| j.makeflags.clone()) {
            env.push(("MAKEFLAGS".into(), flags));
        }
        // Commands writing the logs again (a generator's `-t restat`, a
        // build of another configuration) do so for this invocation, and
        // for those it runs for: X's command builds Y, whose builds X.
        if let Some(lock) = self.logs_lock.as_ref().and_then(|l| l.lock.as_ref())
            && let Ok(id) = lock.identity()
        {
            let mut held = held_locks();
            held.push(id);
            env.push((nested::LOGS_LOCK_ENV.into(), held.iter().map(|(volume, file)| format!("{volume:x}:{file:x}")).collect::<Vec<_>>().join(",")));
        }
        // Invocations commands run don't try a remote service this one
        // found unreachable again.
        if let Some((_, endpoint)) = &self.remote_unreachable {
            env.push((nested::UNREACHABLE_ENV.into(), nested::with_unreachable(endpoint)));
        }
        // Nor warn about the certificates this one warned about.
        if !self.tls_warned.is_empty() {
            env.push((nested::TLS_WARNED_ENV.into(), nested::tls_warned_value(&self.tls_warned)));
        }
        // Nor say again that its disk cache is Bazel's.
        if let Some(noted) = self.disk_cache.as_ref().and_then(|(_, c)| c.noted_env()) {
            env.push((ninja_remote::disk_cache::NOTED_ENV.into(), noted));
        }
        if !env.is_empty() {
            r.set_env(env);
        }
        match &self.daemon {
            Some(client) => Box::new(daemon::Telling { inner: Box::new(r), client: client.clone() }),
            None => Box::new(r),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn rebuild_manifest(
        &self,
        input_file: &str,
        state: &mut State,
        build_log: &mut BuildLog,
        deps_log: &mut DepsLog,
        config: &BuildConfig,
        status: &mut dyn Status,
        remote: Option<Arc<dyn RemoteBackend>>,
        backend: &Option<Arc<ninja_remote::Backend>>,
        remote_outputs: Option<&RemoteOutputs>,
    ) -> Result<bool, (String, i32)> {
        if input_file.is_empty() {
            return Err(("empty path".into(), EXIT_FAILURE));
        }
        let (path, _) = canonicalize_path(input_file);
        let Some(node) = state.lookup_node(&path) else { return Ok(false) };
        // As the build's (see `run_build`).
        let real = self.real_disk();
        let fetch = remote_fetch(backend.clone());
        let remote_disk = remote_outputs.map(|outputs| RemoteDisk { inner: &real, outputs, fetch: Some(&fetch) });
        let disk: &dyn Disk = match &remote_disk {
            Some(d) => d,
            None => &real,
        };
        let mut runner = self.make_runner(config, remote, false, None);
        {
            let mut builder = Builder::new(state, disk, Some(build_log), Some(deps_log), config, status);
            builder.add_target(node).map_err(|e| (e, EXIT_FAILURE))?;
            if builder.already_up_to_date() {
                return Ok(false); // Not an error, but we didn't rebuild.
            }
            // Its loop acts on interrupts.
            let _handling = self.interrupt.handle();
            builder.build(runner.as_mut()).await.map_err(|e| {
                let code = if e.interrupted { EXIT_INTERRUPTED } else { EXIT_FAILURE };
                (e.message, code)
            })?;
        }
        // The manifest was only rebuilt if it is now dirty (it may have been
        // cleaned by a restat).
        if !state.node(node).dirty {
            state.reset();
            return Ok(false);
        }
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_build(
        &self,
        targets: &[String],
        state: &mut State,
        build_log: &mut BuildLog,
        deps_log: &mut DepsLog,
        config: &BuildConfig,
        status: &mut ReportingStatus<'_>,
        remote: Option<Arc<dyn RemoteBackend>>,
        targets_file: Arc<ninja_remote::targets::Targets>,
        backend: Option<Arc<ninja_remote::Backend>>,
        remote_outputs: Option<Arc<RemoteOutputs>>,
        requested: &[String],
        jobserver_settings: &jobserver::Settings,
    ) -> i32 {
        let bes = status.bes.clone();
        let nodes = match tools::collect_targets(state, Some(deps_log), targets) {
            Ok(n) => n,
            Err(e) => {
                status.error(&e);
                return EXIT_FAILURE;
            }
        };
        let real = self.real_disk();
        // Outputs of remote actions that weren't downloaded count as
        // present; a file the build itself reads (a dyndep file) is
        // downloaded then.
        let fetch = remote_fetch(backend.clone());
        let remote_disk = remote_outputs.as_deref().map(|outputs| RemoteDisk { inner: &real, outputs, fetch: Some(&fetch) });
        let disk: &dyn Disk = match &remote_disk {
            Some(d) => d,
            None => &real,
        };
        let verbose_remote = remote.is_some() && (self.remote_verbose || config.verbosity == Verbosity::Verbose);
        let jobserver = jobserver::setup(jobserver_settings, std::env::var("MAKEFLAGS").ok(), status);
        let mut runner = self.make_runner(config, remote, verbose_remote, Some(&jobserver));
        let mut builder = Builder::new(state, disk, Some(build_log), Some(deps_log), config, status);
        // Like ninja: cache stats while scanning, but not while building, so
        // restat and later edges see the outputs' new timestamps.
        disk.allow_stat_cache(self.stat_cache);
        for &n in &nodes {
            if let Err(e) = builder.add_target(n) {
                disk.allow_stat_cache(false);
                builder.status.error(&e);
                return EXIT_FAILURE;
            }
        }
        disk.allow_stat_cache(false);
        if let Some(b) = &bes {
            let patterns = bes::patterns(builder.state, targets, &targets_file);
            b.plan(builder.state, &builder.plan, &nodes, targets_file, &patterns, remote_outputs.as_deref());
        }
        let code = if builder.already_up_to_date() {
            if config.verbosity != Verbosity::NoStatusUpdate {
                builder.status.info("no work to do.");
            }
            EXIT_SUCCESS
        } else {
            // Its loop acts on interrupts.
            let _handling = self.interrupt.handle();
            match builder.build(runner.as_mut()).await {
                Ok(()) => EXIT_SUCCESS,
                Err(e) => {
                    if !e.interrupted {
                        builder.status.failure_summary();
                    }
                    builder.status.info(&format!("build stopped: {}.", e.message));
                    if e.interrupted { EXIT_INTERRUPTED } else { failure_code(e.exit_code) }
                }
            }
        };
        // What the command line names is downloaded, even when it was up
        // to date.
        if code == EXIT_SUCCESS
            && let Some(b) = &backend
            && let Err(e) = b.materialize_paths(requested).await
        {
            builder.status.error(&format!("{e:#}"));
            return EXIT_FAILURE;
        }
        code
    }
}

/// Downloads a file only the remote has, for a build reading it itself (a
/// dyndep file): see `RemoteDisk`.
fn remote_fetch(backend: Option<Arc<ninja_remote::Backend>>) -> impl Fn(&str) -> Result<(), String> + Send + Sync {
    let handle = tokio::runtime::Handle::current();
    move |path: &str| -> Result<(), String> {
        let Some(b) = &backend else { return Ok(()) };
        let paths = vec![path.to_string()];
        tokio::task::block_in_place(|| handle.block_on(b.materialize_paths(&paths))).map_err(|e| format!("{e:#}"))
    }
}

/// `file` in the manifest's `builddir` (the current directory if unset).
/// `-t exec [--] COMMAND [ARGS...]`: runs a command (a configure building
/// external projects, a script) whose ninja-rs invocations take this one's
/// options and report under its invocation, as nested invocations do (see
/// `nested`). Returns the command's exit code.
async fn exec(opts: &Options, status: &mut ReportingStatus<'_>, interrupt: &Arc<Interrupt>, tls_warned: &[String]) -> i32 {
    let args = match opts.tool_args.split_first() {
        Some((dashes, rest)) if dashes == "--" => rest,
        _ => &opts.tool_args[..],
    };
    let Some((program, rest)) = args.split_first() else {
        status.error("usage: ninja-rs [options] -t exec [--] COMMAND [ARGS...]");
        return EXIT_FAILURE;
    };
    if let Some(b) = &status.bes {
        b.announce(&[]);
    }
    // The execution log starts here: nested invocations append to it.
    if let Some(log) = &opts.remote.execution_log
        && !opts.remote.execution_log_append
        && let Err(e) = std::fs::File::create(log)
    {
        status.warning(&format!("can't write the execution log {log}: {e}"));
    }
    let mut command = tokio::process::Command::new(program);
    command.args(rest);
    // The invocations it runs don't warn about the certificates this one did.
    if !tls_warned.is_empty() {
        command.env(nested::TLS_WARNED_ENV, nested::tls_warned_value(tls_warned));
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            status.error(&format!("{program}: {e}"));
            return 127;
        }
    };
    // Interrupts are the command's to act on: it gets those the terminal
    // didn't send it too (see `forward`); another one kills it.
    let _handling = interrupt.handle();
    let mut seen = 0;
    let waited = loop {
        tokio::select! {
            s = child.wait() => break s,
            _ = interrupt.wait_beyond(seen) => {
                seen = interrupt.count();
                if seen == 1 {
                    forward(&child, interrupt);
                } else {
                    let _ = child.start_kill();
                }
            }
        }
    };
    match waited {
        Ok(s) => match s.code() {
            Some(code) => code,
            None => {
                #[cfg(unix)]
                if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&s) {
                    return 128 + signal;
                }
                EXIT_FAILURE
            }
        },
        Err(e) => {
            status.error(&format!("waiting for {program}: {e}"));
            EXIT_FAILURE
        }
    }
}

/// Passes an interrupt on to `-t exec`'s command, which shares the terminal
/// with ninja-rs: SIGTERM always (a terminal never sends it), SIGINT and
/// SIGHUP unless the terminal sent them to its foreground process group,
/// which holds the command too: not when a process sent them (`kill(2)`,
/// to ninja-rs alone), or ninja-rs isn't in the foreground. On Windows,
/// Ctrl-C and Ctrl-Break reach every process of the console already.
fn forward(child: &tokio::process::Child, interrupt: &Interrupt) {
    #[cfg(unix)]
    {
        let signal = interrupt.last_signal();
        if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok())
            && (signal == libc::SIGTERM || interrupt.last_sent_by_process() == Some(true) || !in_terminal_foreground())
        {
            // SAFETY: plain system call; the child isn't reaped yet.
            unsafe { libc::kill(pid, signal) };
        }
    }
    #[cfg(not(unix))]
    let _ = (child, interrupt);
}

/// Whether this process is in its terminal's foreground process group.
#[cfg(unix)]
fn in_terminal_foreground() -> bool {
    let Ok(tty) = std::fs::File::open("/dev/tty") else { return false };
    // SAFETY: plain system calls on an open descriptor.
    unsafe { libc::tcgetpgrp(std::os::fd::AsRawFd::as_raw_fd(&tty)) == libc::getpgrp() }
}

/// What a build whose last failed command exited with `code` exits with:
/// that code (as ninja 1.13 does), unless it would read as success.
fn failure_code(code: i32) -> i32 {
    if code == 0 || (cfg!(unix) && code & 0xff == 0) { EXIT_FAILURE } else { code }
}

/// How an invocation may write the logs whose lock file is `file`.
pub(crate) enum LogsAccess {
    /// This process holds the lock.
    Locked(LogLock),
    /// An invocation this one runs for holds it (see `nested::LOGS_LOCK_ENV`).
    Inherited,
    /// Another build holds it, as far as the lock file says.
    Busy(Option<Holder>),
}

/// Takes the lock file `file` of a build directory's logs, if it is free.
pub(crate) fn try_lock_logs(file: &str) -> std::io::Result<LogsAccess> {
    let mut lock = LogLock::open(file)?;
    let inherited = |lock: &LogLock| lock.identity().is_ok_and(|id| held_locks().contains(&id));
    match lock.lock(false) {
        Ok(true) => Ok(LogsAccess::Locked(lock)),
        // Also on a file system without locks.
        Ok(false) | Err(_) if inherited(&lock) => Ok(LogsAccess::Inherited),
        Ok(false) => match lock.holder() {
            // The build running this one, whose commands lost
            // `LOGS_LOCK_ENV` (a wrapper scrubbing the environment).
            Some(holder) if nested::runs_this(&holder) => Ok(LogsAccess::Inherited),
            holder => Ok(LogsAccess::Busy(holder)),
        },
        Err(e) => Err(e),
    }
}

/// The identities of the lock files the invocations this one runs for
/// hold (see `nested::LOGS_LOCK_ENV`).
fn held_locks() -> Vec<(u64, u128)> {
    let held = std::env::var(nested::LOGS_LOCK_ENV).unwrap_or_default();
    held.split(',')
        .filter_map(|id| {
            let (volume, file) = id.split_once(':')?;
            Some((u64::from_str_radix(volume, 16).ok()?, u128::from_str_radix(file, 16).ok()?))
        })
        .collect()
}

/// What a build waiting for the lock of its directory's logs says.
pub(crate) fn waiting_message(holder: Option<&Holder>) -> String {
    match holder {
        Some(h) => format!("waiting for the build running in this directory ({}) to finish...", h.describe()),
        None => "waiting for the build running in this directory to finish...".into(),
    }
}

/// Whether the records of `path` stay in the logs when they are
/// recompacted: as ninja's `IsPathDead` has it, those of the manifest's
/// outputs, and of files that exist. Another manifest sharing the build
/// directory builds them (CMake's Ninja Multi-Config), or a generator
/// reads them.
pub(crate) fn is_live(state: &State, path: &str) -> bool {
    state.lookup_node(path).is_some_and(|n| state.node(n).in_edge.is_some()) || std::path::Path::new(path).exists()
}

/// Writes back the records of outputs of remote actions a tool changed.
fn save_remote_outputs(outputs: Option<&RemoteOutputs>, status: &mut dyn Status) {
    if let Some(outputs) = outputs
        && let Err(e) = outputs.save()
    {
        status.warning(&format!("saving {REMOTE_OUTPUTS_FILE}: {e}"));
    }
}

/// The directories a build writes to besides the build directory: its
/// spelling with symlinks resolved, and those of outputs elsewhere (a
/// generator writing into the source tree), each of them (the daemon tells
/// a directory of generated files from a checkout's root a file is copied
/// to). Files there may change during the build, so the scanners and the
/// daemon check them every time.
fn volatile_dirs(state: &State, build_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(real) = std::fs::canonicalize(build_dir).map(ninja_daemon::watch::strip_verbatim)
        && real != build_dir
    {
        dirs.push(real);
    }
    let mut seen = std::collections::HashSet::new();
    for (i, e) in state.edges.iter().enumerate() {
        if state.is_phony(ninja_core::graph::EdgeId(i as u32)) {
            continue;
        }
        for &o in &e.outputs {
            let path = ninja_remote::scanner::normalize(&build_dir.join(state.path(o)));
            if let Some(dir) = path.parent()
                && !dir.starts_with(build_dir)
                && seen.insert(dir.to_path_buf())
            {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    dirs
}

/// The directories build edges write to (their outputs'), in the build
/// directory or not, those in it also by its real spelling: the
/// conservative scanner runs a computed include there locally, and sends
/// their header-like files only for one elsewhere (see
/// `ninja_remote::scanner::WrittenDirs`).
fn written_dirs(state: &State, build_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let real = std::fs::canonicalize(build_dir).map(ninja_daemon::watch::strip_verbatim).ok().filter(|r| r != build_dir);
    let mut dirs: Vec<std::path::PathBuf> = real.iter().cloned().collect();
    let mut seen = std::collections::HashSet::new();
    for (i, e) in state.edges.iter().enumerate() {
        if state.is_phony(ninja_core::graph::EdgeId(i as u32)) {
            continue;
        }
        for &o in &e.outputs {
            let path = ninja_remote::scanner::normalize(&build_dir.join(state.path(o)));
            let Some(dir) = path.parent() else { continue };
            if !seen.insert(dir.to_path_buf()) {
                continue;
            }
            if let (Some(real), Ok(rel)) = (&real, dir.strip_prefix(build_dir))
                && !rel.as_os_str().is_empty()
            {
                dirs.push(real.join(rel));
            }
            dirs.push(dir.to_path_buf());
        }
    }
    dirs
}

/// `dirs` without those under another (the scanners in the build take a
/// directory for all under it).
fn outermost(dirs: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    dirs.iter().filter(|d| !dirs.iter().any(|o| o != *d && d.starts_with(o))).cloned().collect()
}

fn in_build_dir(build_dir: &str, file: &str) -> String {
    if build_dir.is_empty() { file.to_string() } else { format!("{build_dir}/{file}") }
}

/// The files the command line names, through phony targets: they are always
/// downloaded. None for the default targets.
fn requested_paths(state: &State, deps_log: &DepsLog, targets: &[String]) -> Vec<String> {
    if targets.is_empty() {
        return Vec::new();
    }
    let Ok(mut stack) = tools::collect_targets(state, Some(deps_log), targets) else { return Vec::new() };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    while let Some(n) = stack.pop() {
        if !seen.insert(n) {
            continue;
        }
        match state.node(n).in_edge {
            Some(e) if state.is_phony(e) => stack.extend(state.edge(e).inputs.iter().copied()),
            _ => out.push(state.path(n).to_string()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_a_build_writes_to() {
        let mut state = State::new();
        let reader = RealFileReader;
        // Paths only: nothing is written there.
        let build = test_dirs::base().join("ninja-rs-volatile").join("build");
        let generated = build.parent().unwrap().join("src").join("gen");
        let manifest = format!(
            "rule r\n  command = x\nbuild obj/a.o: r\nbuild {generated}/a.h {generated}/sub/b.h: r\nbuild all: phony ../elsewhere/x\n",
            generated = generated.to_string_lossy().replace('\\', "/").replace(':', "$:")
        );
        ManifestParser::new(&mut state, &reader, ParserOptions::default()).parse_test(&manifest).unwrap();
        // Outputs outside the build directory, not the build directory's
        // own, nor phony ones; each directory, those under another too (but
        // for the scanners in the build).
        let dirs = volatile_dirs(&state, &build);
        assert_eq!(dirs, vec![generated.clone(), generated.join("sub")]);
        assert_eq!(outermost(&dirs), vec![generated.clone()]);
        // For the conservative scanner: each directory edges write to, in
        // the build directory too.
        assert_eq!(written_dirs(&state, &build), vec![build.join("obj"), generated.clone(), generated.join("sub")]);
    }

    /// The build directory's real spelling, when it differs, is as paths
    /// are compared (on Windows, without `\\?\`, which nothing matched).
    #[test]
    fn real_build_dir() {
        let dir = test_dirs::TestDir::new("ninja-rs-volatile-real");
        let dirs = volatile_dirs(&State::new(), &dir);
        assert!(dirs.iter().all(|d| !d.to_string_lossy().starts_with(r"\\?\")), "{dirs:?}");
    }
}
