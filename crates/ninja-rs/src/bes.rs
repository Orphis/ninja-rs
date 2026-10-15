//! Build events for ninja builds, in Bazel's Build Event Protocol, streamed
//! to a Build Event Service (`--bes_backend`) and/or written to a file
//! (`--build_event_binary_file`), so a build shows up in a BES UI like a
//! Bazel invocation.
//!
//! How a build maps onto BEP (docs/design.md, "BES integration"):
//! - **Targets** are the `rbe_target`s of the edges the requested targets
//!   need, labelled and typed by the `rbe_targets` file. `Pattern` announces
//!   one `TargetConfigured` per target, each announcing one
//!   `TargetCompleted` per configuration.
//! - **Configurations** come from the `rbe_targets` file (`rbe_config` on
//!   edges picks one), or a single default one.
//! - A target **completes** when the last of its edges this build runs
//!   finishes or leaves the plan (a `restat`), at once if it has none to run
//!   (it is up to date: every target the requested ones need is reported,
//!   as Bazel does), or fails with its first failed edge. Its outputs are
//!   those of its `rbe_artifact` (or `rbe_deliverable`) edges: CAS
//!   references (`bytestream://`) for outputs built remotely, `file://`
//!   otherwise, hashed off the build loop, a few targets at once (and kept
//!   in the file digest cache, which build events persist too); after an
//!   interruption, targets still waiting for theirs are reported without
//!   them. Targets that never complete (after a failure, or an
//!   interruption) are `Aborted`.
//! - **Actions:** failed edges are reported as `ActionExecuted` (all of them
//!   with `--build_event_publish_all_actions`); every edge counts in
//!   `BuildMetrics`.
//! - **Console:** status lines, command output and messages, as `Progress`.
//!
//! Build events never fail the build: problems are warnings.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use bep::build_event::Payload;
use ninja_core::build::{Plan, Want};
use ninja_core::graph::{EdgeId, NodeId, State};
use ninja_core::runner::ExecKind;
use ninja_core::status::{EdgeInfo, Status, StatusPrinter};
use ninja_remote::Store;
use ninja_remote::remote_outputs::RemoteOutputs;
use ninja_remote::targets::{Configuration, Targets};
use prost::Message;
use rbe_bes::events::{EventStream, id};
use rbe_bes::proto::build_event_stream as bep;
use rbe_bes::proto::command_line as cl;
use rbe_bes::uploader::{BesConfig, BesUploader, BuildOutcome};

use crate::options::BesFlags;

const TOOL_NAME: &str = "ninja-rs";

/// The ids shared by build events and remote execution requests.
#[derive(Clone, Debug, Default)]
pub struct InvocationIds {
    pub invocation_id: String,
    /// Groups related invocations (Bazel's build request id).
    pub build_request_id: String,
}

impl InvocationIds {
    pub fn new(flags: &BesFlags) -> Self {
        Self { invocation_id: flags.invocation_id.clone().unwrap_or_else(new_uuid), build_request_id: flags.build_request_id.clone().unwrap_or_else(new_uuid) }
    }
}

/// A random (version 4) UUID, from the OS-seeded hasher keys std uses.
pub fn new_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let mut bytes = [0u8; 16];
    for (i, half) in bytes.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(now);
        h.write_u32(std::process::id());
        h.write_usize(i);
        half.copy_from_slice(&h.finish().to_be_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// `bytestream://host:port[/instance]` of a REAPI endpoint, for CAS references.
pub fn bytestream_prefix(endpoint: &str, instance_name: &str) -> String {
    let host = endpoint.split_once("://").map_or(endpoint, |(_, rest)| rest).trim_end_matches('/');
    if instance_name.is_empty() { format!("bytestream://{host}") } else { format!("bytestream://{host}/{instance_name}") }
}

fn timestamp(t: SystemTime) -> prost_types::Timestamp {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    prost_types::Timestamp { seconds: d.as_secs() as i64, nanos: d.subsec_nanos() as i32 }
}

fn millis(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|h| h.trim().to_string()))
        .unwrap_or_default()
}

fn user_name() -> String {
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default()
}

/// What a build builds, as `BuildStarted` announces it: the targets named
/// on the command line, or else the manifest's defaults (`all` for CMake),
/// a target of the targets file by its label.
pub fn patterns(state: &State, typed: &[String], targets_file: &Targets) -> Vec<String> {
    let names: Vec<String> = if typed.is_empty() {
        let defaults = state.default_nodes().unwrap_or_default();
        defaults.iter().map(|&n| state.path(n).to_string()).collect()
    } else {
        typed.to_vec()
    };
    names.into_iter().map(|n| if targets_file.get(&n).is_some() { targets_file.label(&n) } else { n }).collect()
}

/// Runners reported in `BuildMetrics`: name, exec kind.
const RUNNERS: [(&str, &str); 4] = [("remote", "Remote"), ("remote cache hit", "Remote"), ("disk cache hit", "Local"), ("local", "Local")];

/// A nested invocation sending events (see `Bes::relayed`).
struct RelaySource {
    /// Its build directory, relative to this one's when inside it.
    rel: String,
    /// `rel` as path components, for its files.
    prefix: Vec<String>,
    /// Tells its configurations apart: its build directory's.
    tag: String,
    /// Tells its file sets apart, which each invocation numbers from 0:
    /// this connection's (LLVM runs a nested build per tool in `NATIVE`).
    sets_tag: String,
}

impl RelaySource {
    /// The `connection`th nested invocation, building in `nested`.
    fn new(build_dir: &Path, nested: &str, connection: usize) -> RelaySource {
        let nested = Path::new(nested);
        let rel = nested.strip_prefix(build_dir).unwrap_or(nested);
        let prefix: Vec<String> =
            rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).filter(|c| !c.is_empty() && c != "/" && c != "\\").collect();
        let rel = prefix.join("/");
        RelaySource { tag: rel.clone(), sets_tag: format!("{rel}#{connection}"), rel, prefix }
    }

    fn config(&self, id: &str) -> String {
        format!("{id}-{}", self.tag)
    }

    fn path(&self, p: &str) -> String {
        if self.rel.is_empty() { p.to_string() } else { format!("{}/{p}", self.rel) }
    }
}

/// The configuration used when the targets file has none.
fn default_configuration() -> Configuration {
    Configuration { name: "default".into(), mnemonic: "ninja".into(), ..Default::default() }
}

/// (target name, `rbe_config`): what a `TargetCompleted` event reports.
type TargetKey = (String, Option<String>);

/// Output path → (hash, size), for outputs built remotely.
type CasOutputs = HashMap<String, (String, i64)>;

/// What `report_targets` does next.
enum Hashing {
    /// Reports a completed target, with its artifacts (and the CAS
    /// references of those built remotely).
    Target { key: TargetKey, paths: Vec<String>, cas: CasOutputs },
    /// Says when every target before it was reported.
    Settled(tokio::sync::oneshot::Sender<()>),
}

struct TargetState {
    label: String,
    config_id: String,
    /// Edges this build still has to run.
    remaining: usize,
    /// Outputs of its `rbe_artifact` (and `rbe_deliverable`) edges.
    artifacts: Vec<String>,
    done: bool,
}

#[derive(Default)]
struct MnemonicStats {
    created: i64,
    executed: i64,
    first_ms: i64,
    last_ms: i64,
}

#[derive(Default)]
struct Inner {
    stream: EventStream,
    targets: BTreeMap<TargetKey, TargetState>,
    /// The target of every edge the requested targets need.
    edge_targets: HashMap<EdgeId, TargetKey>,
    /// The edges their target waits for: those it counted (see
    /// `TargetState::remaining`). Only their end counts down.
    counted: std::collections::HashSet<EdgeId>,
    /// The targets were planned (`plan`): edges added to or removed from
    /// the plan since change what they wait for.
    planned: bool,
    targets_file: Arc<Targets>,
    starts: HashMap<EdgeId, SystemTime>,
    cas: CasOutputs,
    mnemonics: BTreeMap<String, MnemonicStats>,
    /// Runner name → (count, exec kind).
    runners: BTreeMap<&'static str, (i32, &'static str)>,
    named_sets: usize,
    targets_configured: i64,
    /// The patterns `BuildStarted` announced, once it was sent.
    patterns: Option<Vec<String>>,
    /// Console messages from before `BuildStarted`, sent after it.
    early_console: String,
}

impl Inner {
    /// One edge fewer for the target `key` to wait for: the target, when
    /// that was its last.
    fn count_down(&mut self, key: &TargetKey) -> Option<TargetKey> {
        let t = self.targets.get_mut(key).filter(|t| !t.done)?;
        t.remaining = t.remaining.saturating_sub(1);
        (t.remaining == 0).then(|| key.clone())
    }
}

pub struct Bes {
    flags: BesFlags,
    /// For explaining a failed connection to the backend.
    tls: crate::options::TlsFlags,
    ids: InvocationIds,
    /// `bytestream://...` of the remote cache, when there is one.
    bytestream: Option<String>,
    store: Arc<Store>,
    uploader: Mutex<Option<BesUploader>>,
    /// Completed targets waiting for their artifacts' digests (see
    /// `report_targets`).
    hashing: tokio::sync::mpsc::UnboundedSender<Hashing>,
    /// Set once the build was interrupted: targets are reported without
    /// waiting for their artifacts' digests.
    hurry: tokio::sync::watch::Sender<bool>,
    file: Mutex<Option<std::io::BufWriter<std::fs::File>>>,
    /// A nested invocation: its events go to the outermost one instead.
    relay_out: Mutex<Option<ninja_daemon::ipc::Stream>>,
    /// The outermost invocation: where nested ones send their events.
    relay_in: Option<crate::nested::RelayAddress>,
    /// This invocation's build directory.
    build_dir: std::path::PathBuf,
    inner: Mutex<Inner>,
    started: Instant,
    start_time: SystemTime,
    argv: Vec<String>,
    given: Vec<crate::options::GivenOption>,
    /// The targets named on the command line (`-t exec`: the command).
    typed: Vec<String>,
    /// Bazel's command: `build`, or `exec` for `-t exec`.
    command: &'static str,
}

impl Bes {
    /// Opens the file (when given) and starts the upload to the backend,
    /// which connects in the background: the build doesn't wait for it. The
    /// events describing the invocation go first, once the build's targets
    /// are known (`announce`), or with the first other event.
    pub async fn start(
        opts: &crate::options::Options,
        ids: &InvocationIds,
        bytestream: Option<String>,
        store: Arc<Store>,
        relay: Option<crate::nested::Relay>,
    ) -> Arc<Bes> {
        let (flags, tls) = (&opts.bes, &opts.tls);
        let exec = opts.tool.as_deref() == Some("exec");
        let command = if exec { "exec" } else { "build" };
        let build_dir = std::env::current_dir().unwrap_or_default();
        let relay_out = flags.relay.as_ref().and_then(|r| match crate::nested::connect(r, &build_dir) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("ninja: warning: this build's events won't reach the invocation running it: {e}");
                None
            }
        });
        let mut uploader = None;
        if let Some(backend) = &flags.backend {
            match tls.load() {
                Ok(t) => {
                    let connect = t.connect_options(backend, "", &[]);
                    let mut config = BesConfig::new(&flags.instance_name, &ids.build_request_id, &ids.invocation_id);
                    config.keywords = flags.keywords.clone();
                    config.keywords.extend([format!("command_name={command}"), "protocol_name=BEP".to_string()]);
                    config.lifecycle_events = flags.lifecycle_events;
                    config.headers = flags.headers.clone();
                    config.rpc_timeout = flags.rpc_timeout;
                    config.trace = std::env::var_os("NINJA_RS_BES_DEBUG").is_some_and(|v| !v.is_empty());
                    // A failed connection is reported at the end (`finish`).
                    uploader = Some(BesUploader::connecting(async move { rbe_client::connect_channel(&connect).await }, config));
                }
                Err(e) => eprintln!("ninja: warning: build events won't be uploaded: {e}"),
            }
        }
        let file = flags.binary_file.as_ref().and_then(|p| match std::fs::File::create(p) {
            Ok(f) => Some(std::io::BufWriter::new(f)),
            Err(e) => {
                eprintln!("ninja: warning: can't write build events to {p}: {e}");
                None
            }
        });
        if let (Some(url), true) = (&flags.results_url, uploader.is_some()) {
            eprintln!("ninja: streaming build results to: {url}{}", ids.invocation_id);
        }
        let relay_in = relay.as_ref().map(|r| r.address.clone());
        let (hashing, jobs) = tokio::sync::mpsc::unbounded_channel();
        let (hurry, hurried) = tokio::sync::watch::channel(false);
        let bes = Arc::new(Bes {
            flags: flags.clone(),
            tls: tls.clone(),
            ids: ids.clone(),
            bytestream,
            store,
            uploader: Mutex::new(uploader),
            hashing,
            hurry,
            file: Mutex::new(file),
            relay_out: Mutex::new(relay_out),
            relay_in,
            build_dir,
            inner: Mutex::default(),
            started: Instant::now(),
            start_time: SystemTime::now(),
            // Header values (tokens) aren't published: their keys are.
            argv: crate::options::redact_args(&opts.argv),
            given: opts.given.iter().map(crate::options::GivenOption::redacted).collect(),
            typed: if exec { opts.tool_args.iter().skip_while(|a| *a == "--").cloned().collect() } else { opts.targets.clone() },
            command,
        });
        tokio::spawn(report_targets(Arc::downgrade(&bes), jobs, hurried));
        if let Some(r) = relay {
            bes.serve_relay(r.listener);
        }
        bes
    }

    /// A nested invocation's events stop going to the outer one (before any
    /// was sent: they start with the build's targets).
    pub fn leave_relay(&self) {
        self.relay_out.lock().unwrap().take();
    }

    /// Accepts nested invocations' connections: each sends its build
    /// directory, then its events.
    fn serve_relay(self: &Arc<Self>, mut listener: ninja_daemon::ipc::Listener) {
        let me = Arc::downgrade(self);
        std::thread::spawn(move || {
            let mut connections = 0;
            while let Ok(mut stream) = listener.accept() {
                // The stream ended: later events have nowhere to go.
                if me.upgrade().is_none_or(|b| b.inner.lock().unwrap().stream.is_finished()) {
                    return;
                }
                let me = me.clone();
                connections += 1;
                let connection = connections;
                std::thread::spawn(move || {
                    let Ok(Some(dir)) = crate::nested::read_frame(&mut stream) else { return };
                    let source = RelaySource::new(&me.upgrade().map(|b| b.build_dir.clone()).unwrap_or_default(), &String::from_utf8_lossy(&dir), connection);
                    while let Ok(Some(frame)) = crate::nested::read_frame(&mut stream) {
                        // The end: everything before it is merged.
                        if frame.is_empty() {
                            let _ = std::io::Write::write_all(&mut stream, &[1]);
                            return;
                        }
                        let (Some(bes), Ok(event)) = (me.upgrade(), bep::BuildEvent::decode(frame.as_slice())) else { return };
                        bes.relayed(&source, event);
                    }
                });
            }
        });
    }

    /// Merges a nested invocation's event into this stream: its targets,
    /// failed actions, console output and action counts. Its configurations
    /// (and so its targets' completions) are told apart by its build
    /// directory: LLVM builds `llvm-tblgen` in both.
    fn relayed(&self, source: &RelaySource, event: bep::BuildEvent) {
        use bep::build_event_id::Id;
        let (Some(id), Some(payload)) = (event.id.and_then(|i| i.id), event.payload) else { return };
        let config = |c: &str| source.config(c);
        let set = |s: &str| format!("{}:{s}", source.sets_tag);
        let sets = |sets: Vec<bep::build_event_id::NamedSetOfFilesId>| -> Vec<bep::build_event_id::NamedSetOfFilesId> {
            sets.into_iter().map(|s| bep::build_event_id::NamedSetOfFilesId { id: set(&s.id) }).collect()
        };
        let rewrite_children = |children: Vec<bep::BuildEventId>| -> Vec<bep::BuildEventId> {
            children
                .into_iter()
                .filter_map(|c| match c.id? {
                    Id::TargetCompleted(t) => Some(id::target_completed(&t.label, &config(&t.configuration.unwrap_or_default().id))),
                    _ => None,
                })
                .collect()
        };
        match (id, payload) {
            (Id::Progress(_), Payload::Progress(p)) => {
                if !p.stdout.is_empty() || !p.stderr.is_empty() {
                    self.emit(|i| i.stream.progress(&p.stdout, &p.stderr));
                }
            }
            (Id::Configuration(c), Payload::Configuration(mut cfg)) => {
                cfg.mnemonic = format!("{} ({})", cfg.mnemonic, source.rel);
                self.emit(|i| i.stream.post(id::configuration(&config(&c.id)), vec![], Payload::Configuration(cfg)));
            }
            (Id::TargetConfigured(t), payload) => {
                // A label this invocation configured too keeps its event:
                // the nested completions are announced as they come.
                let children = rewrite_children(event.children);
                self.emit(|i| i.stream.post(id::target_configured(&t.label), children, payload));
            }
            (Id::TargetCompleted(t), mut payload) => {
                if let Payload::Completed(c) = &mut payload {
                    for g in &mut c.output_group {
                        g.file_sets = sets(std::mem::take(&mut g.file_sets));
                    }
                }
                let config_id = config(&t.configuration.unwrap_or_default().id);
                self.emit(|i| i.stream.post(id::target_completed(&t.label, &config_id), vec![], payload));
            }
            (Id::NamedSet(s), Payload::NamedSetOfFiles(mut files)) => {
                for f in &mut files.files {
                    let mut prefix = source.prefix.clone();
                    prefix.append(&mut f.path_prefix);
                    f.path_prefix = prefix;
                }
                files.file_sets = sets(std::mem::take(&mut files.file_sets));
                self.emit(|i| i.stream.post(id::named_set(&set(&s.id)), vec![], Payload::NamedSetOfFiles(files)));
            }
            (Id::ActionCompleted(a), Payload::Action(mut action)) => {
                let config_id = config(&a.configuration.unwrap_or_default().id);
                let primary = source.path(&a.primary_output);
                #[allow(deprecated)]
                {
                    action.configuration = Some(bep::build_event_id::ConfigurationId { id: config_id.clone() });
                }
                if let Some(f) = &mut action.primary_output {
                    f.name = source.path(&f.name);
                }
                self.emit(|i| i.stream.post(id::action_completed(&primary, &a.label, &config_id), vec![], Payload::Action(action)));
            }
            (_, Payload::BuildMetrics(m)) => {
                let mut inner = self.inner.lock().unwrap();
                if let Some(s) = m.action_summary {
                    for a in s.action_data {
                        let e = inner.mnemonics.entry(a.mnemonic).or_default();
                        e.created += a.actions_created;
                        e.executed += a.actions_executed;
                        e.first_ms = if e.first_ms == 0 { a.first_started_ms } else { e.first_ms.min(a.first_started_ms) };
                        e.last_ms = e.last_ms.max(a.last_ended_ms);
                    }
                    for r in s.runner_count.into_iter().filter(|r| r.name != "total") {
                        let name = RUNNERS.iter().find(|(n, _)| *n == r.name).map(|(n, k)| (*n, *k));
                        if let Some((n, k)) = name {
                            inner.runners.entry(n).or_insert((0, k)).0 += r.count;
                        }
                    }
                }
                inner.targets_configured += m.target_metrics.map_or(0, |t| t.targets_configured);
            }
            _ => {}
        }
    }

    /// Sends the events describing the invocation, if they weren't sent yet:
    /// `BuildStarted` announces `patterns`, the targets this build builds
    /// (BES services show them as the invocation's title).
    pub fn announce(&self, patterns: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        if inner.patterns.is_none() && !inner.stream.is_finished() {
            let events = self.start_events(&mut inner, patterns.to_vec());
            self.send(events);
        }
    }

    /// The invocation's events, then the console messages kept until them.
    fn start_events(&self, inner: &mut Inner, patterns: Vec<String>) -> Vec<bep::BuildEvent> {
        let mut events = self.invocation_events(inner, patterns);
        let early = std::mem::take(&mut inner.early_console);
        if !early.is_empty() {
            events.extend(inner.stream.progress("", &early));
        }
        events
    }

    /// Runs `f` on the stream and sends what it produced, in order, after
    /// the invocation's events (announcing the targets named on the command
    /// line, when nothing announced others before).
    fn emit(&self, f: impl FnOnce(&mut Inner) -> Vec<bep::BuildEvent>) {
        let mut inner = self.inner.lock().unwrap();
        // After the end (a nested invocation's events relayed late, a
        // target reported once its artifacts were hashed): nowhere to go.
        if inner.stream.is_finished() {
            return;
        }
        let mut events = if inner.patterns.is_none() { self.start_events(&mut inner, self.typed.clone()) } else { Vec::new() };
        events.extend(f(&mut inner));
        self.send(events);
    }

    /// Writes events to the file and the uploader. Callers hold `inner`, so
    /// that events go out in the stream's order.
    fn send(&self, events: Vec<bep::BuildEvent>) {
        {
            let mut relay = self.relay_out.lock().unwrap();
            if let Some(stream) = relay.as_mut() {
                for e in &events {
                    if let Err(err) = crate::nested::write_frame(stream, &e.encode_to_vec()) {
                        eprintln!("ninja: warning: sending build events to the invocation running this one failed: {err}");
                        *relay = None;
                        break;
                    }
                }
                return;
            }
        }
        let uploader = self.uploader.lock().unwrap();
        let mut file = self.file.lock().unwrap();
        for e in events {
            if let Some(f) = file.as_mut()
                && let Err(err) = f.write_all(&e.encode_length_delimited_to_vec())
            {
                eprintln!("ninja: warning: writing build events failed: {err}");
                *file = None;
            }
            if let Some(u) = uploader.as_ref() {
                u.send(e);
            }
        }
    }

    /// Bazel's structured command line: its sections, which UIs show, with
    /// the options and the targets named on the command line as the
    /// residual. The `original` one has the options typed, the `canonical`
    /// one those of `.ninjars` files and the outer invocation too.
    fn structured_command_line(&self, label: &str) -> cl::CommandLine {
        let chunk = |label: &str, chunk: Vec<String>| cl::CommandLineSection {
            section_label: label.into(),
            section_type: Some(cl::command_line_section::SectionType::ChunkList(cl::ChunkList { chunk })),
        };
        let options = |option: Vec<cl::Option>| cl::command_line_section::SectionType::OptionList(cl::OptionList { option });
        let given = self
            .given
            .iter()
            .filter(|g| label != "original" || g.explicit)
            .map(|g| cl::Option { combined_form: g.combined_form.clone(), option_name: g.name.clone(), option_value: g.value.clone(), ..Default::default() })
            .collect();
        cl::CommandLine {
            command_line_label: label.into(),
            sections: vec![
                chunk("executable", vec![self.argv.first().cloned().unwrap_or_default()]),
                cl::CommandLineSection { section_label: "startup options".into(), section_type: Some(options(vec![])) },
                chunk("command", vec![self.command.into()]),
                cl::CommandLineSection { section_label: "command options".into(), section_type: Some(options(given)) },
                chunk("residual", self.typed.clone()),
            ],
        }
    }

    fn invocation_events(&self, inner: &mut Inner, patterns: Vec<String>) -> Vec<bep::BuildEvent> {
        let (argv, start) = (&self.argv, self.start_time);
        let workspace = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let started = bep::BuildStarted {
            uuid: self.ids.invocation_id.clone(),
            start_time: Some(timestamp(start)),
            build_tool_version: format!("{TOOL_NAME} {}", crate::version()),
            options_description: argv.iter().skip(1).cloned().collect::<Vec<_>>().join(" "),
            command: self.command.into(),
            working_directory: workspace.clone(),
            workspace_directory: workspace.clone(),
            server_pid: i64::from(std::process::id()),
            host: host_name(),
            user: user_name(),
            ..Default::default()
        };
        let mut children = vec![
            id::unstructured_command_line(),
            id::structured_command_line("original"),
            id::structured_command_line("canonical"),
            id::options_parsed(),
            id::workspace_status(),
            id::build_metadata(),
            id::workspace(),
        ];
        if !patterns.is_empty() {
            children.push(id::pattern(&patterns));
        }
        children.push(id::build_finished());
        let command_lines: Vec<_> = ["original", "canonical"].iter().map(|l| (*l, self.structured_command_line(l))).collect();
        let explicit: Vec<String> = argv.iter().skip(1).cloned().collect();
        let options: Vec<String> = self.given.iter().map(|g| g.combined_form.clone()).chain(self.typed.iter().cloned()).collect();
        let status = [("BUILD_HOST", host_name()), ("BUILD_USER", user_name()), ("BUILD_TIMESTAMP", (millis(start) / 1000).to_string())];
        let metadata = self.flags.build_metadata.iter().cloned().collect();
        inner.patterns = Some(patterns);
        {
            let s = &mut inner.stream;
            let mut out = s.started(started, children);
            out.extend(s.post(id::unstructured_command_line(), vec![], Payload::UnstructuredCommandLine(bep::UnstructuredCommandLine { args: argv.to_vec() })));
            for (label, cmd) in command_lines {
                out.extend(s.post(id::structured_command_line(label), vec![], Payload::StructuredCommandLine(cmd)));
            }
            out.extend(s.post(
                id::options_parsed(),
                vec![],
                Payload::OptionsParsed(bep::OptionsParsed { cmd_line: options, explicit_cmd_line: explicit, tool_tag: TOOL_NAME.into(), ..Default::default() }),
            ));
            out.extend(s.post(
                id::workspace_status(),
                vec![],
                Payload::WorkspaceStatus(bep::WorkspaceStatus {
                    item: status.iter().map(|(k, v)| bep::workspace_status::Item { key: (*k).into(), value: v.clone() }).collect(),
                }),
            ));
            out.extend(s.post(id::build_metadata(), vec![], Payload::BuildMetadata(bep::BuildMetadata { metadata })));
            out.extend(s.post(id::workspace(), vec![], Payload::WorkspaceInfo(bep::WorkspaceConfig { local_exec_root: workspace })));
            out
        }
    }

    /// The build's targets, once the requested ones (`roots`) are in
    /// `plan`: configurations, the pattern's expansion and
    /// `TargetConfigured` events. Every target the requested ones need is
    /// reported, those with nothing to build (up to date) completing at
    /// once. `patterns` are the targets built (see `announce`);
    /// `remote_outputs`, outputs left in the remote CAS.
    pub fn plan(&self, state: &State, plan: &Plan, roots: &[NodeId], targets_file: Arc<Targets>, patterns: &[String], remote_outputs: Option<&RemoteOutputs>) {
        self.announce(patterns);
        // The edges the requested targets need, through every input and
        // validation, and those the plan has besides.
        let mut edges: BTreeSet<EdgeId> = plan.want.keys().copied().collect();
        let mut seen = std::collections::HashSet::new();
        let mut stack = roots.to_vec();
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            let Some(e) = state.node(n).in_edge else { continue };
            edges.insert(e);
            let edge = state.edge(e);
            stack.extend(edge.inputs.iter().chain(&edge.validations).copied());
        }
        let mut targets: BTreeMap<TargetKey, TargetState> = BTreeMap::new();
        let mut edge_targets: HashMap<EdgeId, TargetKey> = HashMap::new();
        let mut counted = std::collections::HashSet::new();
        let mut configs: BTreeMap<Option<String>, Configuration> = BTreeMap::new();
        let mut cas: HashMap<String, (String, i64)> = HashMap::new();
        for edge in edges {
            if state.is_phony(edge) {
                continue;
            }
            let want = plan.want.get(&edge).copied().unwrap_or(Want::Nothing);
            let binding = |k: &str| state.edge_binding(edge, k).ok().filter(|v| !v.is_empty());
            let Some(name) = binding("rbe_target") else { continue };
            let config_name = binding("rbe_config");
            let config = configs
                .entry(config_name.clone())
                .or_insert_with(|| targets_file.configuration(config_name.as_deref()).cloned().unwrap_or_else(default_configuration));
            let key = (name.clone(), config_name);
            let t = targets.entry(key.clone()).or_insert_with(|| TargetState {
                label: targets_file.label(&name),
                config_id: config.id(),
                remaining: 0,
                artifacts: Vec::new(),
                done: false,
            });
            edge_targets.insert(edge, key);
            if want != Want::Nothing {
                t.remaining += 1;
                counted.insert(edge);
            }
            if binding("rbe_artifact").as_deref() == Some("1") || binding("rbe_deliverable").as_deref() == Some("1") {
                for &o in &state.edge(edge).outputs {
                    let path = state.path(o).to_string();
                    // Up to date and left remote (minimal downloads): its
                    // CAS reference.
                    if let Some(e) = remote_outputs.and_then(|r| r.get(&path)) {
                        cas.insert(path.clone(), (e.digest.hash.clone(), e.digest.size_bytes));
                    }
                    t.artifacts.push(path);
                }
            }
        }
        // One TargetConfigured per label, announcing a completion per
        // configuration it is built in.
        let mut labels: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
        for ((name, _), t) in &targets {
            let kind = targets_file.get(name).map(|t| t.kind.clone()).filter(|k| !k.is_empty());
            let e = labels.entry(t.label.clone()).or_insert_with(|| (kind.map_or_else(|| "ninja rule".into(), |k| format!("{k} rule")), Vec::new()));
            e.1.push(t.config_id.clone());
        }
        let ready: Vec<TargetKey> = targets.iter().filter(|(_, t)| t.remaining == 0).map(|(k, _)| k.clone()).collect();
        self.emit(|i| {
            i.targets = targets;
            i.edge_targets = edge_targets;
            i.counted = counted;
            i.planned = true;
            for (path, digest) in cas {
                i.cas.entry(path).or_insert(digest);
            }
            i.targets_file = targets_file;
            i.targets_configured = labels.len() as i64;
            let s = &mut i.stream;
            let mut out = Vec::new();
            for c in configs.values() {
                out.extend(s.post(
                    id::configuration(&c.id()),
                    vec![],
                    Payload::Configuration(bep::Configuration {
                        mnemonic: c.mnemonic.clone(),
                        platform_name: c.platform.clone(),
                        cpu: c.cpu.clone(),
                        make_variable: c.variables.clone().into_iter().collect(),
                        is_tool: false,
                    }),
                ));
            }
            // The pattern BuildStarted announced (the targets named on the
            // command line, if events went out before the plan).
            let announced = i.patterns.clone().unwrap_or_default();
            if !announced.is_empty() {
                out.extend(s.post(
                    id::pattern(&announced),
                    labels.keys().map(|l| id::target_configured(l)).collect(),
                    Payload::Expanded(bep::PatternExpanded::default()),
                ));
            }
            for (label, (kind, config_ids)) in &labels {
                out.extend(s.post(
                    id::target_configured(label),
                    config_ids.iter().map(|c| id::target_completed(label, c)).collect(),
                    Payload::Configured(bep::TargetConfigured { target_kind: kind.clone(), ..Default::default() }),
                ));
            }
            out
        });
        for key in ready {
            self.complete(&key, true);
        }
    }

    /// An edge was added to the plan (`added`) or removed from it after
    /// the targets were planned: its target waits for one more edge, or one
    /// fewer (and completes when none is left).
    pub fn plan_changed(&self, edge: EdgeId, added: bool) {
        let complete = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.planned {
                return;
            }
            let Some(key) = inner.edge_targets.get(&edge).cloned() else { return };
            if added {
                if inner.targets.get(&key).is_some_and(|t| !t.done) && inner.counted.insert(edge) {
                    inner.targets.get_mut(&key).expect("a target").remaining += 1;
                }
                None
            } else {
                inner.counted.remove(&edge).then(|| inner.count_down(&key)).flatten()
            }
        };
        if let Some(key) = complete {
            self.complete(&key, true);
        }
    }

    /// Sends `TargetCompleted` for a target (once). A successful one with
    /// artifacts waits for their digests (`report_targets`), unless events
    /// go nowhere.
    fn complete(&self, key: &TargetKey, success: bool) {
        let (paths, cas) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(t) = inner.targets.get_mut(key) else { return };
            if t.done {
                return;
            }
            t.done = true;
            let mut seen = BTreeSet::new();
            let paths: Vec<String> = t.artifacts.iter().filter(|p| seen.insert(p.as_str())).cloned().collect();
            let cas: CasOutputs = paths.iter().filter_map(|p| Some((p.clone(), inner.cas.get(p)?.clone()))).collect();
            (paths, cas)
        };
        if success && !paths.is_empty() && self.has_sink() && self.hashing.send(Hashing::Target { key: key.clone(), paths, cas }).is_ok() {
            return;
        }
        self.report_completed(key, success, Vec::new());
    }

    /// Whether events go anywhere: to the invocation running this one, the
    /// file, or an upload that hasn't failed.
    fn has_sink(&self) -> bool {
        self.relay_out.lock().unwrap().is_some()
            || self.file.lock().unwrap().is_some()
            || self.uploader.lock().unwrap().as_ref().is_some_and(|u| !u.is_closed())
    }

    /// The build was interrupted: targets waiting for their artifacts'
    /// digests are reported without them (`settle` and `finish` don't wait
    /// for the hashing).
    pub fn interrupted(&self) {
        self.hurry.send_replace(true);
    }

    /// Waits until the targets completed so far are reported: their
    /// artifacts' digests are in the store's cache then (but after an
    /// interruption).
    pub async fn settle(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.hashing.send(Hashing::Settled(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    /// Sends `TargetCompleted` for a target, with its outputs' `files`.
    fn report_completed(&self, key: &TargetKey, success: bool, files: Vec<bep::File>) {
        self.emit(|i| {
            let t = &i.targets[key];
            let (label, config_id) = (t.label.clone(), t.config_id.clone());
            let mut out = Vec::new();
            let mut file_sets = Vec::new();
            if !files.is_empty() {
                let set = i.named_sets.to_string();
                i.named_sets += 1;
                out.extend(i.stream.post(id::named_set(&set), vec![], Payload::NamedSetOfFiles(bep::NamedSetOfFiles { files, file_sets: vec![] })));
                file_sets.push(bep::build_event_id::NamedSetOfFilesId { id: set });
            }
            let output_group = if success { vec![bep::OutputGroup { name: "default".into(), file_sets, ..Default::default() }] } else { vec![] };
            out.extend(i.stream.post(
                id::target_completed(&label, &config_id),
                vec![],
                Payload::Completed(bep::TargetComplete { success, output_group, ..Default::default() }),
            ));
            out
        });
    }

    /// The BEP entry for an output: a CAS reference when it was built
    /// remotely (and there is a remote cache), else the local file, hashed
    /// unless the file digest cache knows it.
    fn file_entry(&self, path: &str, cas: &CasOutputs) -> Option<bep::File> {
        let p = Path::new(path);
        let name = p.file_name()?.to_string_lossy().into_owned();
        let path_prefix =
            p.parent().map(|d| d.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).filter(|c| !c.is_empty()).collect()).unwrap_or_default();
        let (uri, digest, length) = match (cas.get(path), &self.bytestream) {
            (Some((hash, size)), Some(bs)) => (format!("{bs}/blobs/{hash}/{size}"), hash.clone(), *size),
            _ => {
                let d = rbe_client::tree::file_digest(p, Some(&self.store.digests)).ok()?;
                let abs = std::path::absolute(p).ok()?;
                (file_uri(&abs.to_string_lossy()), d.hash, d.size_bytes)
            }
        };
        Some(bep::File { path_prefix, name, file: Some(bep::file::File::Uri(uri)), digest, length })
    }

    /// An edge started. One the targets weren't planned with (a dyndep
    /// file brought it) counts in its target from now, unless that one
    /// completed already.
    pub fn edge_started(&self, info: &EdgeInfo) {
        let mut inner = self.inner.lock().unwrap();
        inner.starts.insert(info.edge, SystemTime::now());
        let Some(target) = info.binding("rbe_target") else { return };
        let key = (target.to_string(), info.binding("rbe_config").map(str::to_string));
        if inner.planned && !inner.counted.contains(&info.edge) && inner.targets.get(&key).is_some_and(|t| !t.done) {
            inner.counted.insert(info.edge);
            inner.edge_targets.insert(info.edge, key.clone());
            inner.targets.get_mut(&key).expect("a target").remaining += 1;
        }
    }

    /// Reports a finished edge: its console output, its action (if failed or
    /// all are published) and its target's progress.
    pub fn edge_finished(&self, info: &EdgeInfo, exit_code: Option<i32>, kind: &ExecKind, output: &[u8], progress: &str) {
        let end = SystemTime::now();
        let success = exit_code.is_none();
        let mnemonic = info.binding("rbe_mnemonic").unwrap_or("NinjaAction").to_string();
        let target = info.binding("rbe_target").map(str::to_string);
        let key = target.clone().map(|t| (t, info.binding("rbe_config").map(str::to_string)));
        let (label, config_id, start) = {
            let mut inner = self.inner.lock().unwrap();
            let start = inner.starts.remove(&info.edge).unwrap_or(end);
            let m = inner.mnemonics.entry(mnemonic.clone()).or_default();
            m.created += 1;
            if !matches!(kind, ExecKind::RemoteCacheHit | ExecKind::DiskCacheHit) {
                m.executed += 1;
            }
            m.first_ms = if m.first_ms == 0 { millis(start) } else { m.first_ms.min(millis(start)) };
            m.last_ms = m.last_ms.max(millis(end));
            let runner = match kind {
                ExecKind::Remote => RUNNERS[0],
                ExecKind::RemoteCacheHit => RUNNERS[1],
                ExecKind::DiskCacheHit => RUNNERS[2],
                _ => RUNNERS[3],
            };
            inner.runners.entry(runner.0).or_insert((0, runner.1)).0 += 1;
            for c in &info.cas_outputs {
                inner.cas.insert(c.path.clone(), (c.hash.clone(), c.size));
            }
            match key.as_ref().and_then(|k| inner.targets.get(k)) {
                Some(t) => (t.label.clone(), t.config_id.clone(), start),
                None => {
                    let config = inner.targets_file.configuration(info.binding("rbe_config")).cloned().unwrap_or_else(default_configuration);
                    (target.as_deref().map(|t| inner.targets_file.label(t)).unwrap_or_default(), config.id(), start)
                }
            }
        };

        let mut console = format!("{progress}{}\n", if info.description.is_empty() { &info.command } else { &info.description });
        if let Some(code) = exit_code {
            let code = if code != 1 { format!("[code={code}] ") } else { String::new() };
            console.push_str(&format!("FAILED: {code}{}\n{}\n", info.outputs.join(" "), info.command));
        }
        let output = String::from_utf8_lossy(output).into_owned();
        console.push_str(&output);
        self.emit(|i| i.stream.progress("", &console));

        if !success || self.flags.publish_all_actions {
            let primary = info.outputs.first().cloned().unwrap_or_default();
            let command_line = match ninja_remote::cmdline::unwrap_command(&info.command, cfg!(windows)) {
                Ok(u) => u.steps.join(&["&&".to_string()][..]),
                Err(_) => vec![info.command.clone()],
            };
            #[allow(deprecated)]
            let event = bep::ActionExecuted {
                success,
                r#type: mnemonic,
                exit_code: exit_code.unwrap_or(0),
                stderr: (!output.is_empty()).then(|| bep::File {
                    name: "stderr".into(),
                    file: Some(bep::file::File::Contents(output.clone().into_bytes())),
                    ..Default::default()
                }),
                label: label.clone(),
                configuration: Some(bep::build_event_id::ConfigurationId { id: config_id.clone() }),
                primary_output: Some(bep::File { name: primary.clone(), ..Default::default() }),
                command_line,
                start_time: Some(timestamp(start)),
                end_time: Some(timestamp(end)),
                ..Default::default()
            };
            self.emit(|i| i.stream.post(id::action_completed(&primary, &label, &config_id), vec![], Payload::Action(event)));
        }

        if let Some(key) = key {
            let done = {
                let mut inner = self.inner.lock().unwrap();
                // A failed edge fails its target; only an edge it counted
                // counts down.
                let counted = inner.counted.remove(&info.edge);
                if !success { inner.targets.get(&key).is_some_and(|t| !t.done) } else { counted && inner.count_down(&key).is_some() }
            };
            if done {
                self.complete(&key, success);
            }
        }
    }

    /// A message (`ninja: ...`), as console output.
    pub fn message(&self, text: &str) {
        // Before the targets are known, kept for after BuildStarted.
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.patterns.is_none() {
                inner.early_console.push_str(text);
                return;
            }
        }
        self.emit(|i| i.stream.progress("", text));
    }

    /// Ends the stream (targets that never completed are aborted, then the
    /// build's end and metrics), and waits for the upload, up to
    /// `--bes_timeout`.
    pub async fn finish(&self, exit_code: i32) {
        let interrupted = exit_code == crate::EXIT_INTERRUPTED;
        if interrupted {
            self.interrupted();
        }
        self.settle().await;
        let reason = if interrupted {
            bep::aborted::AbortReason::UserInterrupted
        } else if exit_code != 0 {
            bep::aborted::AbortReason::Skipped
        } else {
            bep::aborted::AbortReason::Incomplete
        };
        let wall_ms = self.started.elapsed().as_millis() as i64;
        self.emit(|i| {
            let mut out = Vec::new();
            for t in i.targets.values_mut().filter(|t| !t.done) {
                t.done = true;
                out.extend(i.stream.post(
                    id::target_completed(&t.label, &t.config_id),
                    vec![],
                    Payload::Aborted(bep::Aborted { reason: reason as i32, description: "the target's actions did not all run".into() }),
                ));
            }
            let metrics = bep::BuildMetrics {
                action_summary: Some(bep::build_metrics::ActionSummary {
                    actions_created: i.mnemonics.values().map(|m| m.created).sum(),
                    actions_executed: i.mnemonics.values().map(|m| m.executed).sum(),
                    action_data: i
                        .mnemonics
                        .iter()
                        .map(|(name, m)| bep::build_metrics::action_summary::ActionData {
                            mnemonic: name.clone(),
                            actions_executed: m.executed,
                            actions_created: m.created,
                            first_started_ms: m.first_ms,
                            last_ended_ms: m.last_ms,
                            ..Default::default()
                        })
                        .collect(),
                    runner_count: std::iter::once(bep::build_metrics::action_summary::RunnerCount {
                        name: "total".into(),
                        count: i.runners.values().map(|(c, _)| c).sum(),
                        exec_kind: String::new(),
                    })
                    .chain(i.runners.iter().map(|(name, (count, kind))| bep::build_metrics::action_summary::RunnerCount {
                        name: (*name).into(),
                        count: *count,
                        exec_kind: (*kind).into(),
                    }))
                    .collect(),
                    ..Default::default()
                }),
                target_metrics: Some(bep::build_metrics::TargetMetrics { targets_configured: i.targets_configured, ..Default::default() }),
                timing_metrics: Some(bep::build_metrics::TimingMetrics { wall_time_in_ms: wall_ms, ..Default::default() }),
                ..Default::default()
            };
            let logs = bep::BuildToolLogs {
                log: vec![bep::File {
                    name: "elapsed time".into(),
                    file: Some(bep::file::File::Contents(format!("{:.6}", wall_ms as f64 / 1000.0).into_bytes())),
                    ..Default::default()
                }],
            };
            let (name, code) = match exit_code {
                0 => ("SUCCESS", 0),
                c if c == crate::EXIT_INTERRUPTED => ("INTERRUPTED", 8),
                _ => ("BUILD_FAILURE", 1),
            };
            out.extend(i.stream.post(
                id::build_finished(),
                vec![id::build_metrics(), id::build_tool_logs()],
                Payload::Finished(bep::BuildFinished {
                    exit_code: Some(bep::build_finished::ExitCode { name: name.into(), code }),
                    finish_time: Some(timestamp(SystemTime::now())),
                    ..Default::default()
                }),
            ));
            out.extend(i.stream.finish(vec![(id::build_metrics(), Payload::BuildMetrics(metrics)), (id::build_tool_logs(), Payload::BuildToolLogs(logs))]));
            out
        });
        if let Some(mut f) = self.file.lock().unwrap().take() {
            let _ = f.flush();
        }
        if let Some(r) = &self.relay_in {
            crate::nested::Relay::remove(r);
        }
        // A nested invocation's stream ends with an empty frame, which the
        // outer invocation acknowledges once it merged the events before
        // it: they are in before the command running this one ends (with
        // `-t exec`, the outer invocation ends with it).
        let relay = self.relay_out.lock().unwrap().take();
        if let Some(mut stream) = relay
            && crate::nested::write_frame(&mut stream, &[]).is_ok()
        {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(std::io::Read::read_exact(&mut stream, &mut [0u8; 1]));
            });
            let _ = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10))).await;
        }
        let uploader = self.uploader.lock().unwrap().take();
        if let Some(u) = uploader {
            let outcome = BuildOutcome { success: exit_code == 0, exit_code };
            // Zero waits as long as it takes, as Bazel's --bes_timeout=0.
            let timeout = Some(self.flags.timeout).filter(|t| !t.is_zero()).unwrap_or(Duration::MAX);
            match tokio::time::timeout(timeout, u.finish(outcome)).await {
                Ok(Ok(summary)) => {
                    if let Some(e) = summary.lifecycle_error {
                        let off =
                            if e.contains("UNIMPLEMENTED") || e.contains("Unimplemented") { " (--nobes_lifecycle_events stops sending them)" } else { "" };
                        eprintln!("ninja: warning: build events were uploaded, not all their lifecycle events: {e}{off}");
                    }
                }
                Ok(Err(e)) => {
                    let e = format!("{e:#}");
                    let first_contact = e.contains("never answered") || e.contains("connecting to");
                    let hint = self.flags.backend.as_deref().map(|b| crate::options::tls_hint(&e, b, &self.tls, first_contact)).unwrap_or_default();
                    eprintln!("ninja: warning: build event upload failed: {e}{hint}");
                }
                Err(_) => eprintln!("ninja: warning: build event upload didn't finish within {timeout:?}"),
            }
            if let Some(url) = &self.flags.results_url {
                eprintln!("ninja: streaming build results to: {url}{}", self.ids.invocation_id);
            }
        }
    }
}

/// A `file://` URI for an absolute path: `file:///C:/x` on Windows.
fn file_uri(abs: &str) -> String {
    let path = abs.replace('\\', "/");
    if path.starts_with('/') { format!("file://{path}") } else { format!("file:///{path}") }
}

/// Targets whose artifacts are hashed at once.
const HASHERS: usize = 4;

/// Reports completed targets, in order, once their artifacts are hashed:
/// off the build loop and outside `Bes::inner` (relayed events go on), as
/// an artifact can be gigabytes, `HASHERS` targets at once. Once `hurry`
/// is set (an interruption), those still waiting are reported without
/// their artifacts.
async fn report_targets(bes: Weak<Bes>, mut jobs: tokio::sync::mpsc::UnboundedReceiver<Hashing>, hurry: tokio::sync::watch::Receiver<bool>) {
    enum Started {
        Target(TargetKey, tokio::task::JoinHandle<Vec<bep::File>>),
        Settled(tokio::sync::oneshot::Sender<()>),
    }
    let (tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    // Starts hashing the targets as they come.
    {
        let (bes, hurry) = (bes.clone(), hurry.clone());
        let pool = Arc::new(tokio::sync::Semaphore::new(HASHERS));
        tokio::spawn(async move {
            while let Some(job) = jobs.recv().await {
                let s = match job {
                    Hashing::Target { key, paths, cas } => {
                        let (bes, pool, hurry) = (bes.clone(), pool.clone(), hurry.clone());
                        Started::Target(
                            key,
                            tokio::spawn(async move {
                                let Ok(_hashing) = pool.acquire_owned().await else { return Vec::new() };
                                match bes.upgrade() {
                                    Some(b) if !*hurry.borrow() => {
                                        tokio::task::spawn_blocking(move || paths.iter().filter_map(|p| b.file_entry(p, &cas)).collect())
                                            .await
                                            .unwrap_or_default()
                                    }
                                    _ => Vec::new(),
                                }
                            }),
                        )
                    }
                    Hashing::Settled(done) => Started::Settled(done),
                };
                if tx.send(s).is_err() {
                    return;
                }
            }
        });
    }
    // Reports them in order.
    let mut hurry = hurry;
    while let Some(s) = started.recv().await {
        match s {
            Started::Target(key, mut hashed) => {
                let files = tokio::select! {
                    biased;
                    files = &mut hashed => files.unwrap_or_default(),
                    _ = hurry.wait_for(|h| *h) => Vec::new(),
                };
                let Some(bes) = bes.upgrade() else { return };
                bes.report_completed(&key, true, files);
            }
            Started::Settled(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// The status printer, also feeding build events when they are enabled.
pub struct ReportingStatus<'a> {
    pub printer: &'a mut StatusPrinter,
    pub bes: Option<Arc<Bes>>,
}

impl Status for ReportingStatus<'_> {
    fn edge_added_to_plan(&mut self, edge: EdgeId) {
        self.printer.edge_added_to_plan(edge);
        if let Some(b) = &self.bes {
            b.plan_changed(edge, true);
        }
    }
    fn edge_removed_from_plan(&mut self, edge: EdgeId) {
        self.printer.edge_removed_from_plan(edge);
        if let Some(b) = &self.bes {
            b.plan_changed(edge, false);
        }
    }
    fn build_edge_started(&mut self, info: &EdgeInfo, start_ms: i64) {
        self.printer.build_edge_started(info, start_ms);
        if let Some(b) = &self.bes {
            b.edge_started(info);
        }
    }
    fn build_edge_finished(&mut self, info: &EdgeInfo, end_ms: i64, exit_code: Option<i32>, kind: &ExecKind, output: &[u8]) {
        self.printer.build_edge_finished(info, end_ms, exit_code, kind, output);
        if let Some(b) = &self.bes {
            let progress = self.printer.format_progress_status("[%f/%t] ", end_ms);
            b.edge_finished(info, exit_code, kind, output, &progress);
        }
    }
    fn build_edge_rewound(&mut self, info: &EdgeInfo, lost: &[String]) {
        self.printer.build_edge_rewound(info, lost);
    }
    fn build_started(&mut self) {
        self.printer.build_started();
    }
    fn build_finished(&mut self) {
        self.printer.build_finished();
    }
    fn info(&mut self, msg: &str) {
        self.printer.info(msg);
        if let Some(b) = &self.bes {
            b.message(&format!("ninja: {msg}\n"));
        }
    }
    fn warning(&mut self, msg: &str) {
        self.printer.warning(msg);
        if let Some(b) = &self.bes {
            b.message(&format!("ninja: warning: {msg}\n"));
        }
    }
    fn error(&mut self, msg: &str) {
        self.printer.error(msg);
        if let Some(b) = &self.bes {
            b.message(&format!("ninja: error: {msg}\n"));
        }
    }
    fn failure_summary(&mut self) {
        self.printer.failure_summary();
    }
    fn explain(&mut self, msg: &str) {
        self.printer.explain(msg);
    }
}

/// Reads the events a `--build_event_binary_file` holds.
pub fn read_binary_file(path: &Path) -> std::io::Result<Vec<bep::BuildEvent>> {
    let data = std::fs::read(path)?;
    let mut buf = data.as_slice();
    let mut events = Vec::new();
    while !buf.is_empty() {
        events.push(bep::BuildEvent::decode_length_delimited(&mut buf).map_err(std::io::Error::other)?);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_version_4_and_differ() {
        let (a, b) = (new_uuid(), new_uuid());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn bytestream_prefixes() {
        assert_eq!(bytestream_prefix("grpcs://cache.example.com:443", ""), "bytestream://cache.example.com:443");
        assert_eq!(bytestream_prefix("grpc://127.0.0.1:8980/", "main"), "bytestream://127.0.0.1:8980/main");
    }

    fn options(args: &[&str]) -> crate::options::Options {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match crate::options::parse_args(&v).unwrap() {
            crate::options::ParseOutcome::Run(o) => *o,
            crate::options::ParseOutcome::Exit(m, _) => panic!("exit: {m}"),
        }
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = test_dirs::base().join(format!("ninja_rs_bes_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A build of one target, up to date, whose artifact is `artifact`:
    /// the graph and the requested node.
    fn artifact_graph(artifact: &Path) -> (State, NodeId) {
        let path = artifact.to_string_lossy().replace('$', "$$").replace(':', "$:").replace(' ', "$ ");
        let manifest = format!("rule gen\n  command = gen\nbuild {path}: gen\n  rbe_target = app\n  rbe_artifact = 1\n");
        let mut state = State::new();
        let files = ninja_core::parser::MemFiles::default();
        ninja_core::parser::ManifestParser::new(&mut state, &files, Default::default()).parse_test(&manifest).unwrap();
        let node = state.edges[0].outputs[0];
        (state, node)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn artifacts_arent_hashed_for_events_going_nowhere() {
        let dir = test_dir("nowhere");
        let artifact = dir.join("app.bin");
        std::fs::write(&artifact, "hello").unwrap();
        let opts = options(&[&format!("--build_event_binary_file={}", dir.join("missing").join("events.bep").display())]);
        let store = Store::new();
        let bes = Bes::start(&opts, &InvocationIds::default(), None, store.clone(), None).await;
        let (state, node) = artifact_graph(&artifact);
        bes.plan(&state, &Plan::default(), &[node], Default::default(), &[], None);
        bes.finish(0).await;
        assert!(store.digests.entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn artifacts_are_hashed_off_the_build_loop() {
        let dir = test_dir("off_the_loop");
        // An artifact that can't be read until something writes to it.
        let artifact = dir.join("app.bin");
        let c_path = std::ffi::CString::new(artifact.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let file = dir.join("events.bep");
        let bes = Bes::start(&options(&[&format!("--build_event_binary_file={}", file.display())]), &InvocationIds::default(), None, Store::new(), None).await;
        let (state, node) = artifact_graph(&artifact);
        // The build goes on, and so do other events (relayed ones).
        let b = bes.clone();
        let planned = tokio::task::spawn_blocking(move || {
            b.plan(&state, &Plan::default(), &[node], Default::default(), &[], None);
            b.message("ninja: no work to do.\n");
        });
        let planned = tokio::time::timeout(Duration::from_secs(5), planned).await;
        let writer = std::thread::spawn(move || std::fs::write(&artifact, "hello"));
        planned.expect("the build waited for the artifact's digest").unwrap();
        writer.join().unwrap().unwrap();
        bes.finish(0).await;
        let digests: Vec<String> = read_binary_file(&file)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                Some(Payload::NamedSetOfFiles(s)) => Some(s.files.into_iter().map(|f| f.digest)),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(digests, [rbe_client::digest_of(b"hello").hash]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_build_doesnt_wait_for_the_backend() {
        // A backend accepting connections and never answering (the TLS
        // handshake waits for it).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().collect();
            drop(held);
        });
        let opts = options(&[&format!("--bes_backend=grpcs://127.0.0.1:{port}"), "--bes_timeout=1s"]);
        let started = tokio::time::timeout(Duration::from_secs(5), Bes::start(&opts, &InvocationIds::default(), None, Store::new(), None)).await;
        let bes = started.expect("the build waited for the backend");
        // The end waits for the upload as long as --bes_timeout says.
        let start = Instant::now();
        bes.finish(0).await;
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    }

    /// A build of up-to-date targets, each with one artifact edge: the
    /// graph and the requested nodes.
    fn targets_graph(artifacts: &[(&str, &Path)]) -> (State, Vec<NodeId>) {
        let mut manifest = "rule gen\n  command = gen\n".to_string();
        for (target, artifact) in artifacts {
            let path = artifact.to_string_lossy().replace('$', "$$").replace(':', "$:").replace(' ', "$ ");
            manifest.push_str(&format!("build {path}: gen\n  rbe_target = {target}\n  rbe_artifact = 1\n"));
        }
        let mut state = State::new();
        let files = ninja_core::parser::MemFiles::default();
        ninja_core::parser::ManifestParser::new(&mut state, &files, Default::default()).parse_test(&manifest).unwrap();
        let nodes = state.edges.iter().map(|e| e.outputs[0]).collect();
        (state, nodes)
    }

    /// The labels of the targets completed, in order, and whether each had
    /// files.
    #[cfg(unix)]
    fn completed(file: &Path) -> Vec<(String, bool)> {
        read_binary_file(file)
            .unwrap()
            .into_iter()
            .filter_map(|e| match (e.id.and_then(|i| i.id), e.payload) {
                (Some(bep::build_event_id::Id::TargetCompleted(t)), Some(Payload::Completed(c))) => {
                    Some((t.label, c.output_group.iter().any(|g| !g.file_sets.is_empty())))
                }
                _ => None,
            })
            .collect()
    }

    /// A file that can't be read until the end returned writes to it and
    /// closes it (a FIFO held open for writing: opening it to read doesn't
    /// wait, reading does).
    #[cfg(unix)]
    fn fifo(path: &Path) -> std::fs::File {
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        std::fs::File::options().read(true).write(true).open(path).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn interrupted_builds_dont_wait_for_hashing() {
        let dir = test_dir("interrupted");
        let artifact = dir.join("app.bin");
        let mut writer = fifo(&artifact);
        let file = dir.join("events.bep");
        let bes = Bes::start(&options(&[&format!("--build_event_binary_file={}", file.display())]), &InvocationIds::default(), None, Store::new(), None).await;
        let (state, nodes) = targets_graph(&[("app", &artifact)]);
        bes.plan(&state, &Plan::default(), &nodes, Default::default(), &[], None);
        let finished = tokio::time::timeout(Duration::from_secs(5), bes.finish(crate::EXIT_INTERRUPTED)).await;
        // Unblocks the hashing, if it started.
        writer.write_all(b"hello").unwrap();
        drop(writer);
        finished.expect("the end waited for the artifact's digest");
        assert_eq!(completed(&file), [("//:app".to_string(), false)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn artifacts_are_hashed_several_targets_at_once() {
        let dir = test_dir("at_once");
        let (slow, quick) = (dir.join("slow.bin"), dir.join("quick.bin"));
        let mut writer = fifo(&slow);
        std::fs::write(&quick, "quick").unwrap();
        // Old enough for its digest to be kept.
        std::fs::File::options().write(true).open(&quick).unwrap().set_modified(SystemTime::now() - Duration::from_secs(3600)).unwrap();
        let file = dir.join("events.bep");
        let store = Store::new();
        let bes = Bes::start(&options(&[&format!("--build_event_binary_file={}", file.display())]), &InvocationIds::default(), None, store.clone(), None).await;
        let (state, nodes) = targets_graph(&[("a_slow", &slow), ("b_quick", &quick)]);
        bes.plan(&state, &Plan::default(), &nodes, Default::default(), &[], None);
        // The second target's artifact is hashed while the first waits.
        let hashed = async {
            while !store.digests.entries().iter().any(|(p, _, _)| p.ends_with("quick.bin")) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let hashed = tokio::time::timeout(Duration::from_secs(5), hashed).await;
        writer.write_all(b"slow").unwrap();
        drop(writer);
        hashed.expect("the targets were hashed one at a time");
        bes.finish(0).await;
        // Reported in order all the same.
        assert_eq!(completed(&file), [("//:a_slow".to_string(), true), ("//:b_quick".to_string(), true)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_after_the_end_go_nowhere() {
        let dir = test_dir("after_the_end");
        let file = dir.join("events.bep");
        let bes = Bes::start(&options(&[&format!("--build_event_binary_file={}", file.display())]), &InvocationIds::default(), None, Store::new(), None).await;
        bes.announce(&[]);
        bes.finish(0).await;
        // A message, a nested invocation's event relayed late.
        bes.message("ninja: late\n");
        let source = RelaySource::new(&dir, &dir.join("nested").to_string_lossy(), 1);
        let late = bep::BuildEvent {
            id: Some(id::progress(0)),
            payload: Some(Payload::Progress(bep::Progress { stdout: String::new(), stderr: "late\n".into() })),
            ..Default::default()
        };
        bes.relayed(&source, late);
        rbe_bes::events::validate(&read_binary_file(&file).unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn edges_the_plan_didnt_know_count_once_started() {
        let dir = test_dir("dyndep");
        let (a, b, d) = (dir.join("a.o"), dir.join("b.o"), dir.join("d.o"));
        let (state, nodes) = targets_graph(&[("app", &a), ("app", &b), ("app", &d)]);
        let bes = Bes::start(&options(&[]), &InvocationIds::default(), None, Store::new(), None).await;
        // The plan wants a and b; a dyndep file brings d later.
        let mut plan = Plan::default();
        for e in [EdgeId(0), EdgeId(1)] {
            plan.want.insert(e, Want::ToStart);
        }
        bes.plan(&state, &plan, &nodes[..2], Default::default(), &[], None);
        let info = |e: u32, out: &Path| EdgeInfo {
            edge: EdgeId(e),
            description: String::new(),
            command: "gen".into(),
            outputs: vec![out.to_string_lossy().into_owned()],
            use_console: false,
            bindings: vec![("rbe_target".into(), "app".into())],
            cas_outputs: Vec::new(),
        };
        let run = |e: u32, out: &Path| {
            bes.edge_started(&info(e, out));
            bes.edge_finished(&info(e, out), None, &ExecKind::Local, b"", "");
        };
        let done = || bes.inner.lock().unwrap().targets[&("app".to_string(), None)].done;
        run(2, &d);
        run(0, &a);
        assert!(!done(), "the target completed with b still to run");
        run(1, &b);
        assert!(done());
        bes.finish(0).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_uris() {
        assert_eq!(file_uri("/home/u/out/app"), "file:///home/u/out/app");
        assert_eq!(file_uri(r"C:\b\out\app.exe"), "file:///C:/b/out/app.exe");
    }
}
