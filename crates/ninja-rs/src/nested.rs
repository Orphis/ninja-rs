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

/// The lock files of the logs an invocation and those it runs for write
/// (see `LogLock`), by identity (`<volume>:<file id>` in hex, comma
/// separated), which its commands get: one writing them again (a
/// generator running `-t restat`, while the build waits for it) does so
/// for the invocation, appending only, without the lock it holds.
pub const LOGS_LOCK_ENV: &str = "NINJA_RS_LOGS_LOCK";

/// The remote services an invocation found unreachable, which its
/// commands get: the invocations they run do without them at once, under
/// the same rules (`--remote_local_fallback`), rather than try them again
/// (a refused connection takes Windows 2 s, one never answered
/// `--remote_timeout`). Their endpoints, separated by spaces.
pub const UNREACHABLE_ENV: &str = "NINJA_RS_REMOTE_UNREACHABLE";

/// The certificate files an invocation warned about, expiring soon, which
/// its commands get: the invocations they run don't warn about them again.
/// Paths, separated as in `PATH`.
pub const TLS_WARNED_ENV: &str = "NINJA_RS_TLS_WARNED";

/// The certificate files the invocations running this one warned about.
pub fn tls_warned() -> Vec<String> {
    let value = std::env::var_os(TLS_WARNED_ENV).unwrap_or_default();
    std::env::split_paths(&value).filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_string_lossy().into_owned()).collect()
}

/// `TLS_WARNED_ENV` for `files` (those that can't be listed, holding the
/// separator, left out).
pub fn tls_warned_value(files: &[String]) -> String {
    let listed = files.iter().filter(|f| std::env::join_paths([f.as_str()]).is_ok());
    std::env::join_paths(listed).ok().and_then(|v| v.into_string().ok()).unwrap_or_default()
}

/// The remote services the invocations running this one found unreachable.
pub fn unreachable_endpoints() -> Vec<String> {
    endpoints_in(std::env::var(UNREACHABLE_ENV).ok().as_deref())
}

/// `UNREACHABLE_ENV` for the invocations this one runs: the remote services
/// those running it found unreachable, and `endpoint`.
pub fn with_unreachable(endpoint: &str) -> String {
    adding_endpoint(std::env::var(UNREACHABLE_ENV).ok().as_deref(), endpoint)
}

fn endpoints_in(value: Option<&str>) -> Vec<String> {
    value.unwrap_or_default().split_whitespace().map(str::to_string).collect()
}

fn adding_endpoint(value: Option<&str>, endpoint: &str) -> String {
    let mut all = endpoints_in(value);
    if !all.iter().any(|e| e == endpoint) {
        all.push(endpoint.to_string());
    }
    all.join(" ")
}

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
            Err(e) => ninja_core::status::print_err(&format!("ninja: warning: the options of the invocation running this one: {}: {e}\n", f.display())),
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
    write_secret_flags_in(&loc.dir, flags)
}

/// Writes `flags` to a new private file in `dir`, which is private too.
/// Files there of processes that are gone (killed, crashed) are removed.
fn write_secret_flags_in(dir: &Path, flags: &[String]) -> io::Result<SecretFlags> {
    sweep_secret_flags(dir);
    let path = dir.join(format!("flags-{}.json", std::process::id()));
    // Left by a process that had the same id (the directory is this
    // user's): a new file gets the permissions.
    let _ = std::fs::remove_file(&path);
    let mut file = ipc::create_private_file(&path)?;
    // Removed if it can't be written.
    let secret = SecretFlags(path);
    file.write_all(serde_json::to_string(flags).map_err(io::Error::other)?.as_bytes())?;
    Ok(secret)
}

/// Removes the secret flags files in `dir` whose processes are gone: a
/// process killed (SIGKILL), or that crashed, can't remove its own.
fn sweep_secret_flags(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.strip_prefix("flags-")?.strip_suffix(".json")?.parse::<u32>().ok());
        if let Some(pid) = pid
            && pid != std::process::id()
            && !process_alive(pid)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Whether process `pid` may still run: one that can't be asked about
/// counts (another user's, on Windows; a reused id keeps a file a while).
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return true };
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        // SAFETY: plain system calls; the handle is owned (closed) below.
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return std::io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32);
            }
            let h = OwnedHandle::from_raw_handle(h);
            WaitForSingleObject(h.as_raw_handle(), 0) == WAIT_TIMEOUT
        }
    }
}

/// Whether process `pid` runs this one: its parent, or the parent's, and so
/// on. A generator's `-t restat` whose environment lost `LOGS_LOCK_ENV` (a
/// wrapper scrubbing it) recognizes the build running it, which holds the
/// lock, by the pid its lock file names, rather than wait for it forever.
/// False where processes' parents can't be asked for.
pub(crate) fn runs_this(pid: u32) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    {
        // On Unix, a process whose parent exits gets another (init, a
        // subreaper): ids in the chain are those of running processes.
        let mut current = std::process::id();
        // A bound, should ids ever make a cycle.
        for _ in 0..256 {
            match parent_of(current) {
                Some(parent) if parent == pid => return true,
                Some(parent) if parent > 1 && parent != current => current = parent,
                _ => return false,
            }
        }
        false
    }
    #[cfg(windows)]
    {
        windows_ancestry::runs_this(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
    {
        let _ = pid;
        false
    }
}

/// The parent of process `pid`, as `/proc` says.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (name) state ppid ...`: the name may hold spaces and parentheses.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// The parent of process `pid`, as `proc_pidinfo` says.
#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    // SAFETY: fills `info`, of the size given, nothing else.
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let filled = libc::proc_pidinfo(libc::c_int::try_from(pid).ok()?, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size);
        (filled == size).then_some(info.pbi_ppid)
    }
}

/// `runs_this` on Windows, where a process keeps the id of its parent,
/// which another process may have taken since the parent exited: that
/// one, created after, isn't the parent.
#[cfg(windows)]
mod windows_ancestry {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    pub fn runs_this(pid: u32) -> bool {
        let parents = parents();
        let mut current = std::process::id();
        let Some(mut created) = creation_time(current) else { return false };
        for _ in 0..256 {
            let Some(&parent) = parents.get(&current) else { return false };
            let Some(parent_created) = creation_time(parent).filter(|&c| c <= created) else { return false };
            if parent == pid {
                return true;
            }
            (current, created) = (parent, parent_created);
        }
        false
    }

    /// Each running process's parent id.
    fn parents() -> std::collections::HashMap<u32, u32> {
        let mut parents = std::collections::HashMap::new();
        // SAFETY: plain system calls; the snapshot handle is owned (closed)
        // below, and the entry's size is set as they want it.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return parents;
            }
            let _owned = OwnedHandle::from_raw_handle(snapshot);
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
        }
        parents
    }

    /// When process `pid` was created (100 ns since 1601), if it can be
    /// asked.
    fn creation_time(pid: u32) -> Option<u64> {
        // SAFETY: plain system calls; the handle is owned (closed) below.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let h = OwnedHandle::from_raw_handle(h);
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
            if GetProcessTimes(std::os::windows::io::AsRawHandle::as_raw_handle(&h), &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
                return None;
            }
            Some(u64::from(creation.dwHighDateTime) << 32 | u64::from(creation.dwLowDateTime))
        }
    }
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

/// The long options given, as nested invocations take them: those in
/// effect (not those left out for one they conflict with, which nested
/// invocations, taking them all as given by settings, would fail on), paths
/// absolute, since they run elsewhere.
fn exported_flags(opts: &Options) -> Vec<String> {
    let absolute = |name: &str| -> Option<String> {
        match name {
            "disk_cache" => opts.remote.disk_cache.clone(),
            "exec_root" => opts.remote.exec_root.clone(),
            "tls_certificate" => opts.tls.certificate.clone(),
            "tls_client_certificate" => opts.tls.client_certificate.clone(),
            "tls_client_key" => opts.tls.client_key.clone(),
            _ => None,
        }
    };
    opts.given
        .iter()
        .filter(|g| g.combined_form.starts_with("--") && !g.left_out && inherited(&g.name))
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
/// exports `NINJA_RS_PARENT`: for builds and `-t exec`, whose commands may
/// run ninja-rs (not for other tools, `-t daemon start` included: the
/// daemon would get the secret flags). Call before any thread starts. The
/// returned `SecretFlags` must live as long as the commands that may read
/// it.
pub fn export(opts: &mut Options) -> (Option<Relay>, Option<SecretFlags>) {
    if opts.tool.as_deref().is_some_and(|t| t != "exec") {
        return (None, None);
    }
    let invocation_id = opts.bes.invocation_id.get_or_insert_with(crate::bes::new_uuid).clone();
    let build_request_id = opts.bes.build_request_id.get_or_insert_with(crate::bes::new_uuid).clone();
    let relay = if opts.bes.relay.is_none() && (opts.bes.backend.is_some() || opts.bes.binary_file.is_some()) {
        match Relay::bind() {
            Ok(r) => Some(r),
            Err(e) => {
                ninja_core::status::print_err(&format!("ninja: warning: nested builds' events won't be reported: {e}\n"));
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
                ninja_core::status::print_err(&format!("ninja: warning: nested builds won't get --remote_header: {e}\n"));
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
        let dir = test_dirs::TestDir::new("ninja-rs-secret-flags");
        let secrets = write_secret_flags_in(&dir, &flags).unwrap();
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

    /// Files of processes that are gone (killed, crashed: they couldn't
    /// remove them) are removed when another invocation writes its own;
    /// those of processes still running stay.
    #[test]
    fn secret_flags_of_processes_gone_are_swept() {
        let dir = test_dirs::TestDir::new("ninja-rs-secret-flags-sweep");
        let spawn = |program: &str, args: &[&str]| std::process::Command::new(program).args(args).stdout(std::process::Stdio::null()).spawn().unwrap();
        let mut gone = if cfg!(windows) { spawn("cmd", &["/c", "exit"]) } else { spawn("sh", &["-c", "exit"]) };
        gone.wait().unwrap();
        let mut running = if cfg!(windows) { spawn("ping", &["-n", "60", "127.0.0.1"]) } else { spawn("sleep", &["60"]) };
        let file = |pid: u32| dir.join(format!("flags-{pid}.json"));
        for pid in [gone.id(), running.id()] {
            std::fs::write(file(pid), "[]").unwrap();
        }
        let secrets = write_secret_flags_in(&dir, &["--remote_header=k=v".to_string()]).unwrap();
        let _ = running.kill();
        let _ = running.wait();
        assert!(!file(gone.id()).exists(), "the file of a process gone stayed");
        assert!(file(running.id()).exists(), "the file of a running process was removed");
        assert!(secrets.0.exists());
    }

    /// Tools other than `-t exec` run no builds: nothing to pass on (the
    /// daemon `-t daemon start` starts would get the secret flags).
    #[test]
    fn only_builds_export() {
        let mut o = parse(&["--remote_header=k=v", "-t", "daemon", "start"]);
        let (relay, secrets) = export(&mut o);
        assert!(relay.is_none() && secrets.is_none());
        assert_eq!(o.bes.invocation_id, None);
    }

    /// The file has permissions of its own, this user's and the system's,
    /// none from its directory (which may let others read).
    #[cfg(windows)]
    #[test]
    fn secret_flags_file_is_private() {
        let dir = test_dirs::TestDir::new("ninja-rs-secret-flags");
        let secrets = write_secret_flags_in(&dir, &["--remote_header=k=v".to_string()]).unwrap();
        let out = std::process::Command::new("icacls").arg(&secrets.0).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(text.lines().filter(|l| l.contains(":(")).count(), 2, "{text}");
        assert!(!text.contains("(I)"), "{text}");
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

    /// An option typed here won over one of a settings file: nested
    /// invocations get the one in effect only. Given both, as settings,
    /// they failed on the conflict (CMake's `-t restat`, every nested
    /// build), or warned again.
    #[test]
    fn options_left_out_arent_passed_on() {
        use crate::options::parse_invocation;
        let parse = |args: &[&str], explicit: &[bool]| {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            match parse_invocation(&args, explicit) {
                Ok(ParseOutcome::Run(o)) => o,
                Ok(ParseOutcome::Exit(..)) => panic!("exit"),
                Err(e) => panic!("{e}"),
            }
        };
        // `.ninjars`: --remote_strict, --remote_cache; typed: --remote_racing,
        // --remote_executor.
        let outer = parse(
            &["--remote_strict", "--remote_cache=grpc://c:1", "--noremote_strict", "--remote_strict", "--remote_racing", "--remote_executor=grpc://e:1"],
            &[false, false, false, false, true, true],
        );
        assert_eq!(outer.warnings.len(), 2, "{:?}", outer.warnings);
        let flags = exported_flags(&outer);
        assert_eq!(flags, ["--remote_racing", "--remote_executor=grpc://e:1"]);
        // A nested invocation takes them as settings: the same options in
        // effect, nothing to warn about.
        let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
        let nested = parse(&refs, &[false, false]);
        assert!(nested.remote.racing && !nested.remote.strict && nested.remote.cache.is_none());
        assert_eq!(nested.remote.executor.as_deref(), Some("grpc://e:1"));
        assert!(nested.warnings.is_empty(), "{:?}", nested.warnings);
        // The other way round.
        let outer = parse(&["--remote_racing", "--remote_strict"], &[false, true]);
        assert_eq!(exported_flags(&outer), ["--remote_strict"]);
    }

    #[test]
    fn unreachable_endpoints_add_up() {
        assert_eq!(adding_endpoint(None, "grpc://a:1"), "grpc://a:1");
        assert_eq!(adding_endpoint(Some("grpc://a:1"), "grpcs://b"), "grpc://a:1 grpcs://b");
        assert_eq!(adding_endpoint(Some("grpc://a:1 grpcs://b"), "grpc://a:1"), "grpc://a:1 grpcs://b");
        assert_eq!(endpoints_in(Some(" grpc://a:1  grpcs://b ")), ["grpc://a:1", "grpcs://b"]);
        assert!(endpoints_in(None).is_empty());
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
