//! Storing the result of a command run locally on a cache miss, in the
//! form a remote execution would have given: only one the action's key
//! describes (see `Capture::store`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use ninja_core::clparser::{DEFAULT_DEPS_PREFIX, filter_show_includes};
use ninja_core::depfile::parse_depfile;
use ninja_core::runner::{LocalCapture, Task, TaskResult};
use rbe_client::proto as re;

use super::{Inner, escape_make};
use crate::action::Prepared;
use crate::store::Store;

/// Stores the result of an action that ran locally after a disk cache miss,
/// in the form a remote execution would have given (paths relative to the
/// working directory), so that both kinds of entries are interchangeable.
pub(super) struct Capture {
    pub(super) inner: Arc<Inner>,
    pub(super) prepared: Box<Prepared>,
    pub(super) task: Task,
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
    ///
    /// Only a result the action's key describes, one its remote execution
    /// would give: not after steps on the host, nor with a variable its
    /// tools read that it can't carry, nor when an input changed while the
    /// command ran, nor when the command read files its inputs lack. The
    /// remote cache, which other machines read, only gets those whose reads
    /// were checked (a depfile or `/showIncludes` lists them), of tools the
    /// toolchains file declares; others stay in the disk cache.
    fn store(&mut self, output: &[u8]) -> Result<()> {
        let disk = self.inner.disk.clone();
        let upload = self.inner.upload && self.inner.client.is_some();
        if disk.is_none() && !upload {
            return Ok(());
        }
        // What the remote cache refuses, before any work when it's all.
        let checkable = self.task.deps.as_deref() == Some("msvc") || self.prepared.depfile.is_some();
        let remote_refusal = if !self.prepared.toolchain_declared {
            Some("the command runs a tool the toolchains file doesn't declare")
        } else if !checkable {
            Some("what the command read can't be checked (no depfile lists it)")
        } else {
            None
        };
        if disk.is_none()
            && let Some(why) = remote_refusal
        {
            bail!("{why}");
        }
        if !self.prepared.local_before.is_empty() || !self.prepared.local_after.is_empty() {
            bail!("the command runs steps on the host");
        }
        if let Some(var) = &self.prepared.env_refused {
            if !self.inner.env_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("ninja-rs: warning: local results aren't cached: {var} is set, which their actions can't have (--action_env={var} gives it them)");
            }
            bail!("{var} is set");
        }
        // As Bazel guards against concurrent changes: an input edited after
        // the key was computed, while the command waited or ran.
        if let Some(changed) = changed_input(&self.prepared, &self.inner.shared.store) {
            bail!("{changed} changed while the command ran");
        }
        let mut checked = false;
        if self.task.deps.as_deref() == Some("msvc") {
            let prefix = if self.task.msvc_deps_prefix.is_empty() { DEFAULT_DEPS_PREFIX } else { self.task.msvc_deps_prefix.as_str() };
            let text = String::from_utf8_lossy(output);
            let included = text.lines().filter_map(|l| filter_show_includes(l.trim_end_matches('\r'), prefix)).filter(|p| !p.is_empty());
            if let Some(dep) = undeclared(&mut self.prepared, included) {
                bail!("{dep} isn't among the action's inputs");
            }
            checked = true;
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
            files.push(re::OutputFile {
                path: rel.clone(),
                digest: Some(digest),
                is_executable: std::fs::metadata(local).is_ok_and(|m| ninja_os::is_executable(&m)),
                ..Default::default()
            });
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
            let deps = parse_depfile(&data).map_err(|e| anyhow::anyhow!("{}: {e}", local.display()))?;
            if let Some(dep) = undeclared(&mut self.prepared, deps.ins.iter().map(String::as_str)) {
                bail!("{dep} isn't among the action's inputs");
            }
            checked = true;
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
        if !upload {
            return Ok(());
        }
        // A depfile the command didn't write lists nothing.
        let why = remote_refusal.or((!checked).then_some("what the command read can't be checked (the depfile is missing)"));
        if let Some(why) = why {
            if self.inner.config.verbose {
                eprintln!("ninja-rs: in the disk cache only: {why}");
            }
            return Ok(());
        }
        let Some(client) = self.inner.client.as_ref().map(|c| c.for_action(&self.inner.action_meta(&self.task), &self.prepared.action_digest.hash)) else {
            return Ok(());
        };
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

/// The first of these files a command read (local paths, relative to the
/// build directory or absolute) that the action's input tree lacks: a
/// remote run wouldn't have read it.
fn undeclared<'a>(p: &mut Prepared, deps: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let windows = p.mapper.windows;
    deps.into_iter().find(|d| !p.mapper.tree_path(d).is_some_and(|t| p.tree_index.contains(&t, windows))).map(str::to_string)
}

/// The first input read from the host whose contents aren't those in the
/// action's key anymore, if any. Through the digest cache: a file whose
/// stamp didn't change isn't read again.
fn changed_input(p: &Prepared, store: &Store) -> Option<String> {
    p.local_inputs
        .iter()
        .find(|(path, d)| !rbe_client::tree::file_digest(path, Some(&store.digests)).is_ok_and(|now| now == *d))
        .map(|(path, _)| path.display().to_string())
}
