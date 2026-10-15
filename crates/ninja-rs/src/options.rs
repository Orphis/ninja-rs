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
    /// `.ninjars` files (see `rc`).
    pub rc: RcOptions,
    /// The whole command line (for build events).
    pub argv: Vec<String>,
    /// The options given, in order (for build events): from `.ninjars`
    /// files and the command line.
    pub given: Vec<GivenOption>,
    /// About the options, printed before the invocation runs.
    pub warnings: Vec<String>,
}

/// An option as given on the command line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GivenOption {
    /// As written: `--remote_executor=grpcs://...`, `-j 8`.
    pub combined_form: String,
    /// Without dashes: `remote_executor`, `j`.
    pub name: String,
    pub value: String,
    /// Where it is in the arguments parsed.
    pub index: usize,
    /// Typed on the command line, not from a `.ninjars` file or the
    /// invocation running this one.
    pub explicit: bool,
    /// Left out for an option it conflicts with (see `validate`): the
    /// invocations this one runs don't get it, so that they take the
    /// options in effect here, without the conflict.
    pub left_out: bool,
}

/// `.ninjars` files and their configs (see `rc`). Which files are read is
/// decided from the command line alone (`rc::prescan`); the options are
/// kept here for build events and nested invocations.
#[derive(Clone, Debug, PartialEq)]
pub struct RcOptions {
    /// `--ninjars=FILE`: read after the others (repeatable).
    pub files: Vec<String>,
    /// `--[no]system_rc`, `--[no]workspace_rc`, `--[no]home_rc`.
    pub system: bool,
    pub workspace: bool,
    pub home: bool,
    /// `--ignore_all_rc_files`.
    pub ignore_all: bool,
    /// `--config=NAME`, as expanded.
    pub configs: Vec<String>,
    /// `--announce_rc`: print the options each file gave.
    pub announce: bool,
    /// `--enable_platform_specific_config`: the config named after the
    /// host's OS (`linux`, `macos`, `windows`...) applies when defined.
    pub platform_config: bool,
    /// The files read (not their imports) and the workspace directory, for
    /// nested invocations expanding their own `--config`.
    pub read: Vec<String>,
    pub workspace_dir: Option<String>,
}

impl Default for RcOptions {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            system: true,
            workspace: true,
            home: true,
            ignore_all: false,
            configs: Vec::new(),
            announce: false,
            platform_config: false,
            read: Vec::new(),
            workspace_dir: None,
        }
    }
}

/// The options that only go on the command line: they decide which
/// `.ninjars` files are read.
pub const RC_SELECTION: &[&str] = &["ninjars", "system_rc", "workspace_rc", "home_rc", "ignore_all_rc_files"];

/// Options whose values may be credentials (`Authorization=Bearer ...`):
/// shown with their key only, wherever options are reported.
pub const SECRET_OPTIONS: &[&str] = &["remote_header", "bes_header"];

const REDACTED: &str = "<redacted>";

/// `KEY=VALUE` with the value hidden.
fn redact_header(kv: &str) -> String {
    match kv.split_once('=') {
        Some((k, _)) => format!("{k}={REDACTED}"),
        None => REDACTED.to_string(),
    }
}

/// Arguments as they may be reported (build events, `--announce_rc`): the
/// values of `SECRET_OPTIONS` hidden, in `--name=K=V` and `--name K=V`.
pub fn redact_args(args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        let after_name = out.last().and_then(|p| p.strip_prefix("--")).is_some_and(|n| SECRET_OPTIONS.contains(&n));
        let inline = a.strip_prefix("--").and_then(|r| r.split_once('=')).filter(|(n, _)| SECRET_OPTIONS.contains(n));
        out.push(match inline {
            Some((name, kv)) => format!("--{name}={}", redact_header(kv)),
            None if after_name => redact_header(a),
            None => a.clone(),
        });
    }
    out
}

impl GivenOption {
    /// As it may be reported (see `redact_args`).
    pub fn redacted(&self) -> GivenOption {
        if !SECRET_OPTIONS.contains(&self.name.as_str()) {
            return self.clone();
        }
        let value = redact_header(&self.value);
        GivenOption { combined_form: format!("--{}={value}", self.name), value, ..self.clone() }
    }
}

/// A boolean option's value, as Bazel takes them: `--name=true`, `yes`,
/// `1`, or `false`, `no`, `0`.
pub fn parse_bool(name: &str, v: &str) -> Result<bool, String> {
    match v {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(format!("invalid --{name} value '{v}' (true or false)")),
    }
}

/// A duration, as Bazel writes them: a number and a unit (`d`, `h`, `m`,
/// `s`, `ms`), or a number alone, in `unit`.
pub fn parse_duration(name: &str, v: &str, unit: std::time::Duration) -> Result<std::time::Duration, String> {
    use std::time::Duration;
    let bad = || format!("invalid --{name} value '{v}' (a duration: 30s, 5m, 500ms...)");
    let digits = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    let n: u64 = v[..digits].parse().map_err(|_| bad())?;
    Ok(match &v[digits..] {
        "" => unit * u32::try_from(n).map_err(|_| bad())?,
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.checked_mul(60).ok_or_else(bad)?),
        "h" => Duration::from_secs(n.checked_mul(3600).ok_or_else(bad)?),
        "d" => Duration::from_secs(n.checked_mul(86400).ok_or_else(bad)?),
        _ => return Err(bad()),
    })
}

/// An optional value: empty unsets it, as in Bazel (`--disk_cache=` turns
/// off a cache a settings file sets).
fn unless_empty(v: String) -> Option<String> {
    (!v.is_empty()).then_some(v)
}

/// The home directory: `HOME`, or `USERPROFILE` on Windows.
pub fn home_dir() -> Option<std::path::PathBuf> {
    let var = if cfg!(windows) { std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) } else { std::env::var_os("HOME") };
    var.filter(|v| !v.is_empty()).map(std::path::PathBuf::from)
}

/// An optional path option's value (see `path_value`): empty unsets it.
fn optional_path(name: &str, v: &str) -> Result<Option<String>, String> {
    if v.is_empty() { Ok(None) } else { path_value(name, v).map(Some) }
}

/// A path option's value: `~` expanded (as Bazel does), then absolute,
/// relative to where ninja-rs was started (not -C).
fn path_value(name: &str, v: &str) -> Result<String, String> {
    let expanded = match v.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => match home_dir() {
            Some(h) => format!("{}{rest}", h.to_string_lossy()),
            None => v.to_string(),
        },
        _ => v.to_string(),
    };
    std::path::absolute(&expanded).map(|p| p.to_string_lossy().into_owned()).map_err(|e| format!("--{name}={v}: {e}"))
}

/// The field of a boolean option (`--name`, `--noname`, `--name=false`).
fn bool_option<'a>(o: &'a mut Options, name: &str) -> Option<&'a mut bool> {
    Some(match name {
        "remote_upload_local_results" => &mut o.remote.upload_local_results,
        "remote_local_fallback" => &mut o.remote.local_fallback,
        "remote_strict" => &mut o.remote.strict,
        "remote_racing" => &mut o.remote.racing,
        "remote_verbose" => &mut o.remote.verbose,
        "remote_cache_compression" | "experimental_remote_cache_compression" => &mut o.remote.cache_compression,
        "daemon" => &mut o.remote.daemon,
        "bes_lifecycle_events" => &mut o.bes.lifecycle_events,
        "build_event_publish_all_actions" => &mut o.bes.publish_all_actions,
        "system_rc" => &mut o.rc.system,
        "workspace_rc" => &mut o.rc.workspace,
        "home_rc" => &mut o.rc.home,
        "ignore_all_rc_files" => &mut o.rc.ignore_all,
        "announce_rc" => &mut o.rc.announce,
        "enable_platform_specific_config" => &mut o.rc.platform_config,
        _ => return None,
    })
}

/// Whether `name` (without dashes) is a boolean option, or its `no` form.
pub fn is_bool_option(name: &str) -> bool {
    let mut o = blank();
    let base = name.strip_prefix("no").filter(|b| bool_option(&mut o, b).is_some()).unwrap_or(name);
    bool_option(&mut o, base).is_some()
}

/// How an option from a `.ninjars` file parses.
#[derive(Debug, PartialEq)]
pub enum RcOptionError {
    /// Not an option of ninja-rs (`common` lines skip these).
    Unknown(String),
    Invalid(String),
}

/// Checks one option of a `.ninjars` file, with its value when it is
/// separate (`--remote_instance_name main`).
pub fn check_rc_option(group: &[String]) -> Result<(), RcOptionError> {
    let only_command_line = || RcOptionError::Invalid(format!("'{}' only goes on the command line", group.join(" ")));
    match parse_unchecked(group) {
        Err(e) if e.starts_with("unrecognized option") || e.starts_with("invalid option") => {
            Err(RcOptionError::Unknown(e.lines().next().unwrap_or_default().to_string()))
        }
        Err(e) => Err(RcOptionError::Invalid(e)),
        Ok(ParseOutcome::Exit(..)) => Err(only_command_line()),
        Ok(ParseOutcome::Run(o)) => {
            if let Some(t) = o.targets.first() {
                Err(RcOptionError::Invalid(format!("'{t}' isn't an option")))
            } else if o.tool.is_some()
                || o.working_dir.is_some()
                || o.given.iter().any(|g| RC_SELECTION.contains(&g.name.strip_prefix("no").unwrap_or(&g.name)))
            {
                Err(only_command_line())
            } else {
                Ok(())
            }
        }
    }
}

/// TLS for `grpcs://` endpoints, with Bazel's flag names. Paths are
/// absolute (relative ones are resolved where ninja-rs started, not -C).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TlsFlags {
    /// `--tls_certificate`: the CA (PEM; possibly several) trusted to sign
    /// server certificates, instead of the web PKI roots (as Bazel's).
    pub certificate: Option<String>,
    /// `--tls_client_certificate` and `--tls_client_key`: the client's
    /// certificate and private key (PEM), for mTLS.
    pub client_certificate: Option<String>,
    pub client_key: Option<String>,
    /// `--tls_authority_override`: the name server certificates are checked
    /// against, instead of the endpoint's host.
    pub authority_override: Option<String>,
}

/// What `TlsFlags::load` read.
#[derive(Debug)]
pub struct LoadedTls {
    pub tls: ninja_remote::Tls,
    /// The files holding a certificate that expires soon, with the warning
    /// about it (see `certs::check`).
    pub expiring: Vec<(String, String)>,
}

impl TlsFlags {
    /// Reads the certificate files, which must hold PEM: certificates, and
    /// a private key for `--tls_client_key`. Anything else (a DER file)
    /// would leave the CAs trusted none, or the client without a key. The
    /// certificates' dates are checked now (see `certs::check`): before
    /// connecting, which a certificate refused may end with no more than a
    /// connection closed.
    pub fn load(&self) -> Result<LoadedTls, String> {
        self.load_at(std::time::SystemTime::now())
    }

    /// `load`, the certificates' dates checked at `now`.
    pub fn load_at(&self, now: std::time::SystemTime) -> Result<LoadedTls, String> {
        let mut expiring = Vec::new();
        // `kind` names the certificates' (`client`, `CA`); none for a key.
        let mut read = |flag: &str, p: &Option<String>, kind: Option<&str>| -> Result<Option<Vec<u8>>, String> {
            let Some(p) = p else { return Ok(None) };
            let data = std::fs::read(p).map_err(|e| format!("--{flag}={p}: {e}"))?;
            let found = match kind {
                Some(kind) => {
                    let certs = crate::certs::pem_certificates(&data).map_err(|e| format!("--{flag}={p}: {e}"))?;
                    if let Some(warning) = crate::certs::check(kind, &format!("--{flag}={p}"), &certs, now)? {
                        expiring.push((p.clone(), warning));
                    }
                    !certs.is_empty()
                }
                // An encrypted key (PKCS#8, or OpenSSL's PKCS#1 with a
                // `Proc-Type` header) rustls doesn't read.
                None if pem_labels(&data).any(|l| l == "ENCRYPTED PRIVATE KEY") || data.windows(22).any(|w| w == b"Proc-Type: 4,ENCRYPTED") => {
                    return Err(format!("--{flag}={p}: the private key is encrypted, which isn't supported (`openssl pkey -in {p} -out key.pem` decrypts it)"));
                }
                // The sections rustls reads: PKCS#8, PKCS#1 and SEC1 keys.
                None => pem_labels(&data).any(|l| ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"].contains(&l)),
            };
            if !found {
                let (what, convert) = if kind.is_none() { ("private key", "openssl pkey") } else { ("certificate", "openssl x509") };
                return Err(format!("--{flag}={p}: no PEM {what} in it (`{convert} -inform der` converts a DER one)"));
            }
            Ok(Some(data))
        };
        let tls = ninja_remote::Tls {
            ca: read("tls_certificate", &self.certificate, Some("CA"))?,
            client_cert: read("tls_client_certificate", &self.client_certificate, Some("client"))?,
            client_key: read("tls_client_key", &self.client_key, None)?,
            authority_override: self.authority_override.clone(),
        };
        Ok(LoadedTls { tls, expiring })
    }
}

/// Whether `endpoint` connects with TLS: `grpcs://`, `https://`, or no
/// scheme (as the REAPI client takes it).
pub fn uses_tls(endpoint: &str) -> bool {
    tls_host(endpoint).is_some()
}

/// What follows the scheme of an endpoint connecting with TLS (`host:port`).
fn tls_host(endpoint: &str) -> Option<&str> {
    endpoint.strip_prefix("grpcs://").or_else(|| endpoint.strip_prefix("https://")).or_else(|| (!endpoint.contains("://")).then_some(endpoint))
}

/// The labels of the PEM sections in `data` (`CERTIFICATE`, `PRIVATE KEY`).
fn pem_labels(data: &[u8]) -> impl Iterator<Item = &str> {
    data.split(|&b| b == b'\n').filter_map(|line| std::str::from_utf8(line).ok()?.trim().strip_prefix("-----BEGIN ")?.strip_suffix("-----"))
}

/// Explains a failed connection to a `grpcs://` endpoint (or one without a
/// scheme, which means TLS): whose certificate was refused, the client's
/// (by the server, whose alert says why) or the server's (by this client),
/// what was sent or checked, and what the error usually means. Empty for
/// other errors. A connection closed is only explained when the server
/// never answered anything (`first_contact`): with TLS 1.3, a server
/// refusing the client's certificate may just close it after the
/// handshake; later, it's a broken connection (made again already). An
/// alert comes first: one saying a certificate expired is about the
/// client's, not the server's.
pub fn tls_hint(error: &str, endpoint: &str, tls: &TlsFlags, first_contact: bool) -> String {
    let Some(rest) = tls_host(endpoint) else {
        return String::new();
    };
    // The words of the error, not those of the endpoint it names.
    let e = error.replace(endpoint, "").to_ascii_lowercase();
    let cert = match &tls.client_certificate {
        Some(c) => format!("the client certificate {c}"),
        None => "no client certificate (--tls_client_certificate)".to_string(),
    };
    // The alert the server sent, as rustls names them (`CertificateExpired`).
    let alert = e.split_once("alert: ").map(|(_, a)| a.split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or_default());
    let client_refused = |why: &str| format!("the server refused the client's certificate{why}: sent {cert}");
    let hint = if let Some(alert) = alert.filter(|a| a.contains("certificate") || ["unknownca", "accessdenied", "handshakefailure", "decrypterror"].contains(a))
    {
        match alert {
            "certificaterequired" | "nocertificate" if tls.client_certificate.is_none() => {
                "the server requires a client certificate: --tls_client_certificate and --tls_client_key give one".to_string()
            }
            "certificateexpired" => client_refused(" as expired, or not valid yet (is the clock right?)"),
            "unknownca" => client_refused(", whose issuer it doesn't trust"),
            "certificaterevoked" => client_refused(" as revoked"),
            // Also what a server sharing no TLS version or cipher suite with
            // the client says.
            "handshakefailure" => format!(
                "the server refused the TLS handshake: it shares no TLS version or cipher suite with the client, or refused its certificate (sent {cert})"
            ),
            _ => client_refused(""),
        }
    } else if e.contains("keymismatch") {
        format!("{cert} and its key (--tls_client_key) don't match")
    } else if e.contains("unknownissuer") || e.contains("unknown issuer") {
        let ca = match &tls.certificate {
            Some(c) => format!("{c} only"),
            None => "the public roots (--tls_certificate names a CA instead)".to_string(),
        };
        format!("the server's certificate isn't trusted: checked against {ca}")
    } else if e.contains("notvalidforname") || e.contains("not valid for name") {
        let name = tls.authority_override.clone().unwrap_or_else(|| {
            let host = rest.split('/').next().unwrap_or(rest);
            match host.strip_prefix('[') {
                Some(v6) => v6.split(']').next().unwrap_or(v6).to_string(),
                None => host.split(':').next().unwrap_or(host).to_string(),
            }
        });
        format!("the server's certificate isn't valid for {name} (--tls_authority_override checks it against another name)")
    } else if e.contains("notvalidyet") || e.contains("not valid yet") {
        "the server's certificate isn't valid yet (is the clock right?)".to_string()
    } else if e.contains("expired") {
        "the server's certificate has expired (is the clock right?)".to_string()
    } else if e.contains("invalidpurpose") || e.contains("extended key usage") {
        "the server's certificate isn't one for a server (its extended key usage)".to_string()
    } else if e.contains("invalid peer certificate") {
        "the server's certificate was refused".to_string()
    } else if first_contact
        && ["broken pipe", "connection closed", "connection reset", "unexpected eof", "forcibly closed", "connection was aborted", "connection aborted"]
            .iter()
            .any(|c| e.contains(c))
    {
        format!(
            "the server closed the connection before answering anything, as when it refuses the client's certificate (sent {cert}), or doesn't speak TLS (grpc:// connects without)"
        )
    } else {
        return String::new();
    };
    format!("\n  {hint}")
}

/// Explains a connection to a plaintext endpoint (`grpc://`) that broke
/// before anything answered (`rbe_client::fault::Unreachable`), which a
/// TLS server does too (Windows resets it before the client reads the
/// server's alert): empty for other errors.
pub fn plaintext_hint(error: &str, endpoint: &str) -> String {
    let plaintext = endpoint.starts_with("grpc://") || endpoint.starts_with("http://");
    if !plaintext || !error.contains("the connection broke before anything answered") {
        return String::new();
    }
    "\n  a server that speaks TLS closes plaintext connections so too: grpcs:// connects with TLS".to_string()
}

/// Build event reporting, with Bazel's flag names.
#[derive(Clone, Debug)]
pub struct BesFlags {
    /// `grpc://host:port` or `grpcs://host:port` of a Build Event Service.
    pub backend: Option<String>,
    /// BES project / instance name.
    pub instance_name: String,
    pub headers: Vec<(String, String)>,
    /// How long the service may take to answer a call, or to ack events
    /// waiting for it (`--bes_rpc_timeout`).
    pub rpc_timeout: std::time::Duration,
    /// Printed with the invocation id appended.
    pub results_url: Option<String>,
    /// Writes the events, length-delimited, to this file.
    pub binary_file: Option<String>,
    pub keywords: Vec<String>,
    pub lifecycle_events: bool,
    /// Report every action, not only failed ones.
    pub publish_all_actions: bool,
    /// How long to wait for the upload at the end of the build; zero
    /// waits as long as it takes (Bazel's `0`).
    pub timeout: std::time::Duration,
    pub build_metadata: Vec<(String, String)>,
    pub invocation_id: Option<String>,
    pub build_request_id: Option<String>,
    /// A nested invocation's events go to the outermost one (see `nested`).
    pub relay: Option<crate::nested::RelayAddress>,
}

impl Default for BesFlags {
    fn default() -> Self {
        Self {
            backend: None,
            instance_name: String::new(),
            headers: Vec::new(),
            rpc_timeout: std::time::Duration::from_secs(15),
            results_url: None,
            binary_file: None,
            keywords: Vec::new(),
            lifecycle_events: true,
            publish_all_actions: false,
            timeout: std::time::Duration::from_secs(60),
            build_metadata: Vec::new(),
            invocation_id: None,
            build_request_id: None,
            relay: None,
        }
    }
}

impl BesFlags {
    pub fn enabled(&self) -> bool {
        self.backend.is_some() || self.binary_file.is_some() || self.relay.is_some()
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
    /// Appended to (a nested invocation's, see `nested`), not replaced.
    pub execution_log_append: bool,
    /// `--remote_include_scanner`: how compiles' inputs are found.
    pub include_scanner: ninja_remote::action::IncludeScanner,
    /// `--[no]daemon` (`NINJA_RS_DAEMON=1`): use the daemon keeping caches
    /// between builds.
    pub daemon: bool,
    /// `--[no]remote_cache_compression`: blobs travel compressed with zstd
    /// when the remote supports it.
    pub cache_compression: bool,
    /// `--remote_retries`: calls failing for a transient reason are made
    /// again this many times.
    pub retries: u32,
    /// `--remote_timeout`: how long a call may take, or a transfer go
    /// without moving (executions aren't bounded).
    pub timeout: std::time::Duration,
    /// `--action_env=NAME=VALUE`, or `NAME` for the value ninja-rs has:
    /// variables every remote action gets, in order (the last wins).
    pub action_env: Vec<(String, Option<String>)>,
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

    /// `--action_env`, with the values of the variables named alone (those
    /// unset left out).
    pub fn action_env(&self) -> Vec<(String, String)> {
        self.action_env.iter().filter_map(|(name, value)| Some((name.clone(), value.clone().or_else(|| std::env::var(name).ok())?))).collect()
    }
}

/// `--remote_racing_delay` default: long enough for most remote cache hits,
/// which then don't start local commands.
pub const DEFAULT_RACING_DELAY_MS: u64 = 250;

/// `--disk_cache_max_size` default: 10 GiB.
pub const DEFAULT_DISK_CACHE_MAX_SIZE: u64 = 10 << 30;

pub enum ParseOutcome {
    Run(Box<Options>),
    /// Print this where it says and exit with the code (`--version`, `-h`,
    /// `-d list`).
    Exit(String, i32, Out),
}

/// Where an answer of the command line goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    Stdout,
    Stderr,
}

/// What the command line alone answers, before any settings file is read,
/// as ninja does before anything else: `--version` (CMake runs it to find
/// the generator: a settings file mustn't change it or make it fail),
/// `--help`, the listings (`-d list`, `-w list`, `-t list`) and an unknown
/// tool. None for an invocation to run, or an error the whole parse reports.
pub fn answer(args: &[String]) -> Option<(String, i32, Out)> {
    match parse_unchecked(args) {
        Ok(ParseOutcome::Exit(text, code, out)) => Some((text, code, out)),
        Ok(ParseOutcome::Run(o)) => match o.tool.as_deref() {
            Some("list") => Some((crate::tools::list_tools(), 0, Out::Stdout)),
            Some(t) if crate::tools::find_tool(t).is_none() => Some((format!("ninja: fatal: {}\n", crate::tools::unknown_tool(t)), 1, Out::Stderr)),
            _ => None,
        },
        Err(_) => None,
    }
}

/// What ninja prints for an error of the command line: getopt's (with the
/// usage), `Error`'s (an unknown debug or warning mode) or `Fatal`'s.
pub fn error_text(e: &str) -> String {
    if e.starts_with("unrecognized option") || e.contains("requires an argument") {
        format!("ninja: {e}\n{}", usage())
    } else if e.starts_with("invalid option") {
        format!("ninja: {e}")
    } else if e.starts_with("unknown debug setting") || e.starts_with("unknown warning flag") {
        format!("ninja: error: {e}\n")
    } else {
        format!("ninja: fatal: {e}\n")
    }
}

/// `what 'word'`, with the closest of `words` suggested, as ninja's
/// `SpellcheckString` finds it.
pub fn did_you_mean(what: &str, word: &str, words: &[&str]) -> String {
    const MAX_DISTANCE: usize = 3;
    let closest =
        words.iter().map(|w| (ninja_core::graph::edit_distance(w, word, MAX_DISTANCE), w)).filter(|(d, _)| *d <= MAX_DISTANCE).min_by_key(|(d, _)| *d);
    match closest {
        Some((_, w)) => format!("{what} '{word}', did you mean '{w}'?"),
        None => format!("{what} '{word}'"),
    }
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
  --version      print ninja version (\"{version}\"; with -v, ninja-rs' too)
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

settings files (.ninjars, in the format of Bazel's .bazelrc: /etc/ninjars, the first
.ninjars from the build directory up, ~/.ninjars, then --ninjars files):
  --config=NAME              add the options of the NAME config, where it is given
  --ninjars=FILE             also read FILE, after the others (repeatable; /dev/null
                             ends them)
  --[no]system_rc  --[no]workspace_rc  --[no]home_rc
                             read the system's, the workspace's, the user's file [default=on]
  --ignore_all_rc_files      read none
  --announce_rc              print the options each file gives
  --enable_platform_specific_config
                             add the config named after the host OS (linux, macos,
                             windows, freebsd, openbsd) when a file defines it

remote execution (REAPI v2; only for build.ninja files with rbe_version = 1):
  --remote_executor=URL      grpc://host:port or grpcs://host:port (an empty value, as
                             for every optional value, unsets it)
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
  --remote_racing_delay=DURATION  wait this long before a local run starts
                             (milliseconds without a unit) [default=250ms]
  --remote_verbose           log remote decisions and cache hits
  --remote_retries=N         make calls failing for a transient reason (UNAVAILABLE,
                             RESOURCE_EXHAUSTED...) again, up to N times [default=5]
  --remote_timeout=DURATION  how long a remote call may take, or a transfer go without
                             moving (30s, 2m; seconds without a unit). Executions
                             aren't bounded: a broken one is followed again
                             (WaitExecution) [default=60s]
  --disk_cache=DIR           local action cache and CAS (also without --remote_executor)
  --remote_download_outputs=all|minimal|toplevel
                             outputs of remote actions to download: all, or only
                             deliverables, named targets and local inputs (minimal,
                             and Bazel's toplevel) [default=all]
  --remote_download_minimal  same as --remote_download_outputs=minimal
  --remote_download_toplevel  same as --remote_download_outputs=toplevel
  --remote_download_all      same as --remote_download_outputs=all
  --execution_log_file=FILE  log every remote action (key, command, inputs) as JSON lines
  --remote_include_scanner=conservative|preprocessor|clang-scan-deps|compiler
                             how compiles' inputs are found: following every #include;
                             evaluating the preprocessor exactly (GCC and Clang commands);
                             the clang-scan-deps next to the compiler; or the compiler
                             itself (-M, /showIncludes). The conservative scanner when
                             the chosen one can't tell [default=conservative]
  --disk_cache_max_size=SIZE evict least recently used entries above SIZE [default=10G]
  --[no]remote_cache_compression  blobs travel compressed with zstd when the remote
                             supports it [default=on]
  --[no]daemon               keep scanned headers, compiler profiles, digests and
                             toolchain trees between builds in a daemon watching their
                             files (-t daemon) [default=off; on with NINJA_RS_DAEMON=1]
  --action_env=NAME[=VALUE]  set NAME in the environment of remote actions (and local
                             runs of them): to VALUE, or to its value here (repeatable)

TLS (grpcs:// endpoints: the remote service and the Build Event Service):
  --tls_certificate=FILE     CA certificate (PEM; possibly several) trusted to sign
                             server certificates, instead of the public roots
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
  --bes_timeout=DURATION     wait at most this long for the upload at the end (60s,
                             5m; seconds without a unit; 0 waits as long as it
                             takes) [default=60s]
  --bes_rpc_timeout=DURATION how long the service may take to answer a call, or to
                             ack the events sent [default=15s]
  --build_event_binary_file=PATH  also write the events to PATH (length-delimited)
  --build_event_publish_all_actions  report every action, not only failed ones
  --build_metadata=K=V       extra build metadata (repeatable)
  --invocation_id=UUID       [default: random]
  --build_request_id=UUID    groups invocations [default: random]
";

pub fn usage() -> String {
    USAGE.replace("{version}", ninja_core::parser::NINJA_VERSION).replace("{jobs}", &guess_parallelism().to_string())
}

/// `--version`: the ninja language level alone, as ninja prints it (tools
/// parse the whole output as a version); with `-v`, ninja-rs' own version
/// and commit on a second line.
fn version_text(verbose: bool) -> String {
    let level = ninja_core::parser::NINJA_VERSION;
    if verbose { format!("{level}\nninja-rs {}\n", crate::version()) } else { format!("{level}\n") }
}

fn parse_int(flag: &str, v: &str) -> Result<i64, String> {
    v.parse::<i64>().map_err(|_| format!("invalid {flag} parameter"))
}

/// Options with their defaults.
fn blank() -> Options {
    Options {
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
            cache_compression: true,
            retries: rbe_client::DEFAULT_RETRIES,
            timeout: rbe_client::DEFAULT_TIMEOUT,
            ..Default::default()
        },
        bes: BesFlags::default(),
        rc: RcOptions::default(),
        argv: Vec::new(),
        given: Vec::new(),
        warnings: Vec::new(),
    }
}

/// Parses arguments all typed on the command line.
pub fn parse_args(args: &[String]) -> Result<ParseOutcome, String> {
    parse_invocation(args, &[])
}

/// Parses an invocation's arguments: `explicit[i]` tells whether `args[i]`
/// was typed on the command line, not given by a settings file or the
/// invocation running this one (true past its end).
pub fn parse_invocation(args: &[String], explicit: &[bool]) -> Result<ParseOutcome, String> {
    let mut outcome = parse_unchecked(args)?;
    if let ParseOutcome::Run(o) = &mut outcome {
        for g in &mut o.given {
            g.explicit = explicit.get(g.index).copied().unwrap_or(true);
        }
        validate(o)?;
    }
    Ok(outcome)
}

/// Options that must agree with each other. One typed on the command line
/// wins over one it conflicts with from a settings file or the invocation
/// running this one, with a warning (Bazel takes either); two typed ones
/// are an error. The option left out isn't passed on (see
/// `GivenOption::left_out`): the invocations this one runs take the
/// options in effect, and neither fail on the conflict nor warn again.
fn validate(o: &mut Options) -> Result<(), String> {
    if o.tls.client_certificate.is_some() != o.tls.client_key.is_some() {
        return Err("--tls_client_certificate and --tls_client_key go together".into());
    }
    if o.remote.racing && o.remote.strict {
        // Whether the option's last occurrence was typed.
        let typed = |name: &str| o.given.iter().rev().find(|g| g.name == name).is_some_and(|g| g.explicit);
        let (dropped, kept, why) = match (typed("remote_racing"), typed("remote_strict")) {
            (true, false) => ("remote_strict", "remote_racing", "runs actions locally"),
            (false, true) => ("remote_racing", "remote_strict", "forbids local runs"),
            _ => return Err("--remote_racing runs actions locally, which --remote_strict forbids".into()),
        };
        *bool_option(o, dropped).expect("a boolean option") = false;
        leave_out(o, dropped);
        o.warnings.push(format!("--{dropped}, from a settings file or the invocation running this one, is left out: --{kept}, given here, {why}"));
    }
    if let (Some(e), Some(c)) = (&o.remote.executor, &o.remote.cache)
        && e != c
    {
        let warning = format!("--remote_cache={c} is left out: the cache of the executor (--remote_executor={e}) is used");
        o.warnings.push(warning);
        leave_out(o, "remote_cache");
    }
    Ok(())
}

/// Marks every occurrence of the option `name` (`--name`, `--noname`,
/// `--name=...`) left out.
fn leave_out(o: &mut Options, name: &str) {
    for g in &mut o.given {
        if g.name == name || g.name.strip_prefix("no") == Some(name) {
            g.left_out = true;
        }
    }
}

/// Parses the arguments, without checking that the options agree.
fn parse_unchecked(args: &[String]) -> Result<ParseOutcome, String> {
    let mut o = blank();
    o.argv = std::iter::once("ninja-rs".to_string()).chain(args.iter().cloned()).collect();
    let key_value = |flag: &str, v: &str| -> Result<(String, String), String> {
        v.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or_else(|| format!("--{flag} expects KEY=VALUE, got '{v}'"))
    };
    // A gRPC header: a valid name and an ASCII value.
    let header = |flag: &str, v: &str| -> Result<(String, String), String> {
        let (k, val) = key_value(flag, v)?;
        rbe_client::header(&k, &val).map_err(|e| format!("--{flag}: {e:#}"))?;
        Ok((k, val))
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
            // Booleans, as Bazel takes them: --name, --noname, --name=false.
            let negated = name.strip_prefix("no").filter(|base| bool_option(&mut o, base).is_some());
            if let Some(field) = bool_option(&mut o, negated.unwrap_or(name)) {
                *field = match (&inline, negated) {
                    (None, None) => true,
                    (None, Some(_)) => false,
                    (Some(_), Some(_)) => return Err(format!("option '--{name}' doesn't take a value")),
                    (Some(v), None) => parse_bool(name, v)?,
                };
                o.given.push(GivenOption {
                    combined_form: a.clone(),
                    name: name.to_string(),
                    value: inline.unwrap_or_default(),
                    index: first,
                    explicit: true,
                    left_out: false,
                });
                continue;
            }
            match name {
                "version" => return Ok(ParseOutcome::Exit(version_text(args.iter().any(|a| a == "-v" || a == "--verbose")), 0, Out::Stdout)),
                "verbose" => o.verbosity = Verbosity::Verbose,
                "quiet" => o.verbosity = Verbosity::NoStatusUpdate,
                "status" => o.status_format = Some(value(&mut i)?),
                "jobserver-pool" => o.jobserver_pool = true,
                "help" => return Ok(ParseOutcome::Exit(usage(), 1, Out::Stderr)),
                "tls_certificate" | "tls_client_certificate" | "tls_client_key" => {
                    let abs = optional_path(name, &value(&mut i)?)?;
                    match name {
                        "tls_certificate" => o.tls.certificate = abs,
                        "tls_client_certificate" => o.tls.client_certificate = abs,
                        _ => o.tls.client_key = abs,
                    }
                }
                "tls_authority_override" => o.tls.authority_override = unless_empty(value(&mut i)?),
                "remote_executor" => o.remote.executor = unless_empty(value(&mut i)?),
                "remote_cache" => o.remote.cache = unless_empty(value(&mut i)?),
                "remote_instance_name" => o.remote.instance_name = value(&mut i)?,
                "remote_header" => o.remote.headers.push(header(name, &value(&mut i)?)?),
                "action_env" => {
                    let v = value(&mut i)?;
                    let (name, val) = match v.split_once('=') {
                        Some((n, val)) => (n.to_string(), Some(val.to_string())),
                        None => (v, None),
                    };
                    if name.is_empty() {
                        return Err("--action_env expects NAME or NAME=VALUE".into());
                    }
                    o.remote.action_env.push((name, val));
                }
                "remote_jobs" => {
                    let v = value(&mut i)?;
                    let n = parse_int("--remote_jobs", &v)?;
                    if n <= 0 {
                        return Err("invalid --remote_jobs parameter".into());
                    }
                    o.remote.jobs = n as usize;
                }
                "exec_root" => o.remote.exec_root = optional_path(name, &value(&mut i)?)?,
                "remote_mnemonics" => o.remote.mnemonic_suffixes = value(&mut i)?.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
                "remote_racing_delay" => {
                    let d = parse_duration(name, &value(&mut i)?, std::time::Duration::from_millis(1))?;
                    o.remote.racing_delay_ms = d.as_millis().try_into().unwrap_or(u64::MAX);
                }
                "remote_max_fallbacks" => {
                    let v = value(&mut i)?;
                    let n = parse_int("--remote_max_fallbacks", &v)?;
                    if n < 0 {
                        return Err("invalid --remote_max_fallbacks parameter".into());
                    }
                    o.remote.max_fallbacks = Some(n as usize);
                }
                "remote_retries" => {
                    let v = value(&mut i)?;
                    o.remote.retries = v.parse().map_err(|_| format!("invalid --remote_retries value '{v}'"))?;
                }
                "remote_timeout" => {
                    o.remote.timeout = parse_duration(name, &value(&mut i)?, std::time::Duration::from_secs(1))?;
                    if o.remote.timeout.is_zero() {
                        return Err("invalid --remote_timeout value '0'".into());
                    }
                }
                "remote_max_fallback_ratio" => {
                    let v = value(&mut i)?;
                    let r: f64 = v.parse().ok().filter(|r| (0.0..=1.0).contains(r)).ok_or("invalid --remote_max_fallback_ratio parameter (0 to 1)")?;
                    o.remote.max_fallback_ratio = Some(r);
                }
                "remote_download_outputs" => {
                    o.remote.download_minimal = match value(&mut i)?.as_str() {
                        "all" => false,
                        // Bazel's toplevel: what the build asks for and what
                        // local actions read, which is ninja-rs's minimal.
                        "minimal" | "toplevel" => true,
                        v => return Err(format!("invalid --remote_download_outputs '{v}' (all, minimal or toplevel)")),
                    }
                }
                // Bazel's shorthands.
                "remote_download_minimal" | "remote_download_toplevel" => o.remote.download_minimal = true,
                "remote_download_all" => o.remote.download_minimal = false,
                "remote_include_scanner" => o.remote.include_scanner = value(&mut i)?.parse()?,
                "execution_log_file" => o.remote.execution_log = optional_path(name, &value(&mut i)?)?,
                "disk_cache" => o.remote.disk_cache = optional_path(name, &value(&mut i)?)?,
                "disk_cache_max_size" => {
                    let v = value(&mut i)?;
                    o.remote.disk_cache_max_size = ninja_remote::disk_cache::parse_size(&v).ok_or_else(|| format!("invalid --disk_cache_max_size '{v}'"))?;
                }
                "bes_backend" => o.bes.backend = unless_empty(value(&mut i)?),
                "bes_instance_name" => o.bes.instance_name = value(&mut i)?,
                "bes_header" => o.bes.headers.push(header(name, &value(&mut i)?)?),
                "bes_results_url" => o.bes.results_url = unless_empty(value(&mut i)?),
                "bes_keywords" => o.bes.keywords.extend(value(&mut i)?.split(',').filter(|s| !s.is_empty()).map(str::to_string)),
                "bes_timeout" => o.bes.timeout = parse_duration(name, &value(&mut i)?, std::time::Duration::from_secs(1))?,
                "bes_rpc_timeout" => {
                    o.bes.rpc_timeout = parse_duration(name, &value(&mut i)?, std::time::Duration::from_secs(1))?;
                    if o.bes.rpc_timeout.is_zero() {
                        return Err("invalid --bes_rpc_timeout value '0'".into());
                    }
                }
                "build_event_binary_file" => o.bes.binary_file = optional_path(name, &value(&mut i)?)?,
                "build_metadata" => o.bes.build_metadata.push(key_value(name, &value(&mut i)?)?),
                "invocation_id" => o.bes.invocation_id = unless_empty(value(&mut i)?),
                "build_request_id" => o.bes.build_request_id = unless_empty(value(&mut i)?),
                "config" => o.rc.configs.push(value(&mut i)?),
                "ninjars" => o.rc.files.push(path_value(name, &value(&mut i)?)?),
                _ => return Err(format!("unrecognized option '--{name}'")),
            }
            let value = match &inline {
                Some(v) => v.clone(),
                None if i > first + 1 => args[i - 1].clone(),
                None => String::new(),
            };
            o.given.push(GivenOption { combined_form: args[first..i].join(" "), name: name.to_string(), value, index: first, explicit: true, left_out: false });
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
                    index: first,
                    explicit: true,
                    left_out: false,
                });
                match c {
                    'd' => match arg.as_deref().unwrap() {
                        "explain" => o.explain = true,
                        "keepdepfile" => o.keep_depfile = true,
                        "keeprsp" => o.keep_rsp = true,
                        "stats" => o.stats = true,
                        "nostatcache" => o.no_stat_cache = true,
                        // On stdout, exiting with 1, as ninja does.
                        "list" => {
                            return Ok(ParseOutcome::Exit(
                                "debugging modes:\n  stats        print operation counts/timing info\n  explain      explain what caused a command to execute\n  keepdepfile  don't delete depfiles after they're read by ninja\n  keeprsp      don't delete @response files on success\n  nostatcache  don't batch stat() calls per directory and cache them\nmultiple modes can be enabled via -d FOO -d BAR\n".into(),
                                1,
                                Out::Stdout,
                            ));
                        }
                        other => return Err(did_you_mean("unknown debug setting", other, &["stats", "explain", "keepdepfile", "keeprsp", "nostatcache"])),
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
                        let n: i64 = arg.as_deref().unwrap().parse().map_err(|_| "-k parameter not numeric; did you mean -k 0?".to_string())?;
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
                        "dupbuild=err" | "dupbuild=warn" => o.warnings.push("deprecated warning 'dupbuild'".into()),
                        "depfilemulti=err" | "depfilemulti=warn" => o.warnings.push("deprecated warning 'depfilemulti'".into()),
                        "list" => {
                            let flags = "warning flags:\n  phonycycle={err,warn}  phony build statement references itself\n";
                            return Ok(ParseOutcome::Exit(flags.into(), 1, Out::Stdout));
                        }
                        other => return Err(did_you_mean("unknown warning flag", other, &["phonycycle=err", "phonycycle=warn"])),
                    },
                    'C' => o.working_dir = arg,
                    'h' => return Ok(ParseOutcome::Exit(usage(), 1, Out::Stderr)),
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
    Ok(ParseOutcome::Run(Box::new(o)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        let args: Vec<String> = ["ninja-rs", "--remote_header=authorization=Bearer t", "--bes_header", "x-key=k", "--remote_jobs=5", "--remote_header=bare"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            redact_args(&args),
            vec!["ninja-rs", "--remote_header=authorization=<redacted>", "--bes_header", "x-key=<redacted>", "--remote_jobs=5", "--remote_header=<redacted>"]
        );
        let o = p(&["--remote_header", "authorization=Bearer t"]);
        let g = o.given[0].redacted();
        assert_eq!((g.combined_form.as_str(), g.value.as_str()), ("--remote_header=authorization=<redacted>", "authorization=<redacted>"));
    }

    fn p(args: &[&str]) -> Options {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_args(&v).unwrap() {
            ParseOutcome::Run(o) => *o,
            ParseOutcome::Exit(m, ..) => panic!("exit: {m}"),
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
        assert!(!o.remote.cache_only() && o.warnings.is_empty());
        // Another one, which Bazel takes: the executor's is used.
        let o = p(&["--remote_executor=grpc://e:1", "--remote_cache=grpc://c:1"]);
        assert_eq!(o.remote.endpoint().map(String::as_str), Some("grpc://e:1"));
        assert!(o.warnings[0].contains("--remote_cache=grpc://c:1 is left out"), "{:?}", o.warnings);
    }

    /// Options typed on the command line win over those they conflict with
    /// from settings files and the invocation running this one (a nested
    /// build's own options), with a warning; two typed ones don't go.
    #[test]
    fn typed_options_win_conflicts() {
        let parse = |args: &[&str], explicit: &[bool]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            match parse_invocation(&args, explicit) {
                Ok(ParseOutcome::Run(o)) => Ok(*o),
                Ok(ParseOutcome::Exit(..)) => panic!("exit"),
                Err(e) => Err(e),
            }
        };
        let o = parse(&["--remote_racing", "--remote_strict"], &[false, true]).unwrap();
        assert!(o.remote.strict && !o.remote.racing);
        assert!(o.warnings[0].starts_with("--remote_racing, from a settings file or the invocation running this one, is left out"), "{:?}", o.warnings);
        let o = parse(&["--remote_strict", "--remote_racing"], &[false, true]).unwrap();
        assert!(o.remote.racing && !o.remote.strict);
        assert!(parse(&["--remote_racing", "--remote_strict"], &[true, true]).is_err());
        assert!(parse(&["--remote_racing", "--remote_strict"], &[false, false]).is_err());
        assert!(o.given.iter().all(|g| g.explicit == (g.name == "remote_racing")));
    }

    /// As ninja: errors of the command line, the listings (on stdout,
    /// exiting with 1), the deprecated warning flags.
    #[test]
    fn ninja_messages() {
        let outcome = |args: &[&str]| parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let error = |args: &[&str]| error_text(&outcome(args).err().expect("an error"));
        assert!(error(&["-j"]).starts_with("ninja: option requires an argument -- 'j'\nusage: ninja-rs"));
        assert!(error(&["--status"]).starts_with("ninja: option '--status' requires an argument\nusage: ninja-rs"));
        assert_eq!(error(&["-k", "x"]), "ninja: fatal: -k parameter not numeric; did you mean -k 0?\n");
        assert_eq!(error(&["-j", "x"]), "ninja: fatal: invalid -j parameter\n");
        assert_eq!(error(&["-d", "explian"]), "ninja: error: unknown debug setting 'explian', did you mean 'explain'?\n");
        assert_eq!(error(&["-d", "nothing-like-it"]), "ninja: error: unknown debug setting 'nothing-like-it'\n");
        assert_eq!(error(&["-w", "phonycycle=er"]), "ninja: error: unknown warning flag 'phonycycle=er', did you mean 'phonycycle=err'?\n");
        for listing in ["-d", "-w"] {
            let Ok(ParseOutcome::Exit(text, 1, Out::Stdout)) = outcome(&[listing, "list"]) else { panic!("{listing} list") };
            assert!(text.ends_with('\n'));
        }
        assert!(matches!(outcome(&["-h"]), Ok(ParseOutcome::Exit(_, 1, Out::Stderr))));
        assert_eq!(p(&["-w", "dupbuild=err"]).warnings, ["deprecated warning 'dupbuild'"]);
    }

    /// The command line alone answers `--version`, `--help`, the listings
    /// and unknown tools, before any settings file is read.
    #[test]
    fn command_line_answers() {
        let answer = |args: &[&str]| answer(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let version = format!("{}\n", ninja_core::parser::NINJA_VERSION);
        assert_eq!(answer(&["--version"]), Some((version, 0, Out::Stdout)));
        assert_eq!(answer(&["-t", "list"]).map(|(t, c, o)| (t.starts_with("ninja-rs subtools:\n"), c, o)), Some((true, 0, Out::Stdout)));
        assert_eq!(answer(&["-t", "cleen"]), Some(("ninja: fatal: unknown tool 'cleen', did you mean 'clean'?\n".to_string(), 1, Out::Stderr)));
        assert_eq!(answer(&["-t", "nothing-like-it"]).map(|a| a.0), Some("ninja: fatal: unknown tool 'nothing-like-it'\n".to_string()));
        assert!(matches!(answer(&["--help"]), Some((_, 1, Out::Stderr))));
        // To run, or the whole parse's error.
        assert_eq!(answer(&["-t", "targets", "all"]), None);
        assert_eq!(answer(&["all"]), None);
        assert_eq!(answer(&["--bogus", "--version"]), None);
    }

    /// `--action_env`: a value given, or the one ninja-rs has (none when
    /// the variable isn't set), in order.
    #[test]
    fn action_env() {
        let o = p(&["--action_env=A=x=y", "--action_env", "PATH", "--action_env=NINJA_RS_TEST_UNSET_VARIABLE", "--action_env=B="]);
        assert_eq!(o.remote.action_env[0], ("A".into(), Some("x=y".into())));
        let resolved = o.remote.action_env();
        assert_eq!(resolved.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["A", "PATH", "B"]);
        assert_eq!(resolved[1].1, std::env::var("PATH").unwrap());
        assert_eq!(resolved[2].1, "");
        assert!(parse_args(&["--action_env==x".to_string()]).is_err());
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
    }

    #[test]
    fn tls_hints() {
        let tls = TlsFlags { client_certificate: Some("/c.pem".into()), certificate: Some("/ca.pem".into()), ..Default::default() };
        let hint = |e: &str, endpoint: &str, first_contact: bool| tls_hint(e, endpoint, &tls, first_contact);
        // A server refusing the client's certificate.
        let refused = hint("transport error: received fatal alert: CertificateRequired", "grpcs://h:1", false);
        assert!(refused.contains("refused the client's certificate: sent the client certificate /c.pem"), "{refused}");
        assert!(hint("transport error: broken pipe", "grpcs://h:1", true).contains("sent the client certificate /c.pem"));
        // A connection that broke later isn't the certificate's fault.
        assert_eq!(hint("transport error: connection reset", "grpcs://h:1", false), "");
        assert!(tls_hint("invalid peer certificate: UnknownIssuer", "h:1", &TlsFlags::default(), true).contains("--tls_certificate names a CA instead"));
        assert!(hint("invalid peer certificate: UnknownIssuer", "h:1", true).contains("checked against /ca.pem only"));
        assert!(hint("invalid peer certificate: NotValidForName", "grpcs://[::1]:8980", true).contains("isn't valid for ::1"));
        let expired = hint("invalid peer certificate: Expired", "grpcs://h:1", true);
        assert!(expired.contains("has expired") && !expired.contains("trusted"), "{expired}");
        // The client's certificate expired, as the server says it.
        let expired = hint("transport error: connection error: received fatal alert: CertificateExpired", "grpcs://h:1", true);
        assert!(expired.contains("refused the client's certificate as expired"), "{expired}");
        // Whose certificate, and what of it, as rustls says it.
        for (error, said) in [
            (
                "invalid peer certificate: certificate expired: verification time 2 (UNIX), but certificate is not valid after 1 (1 seconds ago)",
                "the server's certificate has expired",
            ),
            (
                "invalid peer certificate: certificate not valid yet: verification time 1 (UNIX), but certificate is not valid before 2 (1 seconds in future)",
                "the server's certificate isn't valid yet",
            ),
            ("invalid peer certificate: NotValidYet", "the server's certificate isn't valid yet"),
            (
                "invalid peer certificate: certificate not valid for name \"h\"; certificate is only valid for DnsName(\"x\")",
                "the server's certificate isn't valid for h",
            ),
            (
                "invalid peer certificate: certificate does not allow extended key usage for server authentication, allows client authentication",
                "isn't one for a server",
            ),
            ("invalid peer certificate: BadSignature", "the server's certificate was refused"),
            ("received fatal alert: UnknownCA", "refused the client's certificate, whose issuer it doesn't trust: sent the client certificate /c.pem"),
            ("received fatal alert: CertificateRevoked", "refused the client's certificate as revoked"),
            ("received fatal alert: BadCertificate", "refused the client's certificate: sent"),
            (
                "received fatal alert: HandshakeFailure",
                "shares no TLS version or cipher suite with the client, or refused its certificate (sent the client certificate /c.pem)",
            ),
            ("keys may not be consistent: KeyMismatch", "the client certificate /c.pem and its key (--tls_client_key) don't match"),
        ] {
            let h = hint(&format!("transport error: {error}"), "grpcs://h:1", true);
            assert!(h.contains(said), "{error}: {h}");
        }
        let required = tls_hint("received fatal alert: CertificateRequired", "h:1", &TlsFlags::default(), true);
        assert!(required.contains("the server requires a client certificate"), "{required}");
        // The endpoint's words don't count.
        let named = "grpcs://expired-unknownissuer.example:1";
        assert_eq!(hint(&format!("connecting to {named}: transport error: connection refused"), named, true), "");
        // Which endpoints use the certificates at all.
        assert!(uses_tls("grpcs://h:1") && uses_tls("https://h:1") && uses_tls("h:1") && !uses_tls("grpc://h:1") && !uses_tls("http://h:1"));
        // Windows' words for a connection reset, or aborted, and hyper's.
        for e in [
            "An existing connection was forcibly closed by the remote host. (os error 10054)",
            "An established connection was aborted by the software in your host machine. (os error 10053)",
            "connection error: connection aborted",
        ] {
            assert!(hint(&format!("transport error: {e}"), "grpcs://h:1", true).contains("closed the connection before answering"), "{e}");
        }
        assert_eq!(hint("transport error: broken pipe", "grpc://h:1", true), "");
        assert_eq!(hint("not found", "grpcs://h:1", true), "");
        // A plaintext connection broken before anything answered may be to
        // a TLS server.
        let broke = "GetCapabilities: the connection broke before anything answered: UNKNOWN: transport error: connection aborted";
        assert!(plaintext_hint(broke, "grpc://h:1").contains("grpcs:// connects with TLS"));
        assert_eq!(plaintext_hint(broke, "grpcs://h:1"), "");
        assert_eq!(plaintext_hint("connecting to grpc://h:1: transport error: tcp connect error: refused", "grpc://h:1"), "");
    }

    #[test]
    fn tls_files_hold_pem() {
        let dir = test_dirs::TestDir::new("ninja-rs-tls-files");
        let file = |name: &str, data: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            Some(p.to_string_lossy().into_owned())
        };
        let now = std::time::SystemTime::now();
        let cert = fake_reapi::certs::made_up(now - DAY, now + 365 * DAY);
        let cert = cert.as_bytes();
        let key = b"-----BEGIN EC PRIVATE KEY-----\r\nMHcC\r\n-----END EC PRIVATE KEY-----\r\n";
        let tls = TlsFlags { certificate: file("ca.pem", cert), client_certificate: file("c.pem", cert), client_key: file("k.pem", key), ..Default::default() };
        let loaded = tls.load().unwrap();
        assert_eq!(loaded.tls.ca.as_deref(), Some(cert));
        assert_eq!(loaded.tls.client_key.as_deref(), Some(&key[..]));
        assert!(loaded.expiring.is_empty());
        // DER, or a key where a certificate goes: said so, not an empty
        // trust store.
        let der = file("ca.der", &[0x30, 0x82, 0x01, 0x0a, 0x02, 0x82]);
        let err = TlsFlags { certificate: der.clone(), ..Default::default() }.load().unwrap_err();
        assert!(err.contains("--tls_certificate=") && err.contains("no PEM certificate"), "{err}");
        let err = TlsFlags { client_certificate: file("key-as-cert.pem", key), client_key: tls.client_key.clone(), ..Default::default() }.load().unwrap_err();
        assert!(err.contains("--tls_client_certificate=") && err.contains("no PEM certificate"), "{err}");
        let err = TlsFlags { client_certificate: tls.client_certificate.clone(), client_key: der, ..Default::default() }.load().unwrap_err();
        assert!(err.contains("--tls_client_key=") && err.contains("no PEM private key"), "{err}");
        // An encrypted key, PKCS#8 or OpenSSL's PKCS#1: said so, not as DER.
        for (name, encrypted) in [
            ("p8.pem", "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIF\n-----END ENCRYPTED PRIVATE KEY-----\n"),
            ("p1.pem", "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00\n\nMIIE\n-----END RSA PRIVATE KEY-----\n"),
        ] {
            let err = TlsFlags { client_certificate: tls.client_certificate.clone(), client_key: file(name, encrypted.as_bytes()), ..Default::default() }
                .load()
                .unwrap_err();
            assert!(err.contains("--tls_client_key=") && err.contains("the private key is encrypted") && !err.contains("-inform der"), "{err}");
        }
        // PEM that isn't a certificate.
        let err = TlsFlags { certificate: file("garbled.pem", b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n"), ..Default::default() }
            .load()
            .unwrap_err();
        assert!(err.contains("--tls_certificate=") && err.contains("garbled.pem) can't be read"), "{err}");
    }

    const DAY: std::time::Duration = std::time::Duration::from_secs(86400);

    /// The certificates' dates are checked as they're read: one expired or
    /// not valid yet is an error, one expiring within 14 days a warning
    /// (one per file), every certificate of a bundle counting.
    #[test]
    fn tls_certificate_dates() {
        let dir = test_dirs::TestDir::new("ninja-rs-tls-dates");
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_856_000);
        assert_eq!(crate::certs::format_time(1_790_856_000), "2026-10-01 12:00 UTC");
        let file = |name: &str, certs: &[(std::time::SystemTime, std::time::SystemTime)]| {
            let p = dir.join(name);
            std::fs::write(&p, certs.iter().map(|(from, to)| fake_reapi::certs::made_up(*from, *to)).collect::<String>()).unwrap();
            Some(p.to_string_lossy().into_owned())
        };
        let key = file("key.pem", &[]);
        std::fs::write(key.as_ref().unwrap(), "-----BEGIN PRIVATE KEY-----\nMC4C\n-----END PRIVATE KEY-----\n").unwrap();
        let (year, expired, early, soon) =
            ((now - DAY, now + 365 * DAY), (now - 365 * DAY, now - DAY), (now + DAY, now + 365 * DAY), (now - DAY, now + 3 * DAY));
        let client = |name: &str, cert| TlsFlags { client_certificate: file(name, &[cert]), client_key: key.clone(), ..Default::default() }.load_at(now);
        let ca = |name: &str, certs: &[_]| TlsFlags { certificate: file(name, certs), ..Default::default() }.load_at(now);

        assert!(client("year.pem", year).unwrap().expiring.is_empty());
        let err = client("expired.pem", expired).unwrap_err();
        assert!(err.starts_with("the client certificate (--tls_client_certificate=") && err.ends_with("expired.pem) expired on 2026-09-30 12:00 UTC"), "{err}");
        let err = client("early.pem", early).unwrap_err();
        assert!(err.ends_with("early.pem) is not valid until 2026-10-02 12:00 UTC (is the clock right? it says 2026-10-01 12:00 UTC)"), "{err}");
        let loaded = client("soon.pem", soon).unwrap();
        assert_eq!(loaded.expiring.len(), 1);
        let (file_warned, warning) = &loaded.expiring[0];
        assert!(file_warned.ends_with("soon.pem") && warning.ends_with("soon.pem) expires on 2026-10-04 12:00 UTC, in 3 days"), "{warning}");

        // A bundle: each certificate counts.
        assert!(ca("bundle-ok.pem", &[year, year]).unwrap().expiring.is_empty());
        let warned = ca("bundle-soon.pem", &[year, soon, (now - DAY, now + 5 * DAY)]).unwrap().expiring;
        assert!(warned.len() == 1 && warned[0].1.starts_with("CA certificate 2 of 3 (--tls_certificate="), "{warned:?}");
        let err = ca("bundle-expired.pem", &[year, soon, expired]).unwrap_err();
        assert!(err.starts_with("CA certificate 3 of 3 (--tls_certificate=") && err.contains(" expired on "), "{err}");
        // Both files expiring: a warning each.
        let both = TlsFlags {
            certificate: file("ca-soon.pem", &[soon]),
            client_certificate: file("client-soon.pem", &[soon]),
            client_key: key.clone(),
            ..Default::default()
        };
        assert_eq!(both.load_at(now).unwrap().expiring.len(), 2);
        // No date to check without the files.
        assert!(TlsFlags::default().load_at(now).unwrap().expiring.is_empty());
    }

    #[test]
    fn headers_are_checked() {
        let o = p(&["--bes_header=x-api-key=k", "--remote_header=Authorization=Bearer t"]);
        assert_eq!(o.bes.headers, [("x-api-key".to_string(), "k".to_string())]);
        for bad in ["--bes_header=x key=v", "--bes_header=x-api-key=line\nbreak", "--remote_header=caf\u{e9}=v", "--bes_header=novalue"] {
            assert!(parse_args(&[bad.to_string()]).is_err(), "{bad}");
        }
        assert_eq!(p(&["--bes_rpc_timeout=1m"]).bes.rpc_timeout, std::time::Duration::from_secs(60));
        assert_eq!(BesFlags::default().rpc_timeout, std::time::Duration::from_secs(15));
        assert!(parse_args(&["--bes_rpc_timeout=0".to_string()]).is_err());
    }

    /// Bazel's values: empty unsets, durations with units.
    #[test]
    fn bazel_values() {
        use std::time::Duration;
        let o = p(&["--remote_executor=grpc://e:1", "--disk_cache=/c", "--bes_backend=grpc://b:1", "--remote_executor=", "--disk_cache=", "--bes_backend", ""]);
        assert_eq!((o.remote.executor, o.remote.disk_cache, o.bes.backend), (None, None, None));
        assert_eq!(p(&["--execution_log_file=", "--exec_root="]).remote.execution_log, None);
        assert_eq!(p(&["--bes_timeout=90s"]).bes.timeout, Duration::from_secs(90));
        assert_eq!(p(&["--bes_timeout=2m"]).bes.timeout, Duration::from_secs(120));
        assert_eq!(p(&["--bes_timeout=45"]).bes.timeout, Duration::from_secs(45));
        assert_eq!(p(&["--bes_timeout=0"]).bes.timeout, Duration::ZERO);
        assert_eq!(p(&["--remote_racing_delay=1s"]).remote.racing_delay_ms, 1000);
        assert_eq!(p(&["--remote_racing_delay=40"]).remote.racing_delay_ms, 40);
        assert!(p(&["--remote_download_outputs=toplevel"]).remote.download_minimal);
        assert!(p(&["--remote_download_toplevel"]).remote.download_minimal);
        assert!(!p(&["--remote_download_toplevel", "--remote_download_all"]).remote.download_minimal);
        // Paths are absolute, `~` expanded.
        let root = p(&["--exec_root=rel/root"]).remote.exec_root.unwrap();
        assert_eq!(root, std::path::absolute("rel/root").unwrap().to_string_lossy());
        let o = p(&["--remote_retries=2", "--remote_timeout=5m"]);
        assert_eq!((o.remote.retries, o.remote.timeout), (2, Duration::from_secs(300)));
        for bad in ["--bes_timeout=60x", "--bes_timeout=s", "--bes_timeout=-1s", "--remote_download_outputs=some", "--remote_retries=-1", "--remote_timeout=0"]
        {
            assert!(parse_args(&[bad.to_string()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn version() {
        let text = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let ParseOutcome::Exit(text, 0, Out::Stdout) = parse_args(&args).unwrap() else { panic!("--version doesn't exit") };
            text
        };
        assert_eq!(text(&["--version"]), format!("{}\n", ninja_core::parser::NINJA_VERSION));
        assert_eq!(text(&["-v", "--version"]), format!("{}\nninja-rs {}\n", ninja_core::parser::NINJA_VERSION, crate::version()));
    }

    /// One version for Cargo and Bazel builds: the workspace's (Cargo.toml)
    /// and the module's (MODULE.bazel), which BUILD files give the crates.
    #[test]
    fn version_is_the_workspace_one() {
        let path = |f: &str| match std::env::var_os("TEST_SRCDIR") {
            // Bazel: a data file, in the runfiles tree, or (Windows) in
            // the manifest of them.
            Some(r) => {
                let tree = std::path::PathBuf::from(r).join("_main").join(f);
                let manifest = std::env::var_os("RUNFILES_MANIFEST_FILE").and_then(|m| std::fs::read_to_string(m).ok()).unwrap_or_default();
                let key = format!("_main/{f} ");
                manifest.lines().find_map(|l| l.strip_prefix(&key)).map_or(tree, std::path::PathBuf::from)
            }
            // Cargo: the package directory, set when tests run.
            None => std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../..").join(f),
        };
        let read = |f: &str| std::fs::read_to_string(path(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        // The first `version = "..."` after `after`.
        let version_after = |text: &str, after: &str| -> String {
            let rest = &text[text.find(after).unwrap_or_else(|| panic!("no {after}"))..];
            let rest = &rest[rest.find("version = \"").expect("a version") + "version = \"".len()..];
            rest[..rest.find('"').unwrap()].to_string()
        };
        let cargo = version_after(&read("Cargo.toml"), "[workspace.package]");
        let bazel = version_after(&read("MODULE.bazel"), "module(");
        assert_eq!((cargo.as_str(), bazel.as_str()), (env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_VERSION")));
        let commit = crate::version().strip_prefix(env!("CARGO_PKG_VERSION")).unwrap();
        // `+1a2b3c4d5e6f`, `.dirty` after it when the tree was.
        let hex = commit.strip_prefix('+').map(|c| c.strip_suffix(".dirty").unwrap_or(c));
        assert!(commit.is_empty() || hex.is_some_and(|c| c.len() >= 7 && c.bytes().all(|b| b.is_ascii_hexdigit())), "{commit}");
    }
}
