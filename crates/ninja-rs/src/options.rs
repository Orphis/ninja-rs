//! Command-line parsing compatible with ninja's getopt usage, plus the
//! remote execution flags.

use ninja_core::status::Verbosity;

#[derive(Clone, Debug)]
pub struct Options {
    pub working_dir: Option<String>,
    pub input_file: String,
    pub parallelism: usize,
    /// `-j` was given.
    pub explicit_parallelism: bool,
    /// `--jobserver-pool`: hand a jobserver pool to the commands.
    pub jobserver_pool: bool,
    /// `usize::MAX` for `-k 0`.
    pub failures_allowed: usize,
    pub dry_run: bool,
    pub verbosity: Verbosity,
    pub explain: bool,
    pub keep_depfile: bool,
    pub keep_rsp: bool,
    /// `-d stats`: print operation counts and timings at exit.
    pub stats: bool,
    /// `-d nostatcache`: don't batch stats per directory.
    pub no_stat_cache: bool,
    pub phonycycle_err: bool,
    /// `--status FMT`: the status line with ninja-style variables.
    pub status_format: Option<String>,
    pub tool: Option<String>,
    pub tool_args: Vec<String>,
    pub targets: Vec<String>,
    pub remote: RemoteFlags,
    pub bes: BesFlags,
    /// Certificates for `grpcs://` endpoints: the remote service and the
    /// Build Event Service.
    pub tls: TlsFlags,
    /// The whole command line (for build events).
    pub argv: Vec<String>,
    /// The options given, in order (for build events).
    pub given: Vec<GivenOption>,
}

/// An option as given on the command line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GivenOption {
    /// As written: `--remote_executor=grpcs://...`, `-j 8`.
    pub combined_form: String,
    /// Without dashes: `remote_executor`, `j`.
    pub name: String,
    pub value: String,
}

/// TLS for `grpcs://` endpoints, with Bazel's flag names. Paths are
/// absolute (relative ones are resolved where ninja-rs started, not -C).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TlsFlags {
    /// `--tls_certificate`: a CA (PEM) trusted to sign server certificates,
    /// besides the web PKI roots.
    pub certificate: Option<String>,
    /// `--tls_client_certificate` and `--tls_client_key`: the client's
    /// certificate and private key (PEM), for mTLS.
    pub client_certificate: Option<String>,
    pub client_key: Option<String>,
    /// `--tls_authority_override`: the name server certificates are checked
    /// against, instead of the endpoint's host.
    pub authority_override: Option<String>,
}

impl TlsFlags {
    /// Reads the certificate files.
    pub fn load(&self) -> Result<ninja_remote::Tls, String> {
        let read = |flag: &str, p: &Option<String>| -> Result<Option<Vec<u8>>, String> {
            p.as_ref().map(|p| std::fs::read(p).map_err(|e| format!("--{flag}={p}: {e}"))).transpose()
        };
        Ok(ninja_remote::Tls {
            ca: read("tls_certificate", &self.certificate)?,
            client_cert: read("tls_client_certificate", &self.client_certificate)?,
            client_key: read("tls_client_key", &self.client_key)?,
            authority_override: self.authority_override.clone(),
        })
    }
}

/// Explains a failed connection to a `grpcs://` endpoint (or one without a
/// scheme, which means TLS): what was sent, and what the error usually
/// means. Empty for other errors.
pub fn tls_hint(error: &str, endpoint: &str, tls: &TlsFlags) -> String {
    if !(endpoint.starts_with("grpcs://") || endpoint.starts_with("https://") || !endpoint.contains("://")) {
        return String::new();
    }
    let e = error.to_ascii_lowercase();
    let cert = match &tls.client_certificate {
        Some(c) => format!("the client certificate {c}"),
        None => "no client certificate (--tls_client_certificate)".to_string(),
    };
    if e.contains("unknownissuer") || e.contains("invalid peer certificate") {
        let ca = match &tls.certificate {
            Some(c) => format!("the public roots and {c}"),
            None => "the public roots only (--tls_certificate adds a CA)".to_string(),
        };
        format!(
            "
  the server's certificate isn't trusted: checked against {ca}"
        )
    } else if e.contains("broken pipe") || e.contains("connection closed") || e.contains("connection reset") || e.contains("certificate") {
        format!(
            "
  the server closed the connection after the TLS handshake, as when it refuses the client's certificate: sent {cert}"
        )
    } else {
        String::new()
    }
}

/// Build event reporting, with Bazel's flag names.
#[derive(Clone, Debug)]
pub struct BesFlags {
    /// `grpc://host:port` or `grpcs://host:port` of a Build Event Service.
    pub backend: Option<String>,
    /// BES project / instance name.
    pub instance_name: String,
    pub headers: Vec<(String, String)>,
    /// Printed with the invocation id appended.
    pub results_url: Option<String>,
    /// Writes the events, length-delimited, to this file.
    pub binary_file: Option<String>,
    pub keywords: Vec<String>,
    pub lifecycle_events: bool,
    /// Report every action, not only failed ones.
    pub publish_all_actions: bool,
    /// How long to wait for the upload at the end of the build.
    pub timeout_secs: u64,
    pub build_metadata: Vec<(String, String)>,
    pub invocation_id: Option<String>,
    pub build_request_id: Option<String>,
}

impl Default for BesFlags {
    fn default() -> Self {
        Self {
            backend: None,
            instance_name: String::new(),
            headers: Vec::new(),
            results_url: None,
            binary_file: None,
            keywords: Vec::new(),
            lifecycle_events: true,
            publish_all_actions: false,
            timeout_secs: 60,
            build_metadata: Vec::new(),
            invocation_id: None,
            build_request_id: None,
        }
    }
}

impl BesFlags {
    pub fn enabled(&self) -> bool {
        self.backend.is_some() || self.binary_file.is_some()
    }
}

#[derive(Clone, Debug, Default)]
pub struct RemoteFlags {
    pub executor: Option<String>,
    /// `--remote_cache`: a remote cache without remote execution. Actions
    /// remote execution would run are looked up there, and run locally on
    /// a miss.
    pub cache: Option<String>,
    /// `--[no]remote_upload_local_results`: with `cache`, store the results
    /// of actions run locally.
    pub upload_local_results: bool,
    pub instance_name: String,
    pub headers: Vec<(String, String)>,
    pub jobs: usize,
    pub exec_root: Option<String>,
    pub mnemonic_suffixes: Vec<String>,
    pub local_fallback: bool,
    /// `--remote_strict`: actions the policy sends remotely run there or
    /// fail, instead of running locally.
    pub strict: bool,
    /// `--remote_max_fallbacks`: local runs after remote failures allowed.
    pub max_fallbacks: Option<usize>,
    /// `--remote_max_fallback_ratio`: largest share of remote actions that
    /// may run locally after remote failures.
    pub max_fallback_ratio: Option<f64>,
    /// `--remote_racing`: actions sent remotely also run locally when a
    /// local slot is free; the first to finish wins.
    pub racing: bool,
    /// `--remote_racing_delay`: how long the local branch waits first.
    pub racing_delay_ms: u64,
    pub verbose: bool,
    /// `--disk_cache`: the local action cache and CAS (absolute).
    pub disk_cache: Option<String>,
    /// Bytes the disk cache may take before old entries are evicted.
    pub disk_cache_max_size: u64,
    /// `--remote_download_outputs=minimal`: download only deliverables,
    /// what the command line names and what local commands read.
    pub download_minimal: bool,
    /// `--execution_log_file`: where to log every remote action (absolute).
    pub execution_log: Option<String>,
    /// `--remote_include_scanner`: how compiles' inputs are found.
    pub include_scanner: ninja_remote::action::IncludeScanner,
    /// `--[no]daemon` (`NINJA_RS_DAEMON=1`): use the daemon keeping caches
    /// between builds.
    pub daemon: bool,
}

impl RemoteFlags {
    /// The remote service: the executor, or the cache alone.
    pub fn endpoint(&self) -> Option<&String> {
        self.executor.as_ref().or(self.cache.as_ref())
    }

    /// A remote cache without remote execution.
    pub fn cache_only(&self) -> bool {
        self.executor.is_none() && self.cache.is_some()
    }
}

/// `--remote_racing_delay` default: long enough for most remote cache hits,
/// which then don't start local commands.
pub const DEFAULT_RACING_DELAY_MS: u64 = 250;

/// `--disk_cache_max_size` default: 10 GiB.
pub const DEFAULT_DISK_CACHE_MAX_SIZE: u64 = 10 << 30;

pub enum ParseOutcome {
    Run(Box<Options>),
    /// Print this and exit with the code (e.g. --version, -h).
    Exit(String, i32),
}

pub fn guess_parallelism() -> usize {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    match n {
        0 | 1 => 2,
        2 => 3,
        n => n + 2,
    }
}

pub const USAGE: &str = "usage: ninja-rs [options] [targets...]

if targets are unspecified, builds the 'default' target (see manual).

options:
  --version      print ninja version (\"{version}\")
  -v, --verbose  show all command lines while building
  --quiet        don't show progress status, just command output
  --status FMT   progress status format using Ninja-style $vars
                 (e.g. --status '[$finished/$total] ')

  -C DIR   change to DIR before doing anything else
  -f FILE  specify input build file [default=build.ninja]

  -j N     run N jobs in parallel (0 means infinity) [default={jobs} on this system]
  -k N     keep going until N jobs fail (0 means infinity) [default=1]
  -l N     do not start new jobs if the load average is greater than N (ignored)
  -n       dry run (don't run commands but act like they succeeded)

  -d MODE  enable debugging (use '-d list' to list modes)
  -t TOOL  run a subtool (use '-t list' to list subtools)
    terminates toplevel options; further flags are passed to the tool
  -w FLAG  adjust warnings (use '-w list' to list warnings)

  --jobserver-pool
           setup a GNU jobserver pool of job slots matching the
           current parallel job configuration. Ignored if -j1 is
           specified explicitly, or if an existing pool is detected

remote execution (REAPI v2; only for build.ninja files with rbe_version = 1):
  --remote_executor=URL      grpc://host:port or grpcs://host:port
  --remote_cache=URL         remote cache alone: actions remote execution would run
                             are looked up there, and run locally on a miss
  --[no]remote_upload_local_results  with --remote_cache, store the results of
                             actions run locally [default=on]
  --remote_instance_name=S   REAPI instance name
  --remote_header=K=V        extra gRPC header (repeatable)
  --remote_jobs=N            remote actions in flight [default=50]
  --exec_root=DIR            fixed exec root for remote actions (default: per action)
  --remote_mnemonics=A,B     mnemonic suffixes allowed to run remotely [default=Compile]
  --[no]remote_local_fallback  run locally when remote execution fails [default=on]
  --remote_strict            actions sent remotely run there or fail: no local runs
                             for actions that can't be sent or remote failures
  --remote_max_fallbacks=N   after N local runs on remote failures, fail instead
  --remote_max_fallback_ratio=R  fail instead once more than R (0 to 1) of the
                             remote actions ran locally on remote failures
                             (checked from 20 actions on)
  --remote_racing            also run actions sent remotely locally, when a local
                             slot is free; the first to finish wins
  --remote_racing_delay=MS   wait this long before a local run starts [default=250]
  --remote_verbose           log remote decisions and cache hits
  --disk_cache=DIR           local action cache and CAS (also without --remote_executor)
  --remote_download_outputs=all|minimal
                             outputs of remote actions to download: all, or only
                             deliverables, named targets and local inputs [default=all]
  --remote_download_minimal  same as --remote_download_outputs=minimal
  --execution_log_file=FILE  log every remote action (key, command, inputs) as JSON lines
  --remote_include_scanner=conservative|preprocessor|clang-scan-deps|compiler
                             how compiles' inputs are found: following every #include;
                             evaluating the preprocessor exactly (GCC and Clang commands);
                             the clang-scan-deps next to the compiler; or the compiler
                             itself (-M, /showIncludes). The conservative scanner when
                             the chosen one can't tell [default=conservative]
  --disk_cache_max_size=SIZE evict least recently used entries above SIZE [default=10G]
  --[no]daemon               keep scanned headers, compiler profiles, digests and
                             toolchain trees between builds in a daemon watching their
                             files (-t daemon) [default=off; on with NINJA_RS_DAEMON=1]

TLS (grpcs:// endpoints: the remote service and the Build Event Service):
  --tls_certificate=FILE     CA certificate (PEM) trusted to sign server certificates,
                             besides the public roots
  --tls_client_certificate=FILE  client certificate (PEM), for mTLS
  --tls_client_key=FILE      the client certificate's private key (PEM)
  --tls_authority_override=NAME  check server certificates against NAME, not the host

build events (Bazel's Build Event Protocol):
  --bes_backend=URL          grpc://host:port or grpcs://host:port of a Build Event Service
  --bes_instance_name=S      BES project / instance name
  --bes_header=K=V           extra gRPC header (repeatable)
  --bes_results_url=URL      printed with the invocation id appended
  --bes_keywords=A,B         notification keywords
  --[no]bes_lifecycle_events send lifecycle events [default=on]
  --bes_timeout=SECONDS      wait at most this long for the upload at the end [default=60]
  --build_event_binary_file=PATH  also write the events to PATH (length-delimited)
  --build_event_publish_all_actions  report every action, not only failed ones
  --build_metadata=K=V       extra build metadata (repeatable)
  --invocation_id=UUID       [default: random]
  --build_request_id=UUID    groups invocations [default: random]
";

pub fn usage() -> String {
    USAGE.replace("{version}", ninja_core::parser::NINJA_VERSION).replace("{jobs}", &guess_parallelism().to_string())
}

fn parse_int(flag: &str, v: &str) -> Result<i64, String> {
    v.parse::<i64>().map_err(|_| format!("invalid {flag} parameter"))
}

pub fn parse_args(args: &[String]) -> Result<ParseOutcome, String> {
    let mut o = Options {
        working_dir: None,
        input_file: "build.ninja".into(),
        parallelism: guess_parallelism(),
        explicit_parallelism: false,
        jobserver_pool: false,
        failures_allowed: 1,
        dry_run: false,
        verbosity: Verbosity::Normal,
        explain: false,
        keep_depfile: false,
        keep_rsp: false,
        stats: false,
        no_stat_cache: false,
        phonycycle_err: false,
        status_format: None,
        tool: None,
        tool_args: Vec::new(),
        targets: Vec::new(),
        tls: TlsFlags::default(),
        remote: RemoteFlags {
            jobs: 50,
            mnemonic_suffixes: vec!["Compile".into()],
            local_fallback: true,
            upload_local_results: true,
            racing_delay_ms: DEFAULT_RACING_DELAY_MS,
            disk_cache_max_size: DEFAULT_DISK_CACHE_MAX_SIZE,
            daemon: std::env::var("NINJA_RS_DAEMON").is_ok_and(|v| v == "1"),
            ..Default::default()
        },
        bes: BesFlags::default(),
        argv: std::iter::once("ninja-rs".to_string()).chain(args.iter().cloned()).collect(),
        given: Vec::new(),
    };
    let key_value = |flag: &str, v: &str| -> Result<(String, String), String> {
        v.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("--{flag} expects KEY=VALUE, got '{v}'"))
    };
    let mut i = 0;
    let mut positional = Vec::new();
    while i < args.len() {
        let a = &args[i];
        let first = i;
        i += 1;
        if a == "--" {
            positional.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let value = |i: &mut usize| -> Result<String, String> {
                if let Some(v) = &inline {
                    return Ok(v.clone());
                }
                let v = args.get(*i).cloned().ok_or_else(|| format!("option '--{name}' requires an argument"))?;
                *i += 1;
                Ok(v)
            };
            match name {
                "version" => return Ok(ParseOutcome::Exit(format!("{}\n", ninja_core::parser::NINJA_VERSION), 0)),
                "verbose" => o.verbosity = Verbosity::Verbose,
                "quiet" => o.verbosity = Verbosity::NoStatusUpdate,
                "status" => o.status_format = Some(value(&mut i)?),
                "jobserver-pool" => o.jobserver_pool = true,
                "help" => return Ok(ParseOutcome::Exit(usage(), 1)),
                "tls_certificate" | "tls_client_certificate" | "tls_client_key" => {
                    // Relative to where ninja-rs was started, not to -C.
                    let v = value(&mut i)?;
                    let abs = std::path::absolute(&v).map_err(|e| format!("--{name}={v}: {e}"))?.to_string_lossy().into_owned();
                    match name {
                        "tls_certificate" => o.tls.certificate = Some(abs),
                        "tls_client_certificate" => o.tls.client_certificate = Some(abs),
                        _ => o.tls.client_key = Some(abs),
                    }
                }
                "tls_authority_override" => o.tls.authority_override = Some(value(&mut i)?),
                "remote_executor" => o.remote.executor = Some(value(&mut i)?),
                "remote_cache" => o.remote.cache = Some(value(&mut i)?),
                "remote_upload_local_results" => o.remote.upload_local_results = true,
                "noremote_upload_local_results" => o.remote.upload_local_results = false,
                "remote_instance_name" => o.remote.instance_name = value(&mut i)?,
                "remote_header" => {
                    let v = value(&mut i)?;
                    let (k, val) = v.split_once('=').ok_or_else(|| format!("--remote_header expects KEY=VALUE, got '{v}'"))?;
                    o.remote.headers.push((k.to_string(), val.to_string()));
                }
                "remote_jobs" => {
                    let v = value(&mut i)?;
                    let n = parse_int("--remote_jobs", &v)?;
                    if n <= 0 {
                        return Err("invalid --remote_jobs parameter".into());
                    }
                    o.remote.jobs = n as usize;
                }
                "exec_root" => o.remote.exec_root = Some(value(&mut i)?),
                "remote_mnemonics" => o.remote.mnemonic_suffixes = value(&mut i)?.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
                "remote_local_fallback" => o.remote.local_fallback = true,
                "noremote_local_fallback" => o.remote.local_fallback = false,
                "remote_strict" => o.remote.strict = true,
                "remote_racing" => o.remote.racing = true,
                "remote_racing_delay" => {
                    let v = value(&mut i)?;
                    let n = parse_int("--remote_racing_delay", &v)?;
                    if n < 0 {
                        return Err("invalid --remote_racing_delay parameter".into());
                    }
                    o.remote.racing_delay_ms = n as u64;
                }
                "remote_max_fallbacks" => {
                    let v = value(&mut i)?;
                    let n = parse_int("--remote_max_fallbacks", &v)?;
                    if n < 0 {
                        return Err("invalid --remote_max_fallbacks parameter".into());
                    }
                    o.remote.max_fallbacks = Some(n as usize);
                }
                "remote_max_fallback_ratio" => {
                    let v = value(&mut i)?;
                    let r: f64 = v.parse().ok().filter(|r| (0.0..=1.0).contains(r)).ok_or("invalid --remote_max_fallback_ratio parameter (0 to 1)")?;
                    o.remote.max_fallback_ratio = Some(r);
                }
                "remote_verbose" => o.remote.verbose = true,
                "remote_download_outputs" => {
                    o.remote.download_minimal = match value(&mut i)?.as_str() {
                        "all" => false,
                        "minimal" => true,
                        v => return Err(format!("invalid --remote_download_outputs '{v}' (all or minimal)")),
                    }
                }
                "remote_download_minimal" => o.remote.download_minimal = true,
                "remote_download_all" => o.remote.download_minimal = false,
                "remote_include_scanner" => o.remote.include_scanner = value(&mut i)?.parse()?,
                "daemon" => o.remote.daemon = true,
                "nodaemon" => o.remote.daemon = false,
                "execution_log_file" => {
                    let v = value(&mut i)?;
                    let abs = std::path::absolute(&v).map_err(|e| format!("--execution_log_file={v}: {e}"))?;
                    o.remote.execution_log = Some(abs.to_string_lossy().into_owned());
                }
                "disk_cache" => {
                    // Relative to where ninja-rs was started, not to -C.
                    let v = value(&mut i)?;
                    let abs = std::path::absolute(&v).map_err(|e| format!("--disk_cache={v}: {e}"))?;
                    o.remote.disk_cache = Some(abs.to_string_lossy().into_owned());
                }
                "disk_cache_max_size" => {
                    let v = value(&mut i)?;
                    o.remote.disk_cache_max_size = ninja_remote::disk_cache::parse_size(&v).ok_or_else(|| format!("invalid --disk_cache_max_size '{v}'"))?;
                }
                "bes_backend" => o.bes.backend = Some(value(&mut i)?),
                "bes_instance_name" => o.bes.instance_name = value(&mut i)?,
                "bes_header" => o.bes.headers.push(key_value(name, &value(&mut i)?)?),
                "bes_results_url" => o.bes.results_url = Some(value(&mut i)?),
                "bes_keywords" => o.bes.keywords.extend(value(&mut i)?.split(',').filter(|s| !s.is_empty()).map(str::to_string)),
                "bes_lifecycle_events" => o.bes.lifecycle_events = true,
                "nobes_lifecycle_events" => o.bes.lifecycle_events = false,
                "bes_timeout" => {
                    let v = value(&mut i)?;
                    o.bes.timeout_secs = parse_int("--bes_timeout", &v)?.max(0) as u64;
                }
                "build_event_binary_file" => o.bes.binary_file = Some(value(&mut i)?),
                "build_event_publish_all_actions" => o.bes.publish_all_actions = true,
                "build_metadata" => o.bes.build_metadata.push(key_value(name, &value(&mut i)?)?),
                "invocation_id" => o.bes.invocation_id = Some(value(&mut i)?),
                "build_request_id" => o.bes.build_request_id = Some(value(&mut i)?),
                _ => return Err(format!("unrecognized option '--{name}'")),
            }
            let value = match &inline {
                Some(v) => v.clone(),
                None if i > first + 1 => args[i - 1].clone(),
                None => String::new(),
            };
            o.given.push(GivenOption { combined_form: args[first..i].join(" "), name: name.to_string(), value });
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            // Short options, getopt style: "-j8", "-j 8", "-nv".
            let chars: Vec<char> = a[1..].chars().collect();
            let mut ci = 0;
            while ci < chars.len() {
                let c = chars[ci];
                ci += 1;
                let takes_arg = matches!(c, 'd' | 'f' | 'j' | 'k' | 'l' | 't' | 'w' | 'C');
                let before = i;
                let arg = if takes_arg {
                    if ci < chars.len() {
                        let v: String = chars[ci..].iter().collect();
                        ci = chars.len();
                        Some(v)
                    } else {
                        let v = args.get(i).cloned().ok_or_else(|| format!("option requires an argument -- '{c}'"))?;
                        i += 1;
                        Some(v)
                    }
                } else {
                    None
                };
                o.given.push(GivenOption {
                    combined_form: match &arg {
                        Some(v) if i > before => format!("-{c} {v}"),
                        Some(v) => format!("-{c}{v}"),
                        None => format!("-{c}"),
                    },
                    name: c.to_string(),
                    value: arg.clone().unwrap_or_default(),
                });
                match c {
                    'd' => match arg.as_deref().unwrap() {
                        "explain" => o.explain = true,
                        "keepdepfile" => o.keep_depfile = true,
                        "keeprsp" => o.keep_rsp = true,
                        "stats" => o.stats = true,
                        "nostatcache" => o.no_stat_cache = true,
                        "list" => {
                            return Ok(ParseOutcome::Exit(
                                "debugging modes:\n  stats        print operation counts/timing info\n  explain      explain what caused a command to execute\n  keepdepfile  don't delete depfiles after they're read by ninja\n  keeprsp      don't delete @response files on success\n  nostatcache  don't batch stat() calls per directory and cache them\nmultiple modes can be enabled via -d FOO -d BAR\n".into(),
                                1,
                            ));
                        }
                        other => return Err(format!("unknown debug setting '{other}'")),
                    },
                    'f' => o.input_file = arg.unwrap(),
                    'j' => {
                        let n = parse_int("-j", arg.as_deref().unwrap())?;
                        if n < 0 {
                            return Err("invalid -j parameter".into());
                        }
                        // We want to run N jobs in parallel. For N = 0, INT_MAX
                        // is close enough to infinite for most sane builds.
                        o.parallelism = if n > 0 { n as usize } else { i32::MAX as usize };
                        o.explicit_parallelism = true;
                    }
                    'k' => {
                        let n = parse_int("-k", arg.as_deref().unwrap())?;
                        // We want to go until N jobs fail, which means we should
                        // allow N failures and then stop. For N <= 0, INT_MAX is
                        // close enough to infinite for most sane builds.
                        o.failures_allowed = if n > 0 { n as usize } else { i32::MAX as usize };
                    }
                    'l' => {
                        arg.as_deref().unwrap().parse::<f64>().map_err(|_| "-l parameter not numeric: did you mean -l 0.0?".to_string())?;
                    }
                    'n' => o.dry_run = true,
                    't' => {
                        o.tool = arg;
                        // -t terminates toplevel options; the rest goes to the tool.
                        o.tool_args = args[i..].to_vec();
                        i = args.len();
                    }
                    'v' => o.verbosity = Verbosity::Verbose,
                    'q' => o.verbosity = Verbosity::Quiet,
                    'w' => match arg.as_deref().unwrap() {
                        "phonycycle=err" => o.phonycycle_err = true,
                        "phonycycle=warn" => o.phonycycle_err = false,
                        "dupbuild=err" | "dupbuild=warn" => {}
                        "list" => {
                            return Ok(ParseOutcome::Exit("warning flags:\n  phonycycle={err,warn}  phony build statement references itself\n".into(), 1));
                        }
                        other => return Err(format!("unknown warning flag '{other}'")),
                    },
                    'C' => o.working_dir = arg,
                    'h' => return Ok(ParseOutcome::Exit(usage(), 1)),
                    other => return Err(format!("invalid option -- '{other}'\n{}", usage())),
                }
                if o.tool.is_some() && i == args.len() && c == 't' {
                    break;
                }
            }
            continue;
        }
        positional.push(a.clone());
    }
    o.targets = positional;
    if o.tls.client_certificate.is_some() != o.tls.client_key.is_some() {
        return Err("--tls_client_certificate and --tls_client_key go together".into());
    }
    if o.remote.racing && o.remote.strict {
        return Err("--remote_racing runs actions locally, which --remote_strict forbids".into());
    }
    if let (Some(e), Some(c)) = (&o.remote.executor, &o.remote.cache)
        && e != c
    {
        return Err("--remote_cache and --remote_executor name different services: the executor's cache is used".into());
    }
    Ok(ParseOutcome::Run(Box::new(o)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Options {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_args(&v).unwrap() {
            ParseOutcome::Run(o) => *o,
            ParseOutcome::Exit(m, _) => panic!("exit: {m}"),
        }
    }

    #[test]
    fn ninja_flags() {
        let o = p(&["-C", "build", "-j8", "-k", "0", "-nv", "-f", "x.ninja", "all", "foo"]);
        assert_eq!(o.working_dir.as_deref(), Some("build"));
        assert_eq!(o.parallelism, 8);
        assert_eq!(o.failures_allowed, i32::MAX as usize);
        assert!(o.dry_run);
        assert_eq!(o.verbosity, Verbosity::Verbose);
        assert_eq!(o.input_file, "x.ninja");
        assert_eq!(o.targets, vec!["all", "foo"]);
    }

    #[test]
    fn tool_terminates_options() {
        let o = p(&["-Cbuild", "-t", "clean", "-g", "foo"]);
        assert_eq!(o.tool.as_deref(), Some("clean"));
        assert_eq!(o.tool_args, vec!["-g", "foo"]);
        assert!(o.targets.is_empty());
    }

    #[test]
    fn remote_flags() {
        let o = p(&[
            "--remote_executor=grpc://localhost:8980",
            "--remote_instance_name",
            "main",
            "--remote_header=x-a=b=c",
            "--remote_header=k=v",
            "--remote_jobs=12",
            "--noremote_local_fallback",
            "--remote_mnemonics=Compile,Archive",
        ]);
        assert_eq!(o.remote.executor.as_deref(), Some("grpc://localhost:8980"));
        assert_eq!(o.remote.instance_name, "main");
        assert_eq!(o.remote.headers, vec![("x-a".to_string(), "b=c".to_string()), ("k".into(), "v".into())]);
        assert_eq!(o.remote.jobs, 12);
        assert!(!o.remote.local_fallback);
        assert_eq!(o.remote.mnemonic_suffixes, vec!["Compile", "Archive"]);
    }

    #[test]
    fn remote_cache_flags() {
        let o = p(&["--remote_cache=grpcs://cache:443"]);
        assert!(o.remote.cache_only() && o.remote.upload_local_results);
        assert_eq!(o.remote.endpoint().map(String::as_str), Some("grpcs://cache:443"));
        let o = p(&["--remote_cache=grpc://c:1", "--noremote_upload_local_results"]);
        assert!(!o.remote.upload_local_results);
        // The executor's own cache.
        let o = p(&["--remote_executor=grpc://e:1", "--remote_cache=grpc://e:1"]);
        assert!(!o.remote.cache_only());
        let v: Vec<String> = ["--remote_executor=grpc://e:1", "--remote_cache=grpc://c:1"].iter().map(|s| s.to_string()).collect();
        assert!(parse_args(&v).is_err());
    }

    #[test]
    fn fallback_flags() {
        let o = p(&["--remote_strict", "--remote_max_fallbacks=5", "--remote_max_fallback_ratio", "0.25"]);
        assert!(o.remote.strict);
        assert_eq!(o.remote.max_fallbacks, Some(5));
        assert_eq!(o.remote.max_fallback_ratio, Some(0.25));
        for bad in ["--remote_max_fallback_ratio=1.5", "--remote_max_fallback_ratio=x", "--remote_max_fallbacks=-1"] {
            assert!(parse_args(&[bad.to_string()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn racing_flags() {
        let o = p(&["--remote_racing"]);
        assert!(o.remote.racing);
        assert_eq!(o.remote.racing_delay_ms, DEFAULT_RACING_DELAY_MS);
        assert_eq!(p(&["--remote_racing_delay=0"]).remote.racing_delay_ms, 0);
        assert!(parse_args(&["--remote_racing".to_string(), "--remote_strict".to_string()]).is_err());
        assert!(parse_args(&["--tls_client_certificate=c.pem".to_string()]).is_err());
        let tls = TlsFlags { client_certificate: Some("/c.pem".into()), ..Default::default() };
        assert!(tls_hint("transport error: broken pipe", "grpcs://h:1", &tls).contains("sent the client certificate /c.pem"));
        assert!(tls_hint("invalid peer certificate: UnknownIssuer", "h:1", &TlsFlags::default()).contains("--tls_certificate adds a CA"));
        assert!(tls_hint("transport error: broken pipe", "grpc://h:1", &tls).is_empty());
        assert!(tls_hint("not found", "grpcs://h:1", &tls).is_empty());
    }

    #[test]
    fn version() {
        let v = vec!["--version".to_string()];
        assert!(matches!(parse_args(&v).unwrap(), ParseOutcome::Exit(_, 0)));
    }
}
