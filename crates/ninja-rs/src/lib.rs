//! The ninja-rs executor service.
//!
//! [`Service`] holds everything that should outlive a single build (the
//! remote connection and the caches in [`ninja_remote::Store`]) and runs
//! builds in-process. The CLI creates one and runs one build; a future daemon
//! would keep it alive across builds. There is no global state.

pub mod bes;
pub mod exec_runner;
pub mod options;
pub mod tools;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ninja_core::build::{BuildConfig, Builder};
use ninja_core::build_log::{BUILD_LOG_FILE, BuildLog};
use ninja_core::canon::canonicalize_path;
use ninja_core::deps_log::{DEPS_LOG_FILE, DepsLog};
use ninja_core::disk::{Disk, RealDisk, StatCounters};
use ninja_core::graph::{ROOT_SCOPE, State};
use ninja_core::parser::{ManifestParser, ParserOptions, PhonyCycleAction, RealFileReader};
use ninja_core::runner::{CommandRunner, DryRunRunner, RemoteBackend};
use ninja_core::status::{Status, StatusPrinter, Verbosity};
use ninja_core::subprocess::Interrupt;

use crate::bes::{Bes, InvocationIds, ReportingStatus};
use crate::exec_runner::ExecRunner;
use crate::options::Options;
use crate::tools::{ToolCtx, When};

/// Edge bindings of the RBE interface (docs/rbe-interface.md) that runners
/// need.
pub const RBE_BINDINGS: &[&str] = &["rbe_local", "rbe_mnemonic", "rbe_target", "rbe_config", "rbe_artifact", "rbe_deliverable"];

/// The version of the RBE interface this executor implements.
pub const RBE_VERSION: &str = "1";

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_INTERRUPTED: i32 = 130;

pub struct Service {
    pub store: Arc<ninja_remote::Store>,
    interrupt: Arc<Interrupt>,
    /// Remote connection, kept across builds (keyed by endpoint/instance).
    remote_client: Option<(String, ninja_remote::Client)>,
    digests_loaded: bool,
    remote_verbose: bool,
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
}

impl Service {
    pub fn new(interrupt: Arc<Interrupt>) -> Self {
        Self {
            store: ninja_remote::Store::new(),
            interrupt,
            remote_client: None,
            digests_loaded: false,
            remote_verbose: false,
            metrics: Vec::new(),
            stat_counters: None,
            stat_cache: true,
            ids: InvocationIds::default(),
            skip_teardown: false,
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
        println!("{:<width$}\t{:<6}\t{:<9}\ttotal (ms)", "metric", "count", "avg (us)");
        for (name, count, total) in rows {
            let total_us = total.as_secs_f64() * 1e6;
            let avg = if count > 0 { total_us / count as f64 } else { 0.0 };
            println!("{name:<width$}\t{count:<6}\t{avg:<9.1}\t{:.1}", total_us / 1000.0);
        }
    }

    fn real_disk(&self) -> RealDisk {
        RealDisk::with_counters(self.stat_counters.clone())
    }

    async fn run_invocation(&mut self, opts: &Options) -> i32 {
        self.remote_verbose = opts.remote.verbose;
        self.ids = InvocationIds::new(&opts.bes);
        // Build events cover builds, not tools.
        let bes = if opts.bes.enabled() && opts.tool.is_none() {
            let bytestream = opts.remote.executor.as_ref().map(|e| bes::bytestream_prefix(e, &opts.remote.instance_name));
            Some(Bes::start(&opts.bes, &self.ids, &opts.argv, &opts.targets, bytestream, self.store.clone()).await)
        } else {
            None
        };
        let mut printer = StatusPrinter::new(opts.verbosity);
        let mut status = ReportingStatus { printer: &mut printer, bes: bes.clone() };
        let code = self.run_build_invocation(opts, &mut status).await;
        if let Some(b) = bes {
            b.finish(code).await;
        }
        code
    }

    async fn run_build_invocation(&mut self, opts: &Options, status: &mut ReportingStatus<'_>) -> i32 {
        if let Some(tool) = &opts.tool
            && tool == "list"
        {
            print!("{}", tools::list_tools());
            return 0;
        }
        if let Some(t) = &opts.tool
            && tools::find_tool(t).is_some_and(|i| i.when == When::AfterFlags)
        {
            return tools::tool_restat_after_flags(&opts.tool_args);
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
            let parsed =
                self.timed(".ninja parse", || ManifestParser::new(&mut state, &reader, parser_opts).load(&opts.input_file));
            if let Err(e) = parsed {
                status.error(&e);
                return EXIT_FAILURE;
            }

            let build_dir = state.lookup_variable(ROOT_SCOPE, "builddir");
            let log_path = if build_dir.is_empty() { BUILD_LOG_FILE.to_string() } else { format!("{build_dir}/{BUILD_LOG_FILE}") };
            let deps_path = if build_dir.is_empty() { DEPS_LOG_FILE.to_string() } else { format!("{build_dir}/{DEPS_LOG_FILE}") };
            let mut build_log = BuildLog::new();
            let mut deps_log = DepsLog::new();

            if let Some(t) = tool
                && t.when == When::AfterLoad
            {
                let mut ctx = ToolCtx {
                    state: &mut state,
                    build_log: &mut build_log,
                    deps_log: &mut deps_log,
                    build_log_path: log_path,
                    dry_run: opts.dry_run,
                    verbose: opts.verbosity == Verbosity::Verbose,
                    quiet: opts.verbosity == Verbosity::Quiet,
                };
                let code = tools::run_tool(t.name, &opts.tool_args, &mut ctx);
                return self.finish(code, state, build_log, deps_log);
            }

            if !build_dir.is_empty() && !opts.dry_run && std::fs::create_dir_all(&build_dir).is_err() {
                status.error(&format!("creating build directory {build_dir}"));
                return EXIT_FAILURE;
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
            if let Some(t) = tool
                && t.when == When::AfterLogs
            {
                let mut ctx = ToolCtx {
                    state: &mut state,
                    build_log: &mut build_log,
                    deps_log: &mut deps_log,
                    build_log_path: log_path,
                    dry_run: opts.dry_run,
                    verbose: opts.verbosity == Verbosity::Verbose,
                    quiet: opts.verbosity == Verbosity::Quiet,
                };
                let code = tools::run_tool(t.name, &opts.tool_args, &mut ctx);
                return self.finish(code, state, build_log, deps_log);
            }
            if !opts.dry_run {
                let st = &state;
                let live = |p: &str| st.lookup_node(p).is_some();
                if let Err(e) = build_log.open_for_write(&log_path, &live) {
                    status.error(&format!("opening build log: {e}"));
                    return EXIT_FAILURE;
                }
                if let Err(e) = deps_log.open_for_write(&deps_path, &live) {
                    status.error(&format!("opening deps log: {e}"));
                    return EXIT_FAILURE;
                }
            }

            let targets = Arc::new(self.load_targets(&state, status));
            let remote = match self.remote_backend(&state, opts, targets.clone(), status).await {
                Ok(r) => r,
                Err(e) => {
                    status.error(&e);
                    return EXIT_FAILURE;
                }
            };
            if remote.is_some() && !self.digests_loaded {
                // File digests persist across invocations (keyed by path,
                // size and mtime) so that toolchains are hashed once.
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
                cwd: ".".into(),
            };

            // Attempt to rebuild the manifest before building anything else.
            match self
                .rebuild_manifest(&opts.input_file, &mut state, &mut build_log, &mut deps_log, &config, status, remote.clone())
                .await
            {
                Ok(true) => {
                    // In dry_run mode the regeneration will succeed without
                    // changing the manifest forever. Better to return immediately.
                    if opts.dry_run {
                        return EXIT_SUCCESS;
                    }
                    // Start the build over with the new manifest.
                    continue;
                }
                Ok(false) => {}
                Err((e, code)) => {
                    status.error(&format!("rebuilding '{}': {e}", opts.input_file));
                    return code;
                }
            }

            let digests_file = if build_dir.is_empty() {
                ninja_remote::store::DIGESTS_FILE.to_string()
            } else {
                format!("{build_dir}/{}", ninja_remote::store::DIGESTS_FILE)
            };
            let use_remote = remote.is_some();
            let code = self
                .run_build(&opts.targets, &mut state, &mut build_log, &mut deps_log, &config, status, remote, targets)
                .await;
            if use_remote && let Err(e) = self.store.save_digests(std::path::Path::new(&digests_file)) {
                status.warning(&format!("saving {digests_file}: {e}"));
            }
            let (remote_n, hits) = status.printer.remote_counts();
            if remote_n > 0 && opts.verbosity != Verbosity::Quiet {
                status.info(&format!("{remote_n} remote actions ({hits} remote cache hits)."));
            }
            return self.finish(code, state, build_log, deps_log);
        }
        status.error(&format!("manifest '{}' still dirty after {CYCLE_LIMIT} tries", opts.input_file));
        EXIT_FAILURE
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
        status: &mut dyn Status,
    ) -> Result<Option<Arc<dyn RemoteBackend>>, String> {
        let Some(endpoint) = &opts.remote.executor else { return Ok(None) };
        if opts.dry_run {
            return Ok(None);
        }
        match state.lookup_variable(ROOT_SCOPE, "rbe_version").as_str() {
            RBE_VERSION => {}
            "" => {
                if opts.remote.verbose {
                    status.info("remote execution disabled: the manifest does not set rbe_version");
                }
                return Ok(None);
            }
            other => {
                status.warning(&format!(
                    "remote execution disabled: the manifest uses RBE interface version {other}, this executor supports {RBE_VERSION}"
                ));
                return Ok(None);
            }
        }
        let key = format!("{endpoint}|{}|{:?}", opts.remote.instance_name, opts.remote.headers);
        if self.remote_client.as_ref().is_none_or(|(k, _)| *k != key) {
            let client = ninja_remote::connect(&ninja_remote::ConnectOptions {
                endpoint: endpoint.clone(),
                instance_name: opts.remote.instance_name.clone(),
                headers: opts.remote.headers.clone(),
            })
            .await
            .map_err(|e| format!("connecting to {endpoint}: {e:#}"))?;
            self.remote_client = Some((key, client));
        }
        // Requests carry the invocation's ids, which build events report too.
        let client = self.remote_client.as_ref().expect("connected").1.clone().with_request_metadata(rbe_client::proto::RequestMetadata {
            tool_details: Some(rbe_client::proto::ToolDetails {
                tool_name: "ninja-rs".into(),
                tool_version: env!("CARGO_PKG_VERSION").into(),
            }),
            tool_invocation_id: self.ids.invocation_id.clone(),
            correlated_invocations_id: self.ids.build_request_id.clone(),
            ..Default::default()
        });
        let build_dir = std::env::current_dir().map_err(|e| e.to_string())?;
        let file_var = |name: &str| Some(state.lookup_variable(ROOT_SCOPE, name)).filter(|v| !v.is_empty());
        let backend = ninja_remote::Backend::new(
            ninja_remote::BackendConfig {
                build_dir,
                toolchains_file: file_var("rbe_toolchains"),
                targets,
                exec_root: opts.remote.exec_root.clone().map(Into::into),
                jobs: opts.remote.jobs,
                mnemonic_suffixes: opts.remote.mnemonic_suffixes.clone(),
                local_fallback: opts.remote.local_fallback,
                verbose: opts.remote.verbose,
            },
            client,
            self.store.clone(),
        )
        .await
        .map_err(|e| format!("remote execution: {e:#}"))?;
        Ok(Some(Arc::new(backend)))
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

    fn make_runner(&self, config: &BuildConfig, remote: Option<Arc<dyn RemoteBackend>>, verbose_remote: bool) -> Box<dyn CommandRunner> {
        if config.dry_run {
            Box::new(DryRunRunner::default())
        } else {
            let mut r = ExecRunner::new(config.parallelism, remote, self.interrupt.clone());
            r.verbose_remote = verbose_remote;
            Box::new(r)
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
    ) -> Result<bool, (String, i32)> {
        if input_file.is_empty() {
            return Err(("empty path".into(), EXIT_FAILURE));
        }
        let (path, _) = canonicalize_path(input_file);
        let Some(node) = state.lookup_node(&path) else { return Ok(false) };
        let disk = self.real_disk();
        let mut runner = self.make_runner(config, remote, false);
        {
            let mut builder = Builder::new(state, &disk, Some(build_log), Some(deps_log), config, status);
            builder.add_target(node).map_err(|e| (e, EXIT_FAILURE))?;
            if builder.already_up_to_date() {
                return Ok(false); // Not an error, but we didn't rebuild.
            }
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
    ) -> i32 {
        let bes = status.bes.clone();
        let nodes = match tools::collect_targets(state, Some(deps_log), targets) {
            Ok(n) => n,
            Err(e) => {
                status.error(&e);
                return EXIT_FAILURE;
            }
        };
        let disk = self.real_disk();
        let verbose_remote = remote.is_some() && (self.remote_verbose || config.verbosity == Verbosity::Verbose);
        let mut runner = self.make_runner(config, remote, verbose_remote);
        let mut builder = Builder::new(state, &disk, Some(build_log), Some(deps_log), config, status);
        // Like ninja: cache stats while scanning, but not while building, so
        // restat and later edges see the outputs' new timestamps.
        disk.allow_stat_cache(self.stat_cache);
        for n in nodes {
            if let Err(e) = builder.add_target(n) {
                disk.allow_stat_cache(false);
                builder.status.error(&e);
                return EXIT_FAILURE;
            }
        }
        disk.allow_stat_cache(false);
        if let Some(b) = &bes {
            b.plan(builder.state, &builder.plan, targets_file, targets);
        }
        if builder.already_up_to_date() {
            if config.verbosity != Verbosity::NoStatusUpdate {
                builder.status.info("no work to do.");
            }
            return EXIT_SUCCESS;
        }
        match builder.build(runner.as_mut()).await {
            Ok(()) => EXIT_SUCCESS,
            Err(e) => {
                builder.status.info(&format!("build stopped: {}.", e.message));
                if e.interrupted { EXIT_INTERRUPTED } else { EXIT_FAILURE }
            }
        }
    }
}
