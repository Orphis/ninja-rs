//! The REAPI backend: execution policy, action cache lookups (the local
//! disk cache, then the remote one), upload, Execute, and output download
//! (with depfile and /showIncludes mapping). With only a disk cache, cache
//! misses run locally and their results are stored.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ninja_core::clparser::{DEFAULT_DEPS_PREFIX, filter_show_includes};
use ninja_core::depfile::parse_depfile;
use ninja_core::runner::{CasOutput, ExecKind, ExitStatus, LocalCapture, RemoteBackend, RemoteFuture, RemoteOutcome, Task, TaskResult};
use rbe_client::proto as re;
use rbe_client::{ActionMeta, Client};

use crate::action::{Prepared, Shared, prepare};
use crate::disk_cache::DiskCache;
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
    pub verbose: bool,
}

pub struct Backend {
    inner: Arc<Inner>,
}

struct Inner {
    /// The remote service; without it, the disk cache alone.
    client: Option<Client>,
    disk: Option<Arc<DiskCache>>,
    shared: Shared,
    config: BackendConfig,
}

impl Backend {
    /// A backend with a remote service, a disk cache, or both.
    pub async fn new(config: BackendConfig, client: Option<Client>, disk: Option<Arc<DiskCache>>, store: Arc<Store>) -> Result<Self> {
        let windows = cfg!(windows);
        let toolchains = match &config.toolchains_file {
            Some(f) => Toolchains::load(&config.build_dir.join(f), windows).map_err(anyhow::Error::msg)?,
            None => Toolchains::default(),
        };
        if let Some(mut client) = client.clone() {
            let caps = client.capabilities().await.context("GetCapabilities")?;
            if let Some(cc) = &caps.cache_capabilities
                && !cc.digest_functions.is_empty()
                && !cc.digest_functions.contains(&(re::digest_function::Value::Sha256 as i32))
            {
                bail!("the remote cache does not support SHA-256");
            }
            if caps.execution_capabilities.as_ref().is_some_and(|e| !e.exec_enabled) {
                bail!("the remote instance does not support execution");
            }
        }
        let shared = Shared {
            build_dir: config.build_dir.clone(),
            toolchains,
            exec_root: config.exec_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            store,
            windows,
        };
        Ok(Self { inner: Arc::new(Inner { client, disk, shared, config }) })
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
        Box::pin(async move { inner.execute(task).await })
    }

    fn max_jobs(&self) -> usize {
        self.inner.config.jobs
    }
}

enum Failure {
    /// The action could not be prepared: run locally.
    Prepare(String),
    /// The remote service failed.
    Infra(anyhow::Error),
    /// A disk cache miss without a remote service: run locally, then store
    /// the result.
    RunLocally(Box<Prepared>),
}

impl Inner {
    async fn execute(self: Arc<Self>, task: Task) -> RemoteOutcome {
        match self.clone().try_execute(&task).await {
            Ok(r) => RemoteOutcome::Done(r),
            Err(Failure::Prepare(reason)) => RemoteOutcome::Fallback(reason),
            Err(Failure::RunLocally(prepared)) => RemoteOutcome::RunLocally(Box::new(Capture { inner: self, prepared, task })),
            Err(Failure::Infra(e)) => {
                if self.config.local_fallback {
                    RemoteOutcome::Fallback(format!("remote execution failed: {e:#}"))
                } else {
                    RemoteOutcome::Done(TaskResult {
                        edge: task.edge,
                        status: ExitStatus::Failure(1),
                        output: format!("ninja-rs: remote execution failed: {e:#}\n").into_bytes(),
                        kind: ExecKind::Remote,
                        cas_outputs: Vec::new(),
                    })
                }
            }
        }
    }

    async fn try_execute(self: Arc<Self>, task: &Task) -> Result<TaskResult, Failure> {
        let me = self.clone();
        let t = task.clone();
        let prepared = tokio::task::spawn_blocking(move || prepare(&t, &me.shared))
            .await
            .map_err(|e| Failure::Prepare(format!("internal error: {e}")))?
            .map_err(Failure::Prepare)?;
        let meta = ActionMeta {
            mnemonic: task.binding("rbe_mnemonic").unwrap_or_default().to_string(),
            target_id: task.binding("rbe_target").map(|t| self.config.targets.label(t)).unwrap_or_default(),
            configuration_id: self.config.targets.configuration(task.binding("rbe_config")).map(|c| c.id()).unwrap_or_default(),
        };
        let digest = prepared.action_digest.clone();

        // The disk cache first.
        let disk_hit = match &self.disk {
            Some(disk) => {
                let (disk, d) = (disk.clone(), digest.clone());
                tokio::task::spawn_blocking(move || disk.get_action_result(&d)).await.ok().flatten().filter(|r| r.exit_code == 0)
            }
            None => None,
        };
        if let Some(result) = disk_hit {
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
            return Ok(TaskResult { edge: task.edge, status: ExitStatus::Success, output, kind: ExecKind::DiskCacheHit, cas_outputs: Vec::new() });
        }
        let Some(client) = &self.client else { return Err(Failure::RunLocally(Box::new(prepared))) };
        let client = client.for_action(&meta, &prepared.action_digest.hash);

        // Local steps first (e.g. deleting an archive before re-creating it).
        let mut output = Vec::new();
        if let Some(failed) = self.run_local_steps(task, &prepared.local_before, &mut output).await {
            return Ok(failed);
        }

        // Then the remote action cache.
        let cached = client.get_action_result(&digest).await.map_err(Failure::Infra)?;
        let (result, hit) = match cached {
            Some(r) if r.exit_code == 0 => (r, true),
            _ => {
                let to_upload = self.shared.store.filter_unknown(prepared_blobs(&prepared));
                client.upload_blobs(&to_upload).await.map_err(Failure::Infra)?;
                self.shared.store.mark_uploaded(to_upload.0.keys());
                let resp = client.execute(&digest, false).await.map_err(Failure::Infra)?;
                if let Some(st) = &resp.status
                    && st.code != 0
                {
                    return Err(Failure::Infra(anyhow::anyhow!("Execute returned status {}: {}", st.code, st.message)));
                }
                let hit = resp.cached_result;
                (resp.result.ok_or_else(|| Failure::Infra(anyhow::anyhow!("Execute returned no result")))?, hit)
            }
        };
        let keep = self.disk.is_some() && result.exit_code == 0;
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
            let mut msg = format!(
                "ninja-rs: {} {} ({} inputs{target})\n",
                if hit { "remote cache hit" } else { "executed remotely" },
                digest.hash,
                prepared.input_files
            )
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
        Ok(TaskResult { edge: task.edge, status, output, kind, cas_outputs })
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
            return Some(TaskResult { edge: task.edge, status: ExitStatus::Failure(code), output: std::mem::take(output), kind: ExecKind::Remote, cas_outputs: Vec::new() });
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
                files.push((local, re::OutputFile { path: String::new(), digest: f.digest, is_executable: f.is_executable, contents: vec![], node_properties: None }));
            }
        }
        let depfile_local = p.depfile.as_ref().map(|(_, l)| l.clone());
        let mut needed = Vec::new();
        for (local, f) in &files {
            let d = f.digest.clone().unwrap_or_default();
            if Some(local) != depfile_local.as_ref() && same_content(local, &d, &self.shared.store) {
                continue;
            }
            if f.contents.is_empty() || d.size_bytes == 0 {
                needed.push(d);
            }
        }
        let blobs = self.fetch_blobs(client, &needed).await?;
        for (local, f) in &files {
            let d = f.digest.clone().unwrap_or_default();
            let is_depfile = Some(local) == depfile_local.as_ref();
            if !is_depfile && same_content(local, &d, &self.shared.store) {
                continue;
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
        if keep
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

    /// Blobs from the disk cache, else from the remote CAS (stored on disk
    /// then).
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
        let Ok(d) = parse_depfile(data) else { return data.to_vec() };
        let map = |s: &str| p.mapper.map_back(s, &p.tree_index);
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

    fn map_show_includes(&self, stdout: &[u8], task: &Task, p: &Prepared) -> Vec<u8> {
        let prefix = if task.msvc_deps_prefix.is_empty() { DEFAULT_DEPS_PREFIX } else { task.msvc_deps_prefix.as_str() };
        let text = String::from_utf8_lossy(stdout);
        let mut out = String::new();
        for line in text.split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            let ending = &line[body.len()..];
            match filter_show_includes(body, prefix) {
                Some(path) if !path.is_empty() => {
                    let mapped = p.mapper.map_back(path, &p.tree_index);
                    let mapped = if !crate::paths::is_abs(&mapped, cfg!(windows)) {
                        // Relative to the build dir: make it absolute so that
                        // ninja's normalization sees what a local cl.exe prints.
                        let abs = self.shared.build_dir.join(&mapped);
                        crate::scanner::normalize(&abs).to_string_lossy().into_owned()
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

impl Capture {
    fn store(&mut self, output: &[u8]) -> Result<()> {
        let Some(disk) = self.inner.disk.clone() else { return Ok(()) };
        let digests = Some(&self.inner.shared.store.digests as &dyn rbe_client::tree::DigestCache);
        let mut files = Vec::new();
        for (rel, local) in &self.prepared.outputs {
            if !local.is_file() {
                bail!("{} was not produced", local.display());
            }
            let digest = disk.put_file(local, digests)?;
            files.push(re::OutputFile { path: rel.clone(), digest: Some(digest), is_executable: is_executable(local), ..Default::default() });
        }
        if let Some((rel, local)) = self.prepared.depfile.clone()
            && let Ok(data) = std::fs::read(&local)
        {
            let data = self.depfile_to_remote(&data);
            let digest = rbe_client::digest_of(&data);
            disk.put_bytes(&digest, &data)?;
            files.push(re::OutputFile { path: rel, digest: Some(digest), ..Default::default() });
        }
        let stdout = if self.task.deps.as_deref() == Some("msvc") { self.show_includes_to_remote(output) } else { output.to_vec() };
        let result = re::ActionResult {
            output_files: files,
            exit_code: 0,
            stdout_raw: stdout,
            execution_metadata: Some(re::ExecutedActionMetadata { worker: "local".into(), ..Default::default() }),
            ..Default::default()
        };
        disk.put_action_result(&self.prepared.action_digest, &result)?;
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
