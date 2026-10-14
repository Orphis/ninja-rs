//! Nested invocations: a build command running ninja-rs again, as LLVM's
//! host tools (`cmake --build NATIVE`), ExternalProject and the like do.
//!
//! The outer invocation exports `NINJA_RS_PARENT` to its commands: its
//! invocation ids, its settings (the long options, but per-build ones such
//! as build events or `-j`) and where build events go. A nested invocation
//! takes the settings first, then its own arguments (which win), reports
//! its remote actions under the same invocation, appends to the same
//! execution log, and sends its build events to the outermost invocation
//! (`Relay`), which merges them into its stream: one invocation, wherever
//! its actions ran.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use ninja_daemon::{Location, ipc};
use serde::{Deserialize, Serialize};

use crate::options::Options;

pub const ENV: &str = "NINJA_RS_PARENT";

/// What an invocation passes to the invocations its commands run.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Parent {
    pub invocation_id: String,
    pub build_request_id: String,
    /// Options nested invocations take before their own.
    pub flags: Vec<String>,
    /// `--execution_log_file`, appended to.
    pub execution_log: Option<String>,
    /// Where build events go (the outermost invocation's relay).
    pub relay: Option<RelayAddress>,
    /// The `.ninjars` files the outermost invocation read, and its
    /// workspace: their options are in `flags`, their configs serve the
    /// `--config`s of nested invocations.
    #[serde(default)]
    pub rc_files: Vec<String>,
    #[serde(default)]
    pub rc_workspace: Option<String>,
    /// A file holding the options whose values may be credentials
    /// (`--remote_header`), in a directory only the user can read: they're
    /// not in the environment every build command (and its crash dumps, and
    /// CI logs) sees.
    #[serde(default)]
    pub secret_flags: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayAddress {
    pub dir: PathBuf,
    pub id: String,
}

impl RelayAddress {
    fn location(&self) -> Location {
        Location { dir: self.dir.clone(), id: self.id.clone() }
    }
}

/// The invocation that ran this one, if any, with its secret flags.
pub fn parent() -> Option<Parent> {
    parent_from(&std::env::var(ENV).ok()?)
}

/// A `NINJA_RS_PARENT` value, with its secret flags.
fn parent_from(json: &str) -> Option<Parent> {
    let mut p: Parent = serde_json::from_str(json).ok()?;
    if let Some(f) = &p.secret_flags {
        match std::fs::read_to_string(f).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Vec<String>>(&t).map_err(|e| e.to_string())) {
            Ok(flags) => p.flags.extend(flags),
            Err(e) => eprintln!("ninja: warning: the options of the invocation running this one: {}: {e}", f.display()),
        }
    }
    Some(p)
}

/// The secret flags' file of this invocation, removed when dropped.
pub struct SecretFlags(PathBuf);

impl Drop for SecretFlags {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes `flags` to a file only the user can read, in the daemon's private
/// directory.
fn write_secret_flags(flags: &[String]) -> io::Result<SecretFlags> {
    let loc = Location::with_id("flags")?;
    loc.create_dir()?;
    let path = loc.dir.join(format!("flags-{}.json", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(serde_json::to_string(flags).map_err(io::Error::other)?.as_bytes())?;
    Ok(SecretFlags(path))
}

/// Options not passed on: per invocation (ids, build events, logs written
/// once), about the terminal, or about `.ninjars` files (whose options
/// are passed on instead).
fn inherited(name: &str) -> bool {
    let base = name.strip_prefix("no").filter(|b| crate::options::is_bool_option(b)).unwrap_or(name);
    !(base.starts_with("bes_")
        || base.starts_with("build_event_")
        || crate::options::RC_SELECTION.contains(&base)
        || matches!(
            base,
            "invocation_id"
                | "build_request_id"
                | "execution_log_file"
                | "version"
                | "help"
                | "verbose"
                | "quiet"
                | "status"
                | "jobserver-pool"
                | "config"
                | "announce_rc"
                | "enable_platform_specific_config"
        ))
}

/// The long options given, as nested invocations take them: paths
/// absolute, since they run elsewhere.
fn exported_flags(opts: &Options) -> Vec<String> {
    let absolute = |name: &str| -> Option<String> {
        match name {
            "disk_cache" => opts.remote.disk_cache.clone(),
            "tls_certificate" => opts.tls.certificate.clone(),
            "tls_client_certificate" => opts.tls.client_certificate.clone(),
            "tls_client_key" => opts.tls.client_key.clone(),
            _ => None,
        }
    };
    opts.given
        .iter()
        .filter(|g| g.combined_form.starts_with("--") && inherited(&g.name))
        .map(|g| match absolute(&g.name) {
            Some(v) => format!("--{}={v}", g.name),
            None if g.value.is_empty() && !g.combined_form.contains('=') => format!("--{}", g.name),
            None => format!("--{}={}", g.name, g.value),
        })
        .collect()
}

/// Takes what the outer invocation passes on (the settings were parsed
/// first already): its ids, the relay for build events, its execution log.
pub fn adopt(opts: &mut Options, parent: &Parent) {
    opts.bes.invocation_id.get_or_insert_with(|| parent.invocation_id.clone());
    opts.bes.build_request_id.get_or_insert_with(|| parent.build_request_id.clone());
    if opts.bes.backend.is_none() && opts.bes.binary_file.is_none() {
        opts.bes.relay = parent.relay.clone();
    }
    if opts.remote.execution_log.is_none() && parent.execution_log.is_some() {
        opts.remote.execution_log = parent.execution_log.clone();
        opts.remote.execution_log_append = true;
    }
}

/// Sets this invocation's ids (so that nested ones share them), binds the
/// relay when this is the outermost invocation with build events, and
/// exports `NINJA_RS_PARENT`. Call before any thread starts. The returned
/// `SecretFlags` must live as long as the commands that may read it.
pub fn export(opts: &mut Options) -> (Option<Relay>, Option<SecretFlags>) {
    let invocation_id = opts.bes.invocation_id.get_or_insert_with(crate::bes::new_uuid).clone();
    let build_request_id = opts.bes.build_request_id.get_or_insert_with(crate::bes::new_uuid).clone();
    let relay =
        if opts.bes.relay.is_none() && (opts.bes.backend.is_some() || opts.bes.binary_file.is_some()) && opts.tool.as_deref().is_none_or(|t| t == "exec") {
            match Relay::bind() {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("ninja: warning: nested builds' events won't be reported: {e}");
                    None
                }
            }
        } else {
            None
        };
    let is_secret = |f: &String| crate::options::SECRET_OPTIONS.iter().any(|n| f.starts_with(&format!("--{n}=")));
    let (secret, flags): (Vec<String>, Vec<String>) = exported_flags(opts).into_iter().partition(is_secret);
    let secrets = if secret.is_empty() {
        None
    } else {
        match write_secret_flags(&secret) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("ninja: warning: nested builds won't get --remote_header: {e}");
                None
            }
        }
    };
    let parent = Parent {
        invocation_id,
        build_request_id,
        flags,
        execution_log: opts.remote.execution_log.clone(),
        relay: relay.as_ref().map(|r| r.address.clone()).or_else(|| opts.bes.relay.clone()),
        rc_files: opts.rc.read.clone(),
        rc_workspace: opts.rc.workspace_dir.clone(),
        secret_flags: secrets.as_ref().map(|s| s.0.clone()),
    };
    if let Ok(json) = serde_json::to_string(&parent) {
        // SAFETY: called before the process starts threads.
        unsafe { std::env::set_var(ENV, json) };
    }
    (relay, secrets)
}

/// Where nested invocations send their build events: a local socket of the
/// outermost invocation.
pub struct Relay {
    pub address: RelayAddress,
    pub listener: ipc::Listener,
}

impl Relay {
    fn bind() -> io::Result<Relay> {
        let loc = Location::with_id(&format!("relay-{}", std::process::id()))?;
        loc.create_dir()?;
        let listener = ipc::Listener::bind(&loc)?;
        Ok(Relay { address: RelayAddress { dir: loc.dir.clone(), id: loc.id.clone() }, listener })
    }

    /// Removes the socket file (Unix).
    pub fn remove(address: &RelayAddress) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(address.location().socket());
        #[cfg(not(unix))]
        let _ = address;
    }
}

/// A nested invocation's connection to the relay: its build directory
/// first, then its events.
pub fn connect(address: &RelayAddress, build_dir: &Path) -> io::Result<ipc::Stream> {
    let mut s = ipc::connect(&address.location())?;
    write_frame(&mut s, build_dir.to_string_lossy().as_bytes())?;
    Ok(s)
}

pub fn write_frame(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(4 + data.len());
    frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
    frame.extend_from_slice(data);
    w.write_all(&frame)?;
    w.flush()
}

/// The next frame; None at the end of the stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut data = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut data)?;
    Ok(Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{ParseOutcome, parse_args};

    #[test]
    fn secret_flags_stay_out_of_the_environment() {
        let flags = vec!["--remote_header=authorization=Bearer t".to_string()];
        let secrets = write_secret_flags(&flags).unwrap();
        let path = secrets.0.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let parent = Parent { flags: vec!["--remote_jobs=3".into()], secret_flags: Some(path.clone()), ..Default::default() };
        let json = serde_json::to_string(&parent).unwrap();
        assert!(!json.contains("Bearer"));
        let read = parent_from(&json).unwrap();
        assert_eq!(read.flags, vec!["--remote_jobs=3".to_string(), flags[0].clone()]);
        drop(secrets);
        assert!(!path.exists());
    }

    fn parse(args: &[&str]) -> Options {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_args(&v).unwrap() {
            ParseOutcome::Run(o) => *o,
            ParseOutcome::Exit(..) => panic!("exit"),
        }
    }

    #[test]
    fn settings_pass_on() {
        let o = parse(&[
            "-j8",
            "--remote_executor=grpc://x:1",
            "--remote_header",
            "k=v",
            "--disk_cache=dc",
            "--remote_download_minimal",
            "--bes_backend=grpc://x:1",
            "--build_event_binary_file=b.bep",
            "--execution_log_file=e.jsonl",
            "--invocation_id=abc",
            "--config=ci",
            "--announce_rc",
            "--nosystem_rc",
            "--remote_verbose=false",
            "all",
        ]);
        let flags = exported_flags(&o);
        let dc = std::path::absolute("dc").unwrap().to_string_lossy().into_owned();
        assert_eq!(
            flags,
            vec![
                "--remote_executor=grpc://x:1".to_string(),
                "--remote_header=k=v".into(),
                format!("--disk_cache={dc}"),
                "--remote_download_minimal".into(),
                "--remote_verbose=false".into()
            ]
        );
        // A nested invocation's own arguments win.
        let mut args = flags.clone();
        args.push("--remote_download_outputs=all".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut child = parse(&refs);
        assert!(!child.remote.download_minimal);
        let parent = Parent {
            invocation_id: "abc".into(),
            build_request_id: "req".into(),
            flags,
            execution_log: Some("/log".into()),
            relay: Some(RelayAddress { dir: "/d".into(), id: "relay-1".into() }),
            ..Default::default()
        };
        adopt(&mut child, &parent);
        assert_eq!(child.bes.invocation_id.as_deref(), Some("abc"));
        assert_eq!(child.bes.relay, parent.relay);
        assert!(child.remote.execution_log_append);
    }

    #[test]
    fn frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"/build").unwrap();
        write_frame(&mut buf, b"").unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).unwrap().as_deref(), Some(&b"/build"[..]));
        assert_eq!(read_frame(&mut r).unwrap().as_deref(), Some(&b""[..]));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }
}
