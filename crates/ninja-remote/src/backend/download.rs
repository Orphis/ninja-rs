//! Fetching the blobs of outputs into files next to them: from the disk
//! cache, else from the remote CAS (large blobs as they arrive, or as the
//! chunks the remote splits them into), within a memory budget; and
//! writing outputs atomically.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rbe_client::Client;
use rbe_client::proto as re;

use super::{DiskMiss, Inner, blocking};
use crate::disk_cache::DiskCache;

/// Above this size, a blob is fetched into a file as it arrives rather
/// than read whole.
const STREAMED: i64 = 4 << 20;

/// Bytes of blobs downloads hold in memory at once, all tasks together
/// (small blobs between their read and their file, chunks of a large blob
/// until stored), in KiB: a semaphore's permits.
pub(super) const DOWNLOAD_BUDGET_KIB: usize = 64 << 10;

/// Small blobs read at once: a group, written to files before the
/// memory it takes is given back, of a batch or so...
const SMALL_GROUP: i64 = 4 << 20;

/// ...and groups one download reads at once.
const SMALL_GROUPS: usize = 4;

/// Chunks of a large blob fetched at once: they are in memory until stored.
const CHUNK_GROUP: i64 = 16 << 20;

/// How old a temporary file next to outputs must be for a sweep to remove
/// it (see `Inner::sweep`): one a writer still writes is newer.
const STALE_TEMP: std::time::Duration = std::time::Duration::from_secs(3600);

/// The permits of `DOWNLOAD_BUDGET_KIB` that `bytes` take: all of them for
/// more (one at a time then).
fn budget_kib(bytes: i64) -> u32 {
    (bytes.max(1) as u64).div_ceil(1024).min(DOWNLOAD_BUDGET_KIB as u64) as u32
}

/// Blobs in groups of at most `bytes` each (`size` gives a blob's), but
/// for one larger alone.
fn grouped<T>(blobs: Vec<T>, size: impl Fn(&T) -> i64, bytes: i64) -> Vec<Vec<T>> {
    let mut groups: Vec<Vec<T>> = Vec::new();
    let mut total = 0;
    for b in blobs {
        if groups.is_empty() || total + size(&b) > bytes {
            groups.push(Vec::new());
            total = 0;
        }
        total += size(&b);
        groups.last_mut().expect("a group").push(b);
    }
    groups
}

/// A disk cache write that may fail (a full disk, a read-only shared
/// cache): the build goes on without it. Said once, and whether it worked.
pub(super) fn disk_store(warned: &std::sync::atomic::AtomicBool, write: impl FnOnce() -> Result<()>) -> bool {
    match write() {
        Ok(()) => true,
        Err(e) => {
            if !warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("ninja-rs: warning: the disk cache can't store results: {e:#}");
            }
            false
        }
    }
}

impl Inner {
    /// Whether a blob is fetched as it arrives rather than read whole: a
    /// large one, or one the disk cache stores as chunks.
    fn streamed(&self, d: &re::Digest) -> bool {
        d.size_bytes > STREAMED || (self.disk.is_some() && crate::disk_cache::is_chunked(d))
    }

    /// Blobs for outputs (digest, an output it's for), each into a file
    /// next to its output: from the disk cache, a chunk at a time, else
    /// from the remote CAS (stored on disk then). A large blob comes as it
    /// arrives (as the chunks the remote splits it into when it does,
    /// downloading only those the disk cache lacks); small ones in groups,
    /// each written before the next is read. What downloads hold in memory
    /// is bounded, all tasks together (`DOWNLOAD_BUDGET_KIB`). A blob the
    /// disk cache lacks without a remote to fetch it from is a `DiskMiss`.
    pub(super) async fn fetch_blobs(&self, client: Option<&Client>, needed: &[(re::Digest, PathBuf)]) -> Result<Fetched> {
        let mut fetched = Fetched::default();
        let mut seen = std::collections::HashSet::new();
        let needed: Vec<&(re::Digest, PathBuf)> = needed.iter().filter(|(d, _)| d.size_bytes > 0 && seen.insert(d.hash.as_str())).collect();
        let missing = blocking(|| {
            let mut missing = Vec::new();
            for &(d, dest) in &needed {
                self.sweep(dest);
                let found = self.disk.as_ref().is_some_and(|disk| fetched.stage(d, dest, |f| disk.read_blob_into(d, f)));
                if !found {
                    missing.push((d.clone(), dest.clone()));
                }
            }
            missing
        });
        if missing.is_empty() {
            return Ok(fetched);
        }
        let Some(client) = client else { return Err(DiskMiss(missing.len()).into()) };
        let (large, small): (Vec<_>, Vec<_>) = missing.into_iter().partition(|(d, _)| self.streamed(d));
        // Each task gives back its files, removed if it's dropped unjoined.
        let mut tasks: tokio::task::JoinSet<Result<Vec<(String, TempFile)>>> = tokio::task::JoinSet::new();
        for (d, dest) in large {
            let (client, disk, verbose, budget, warned) =
                (client.clone(), self.disk.clone(), self.config.verbose, self.download_budget.clone(), self.disk_warned.clone());
            tasks.spawn(async move {
                let staged = fetch_into_file(&client, disk.as_deref(), &d, &dest, verbose, &budget, &warned).await?;
                Ok(vec![(d.hash, staged)])
            });
        }
        let groups = Arc::new(tokio::sync::Semaphore::new(SMALL_GROUPS));
        for group in grouped(small, |(d, _)| d.size_bytes, SMALL_GROUP) {
            let (client, disk, budget, warned, groups) =
                (client.clone(), self.disk.clone(), self.download_budget.clone(), self.disk_warned.clone(), groups.clone());
            tasks.spawn(async move {
                let _turn = groups.acquire_owned().await?;
                let _held = budget.acquire_many_owned(budget_kib(group.iter().map(|(d, _)| d.size_bytes).sum())).await?;
                let digests: Vec<re::Digest> = group.iter().map(|(d, _)| d.clone()).collect();
                let read = client.read_blobs(&digests).await?;
                blocking(|| {
                    let mut staged = Vec::new();
                    for (d, dest) in &group {
                        let data = read.get(&d.hash).with_context(|| format!("blob {} wasn't read", rbe_client::digest_str(d)))?;
                        if let Some(disk) = &disk {
                            disk_store(&warned, || Ok(disk.put_bytes(d, data)?));
                        }
                        staged.push((d.hash.clone(), stage_bytes(dest, data)?));
                    }
                    Ok(staged)
                })
            });
        }
        // Every task ends before this returns: their files are known.
        let mut failed = None;
        while let Some(r) = tasks.join_next().await {
            match r.map_err(anyhow::Error::from).and_then(|staged| staged) {
                Ok(staged) => {
                    for (hash, file) in staged {
                        fetched.files.insert(hash, (file.keep(), true));
                    }
                }
                Err(e) => failed = failed.or(Some(e)),
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(fetched),
        }
    }

    /// Removes the temporary files interrupted writes left (a build
    /// stopped, a process killed) next to `output`, an hour old, once per
    /// directory and build: the directories this build writes to.
    pub(super) fn sweep(&self, output: &Path) {
        let Some(dir) = output.parent() else { return };
        {
            let mut swept = self.swept.lock().unwrap();
            if swept.contains(dir) {
                return;
            }
            swept.insert(dir.to_path_buf());
        }
        sweep_temp_files(dir, STALE_TEMP);
    }
}

/// Blob contents in a file next to `dest`, removed unless kept.
fn stage_bytes(dest: &Path, data: &[u8]) -> Result<TempFile> {
    use std::io::Write as _;
    let file = TempFile(Some(temp_next_to(dest)?));
    let path = file.0.as_deref().expect("a path");
    std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(data))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(file)
}

/// Removes the temporary files of `write_output` and `temp_next_to` in
/// `dir` older than `age`.
fn sweep_temp_files(dir: &Path, age: std::time::Duration) {
    let now = std::time::SystemTime::now();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().contains(TEMP_MARK)
            && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > age)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Blobs fetched for outputs, in files.
#[derive(Default)]
pub(super) struct Fetched {
    /// The file holding a blob, and whether it's still where it was
    /// fetched (next to an output it's for): moved to the first output
    /// written with it, which the others copy.
    files: HashMap<String, (PathBuf, bool)>,
}

impl Fetched {
    /// Fetches a blob into a file next to `dest`: `fill` writes it, and
    /// says whether it's whole and right.
    fn stage(&mut self, d: &re::Digest, dest: &Path, fill: impl FnOnce(&mut std::fs::File) -> bool) -> bool {
        let Ok(path) = temp_next_to(dest) else { return false };
        let filled = std::fs::File::options().write(true).create_new(true).open(&path).is_ok_and(|mut f| fill(&mut f));
        if filled {
            self.files.insert(d.hash.clone(), (path, true));
        } else {
            let _ = std::fs::remove_file(&path);
        }
        filled
    }

    /// A blob's contents, for the outputs read here (depfiles).
    pub(super) fn read(&self, d: &re::Digest) -> Result<Vec<u8>> {
        if d.size_bytes == 0 {
            return Ok(Vec::new());
        }
        let (path, _) = self.files.get(&d.hash).with_context(|| format!("missing blob {}", rbe_client::digest_str(d)))?;
        Ok(std::fs::read(path)?)
    }

    /// Writes blob `d` as the output `local`, atomically.
    pub(super) fn write(&mut self, local: &Path, d: &re::Digest, executable: bool) -> Result<()> {
        if d.size_bytes == 0 {
            return write_output(local, &[], executable);
        }
        let (src, staged) = self.files.get(&d.hash).cloned().with_context(|| format!("missing blob {}", rbe_client::digest_str(d)))?;
        if staged && move_output(&src, local, executable).is_ok() {
            self.files.insert(d.hash.clone(), (local.to_path_buf(), false));
            return Ok(());
        }
        write_output_with(local, executable, |f| std::io::copy(&mut std::fs::File::open(&src)?, f).map(drop))
    }
}

/// Files fetched and never moved to an output.
impl Drop for Fetched {
    fn drop(&mut self) {
        for (path, staged) in self.files.values() {
            if *staged {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Fetches a large blob from the remote into a file next to `dest`, and
/// stores it in the disk cache. As the chunks the remote splits it into
/// when it does, those the disk cache lacks, then assembled from the disk
/// cache; else as it arrives. Returns the file, removed if dropped (the
/// task stopped: a race lost, a build interrupted).
async fn fetch_into_file(
    client: &Client,
    disk: Option<&DiskCache>,
    d: &re::Digest,
    dest: &Path,
    verbose: bool,
    budget: &tokio::sync::Semaphore,
    warned: &std::sync::atomic::AtomicBool,
) -> Result<TempFile> {
    let temp = TempFile(Some(blocking(|| temp_next_to(dest))?));
    let path = temp.0.clone().expect("a path");
    if let Some(disk) = disk
        && client.chunking().split
    {
        match fetch_chunks(client, disk, d, &path, budget).await {
            Ok(true) => return Ok(temp),
            // Not split by the server, or not right: fetched whole.
            Ok(false) => {}
            Err(e) => {
                if verbose {
                    eprintln!("ninja-rs: fetching {} as chunks: {e:#}", rbe_client::digest_str(d));
                }
            }
        }
    }
    client.read_blob_to_file(d, &path).await?;
    if let Some(disk) = disk {
        // Checked as it was written: stored without hashing it again.
        blocking(|| disk_store(warned, || disk.put_file_with_digest(&path, d)));
    }
    Ok(temp)
}

/// A file removed when dropped, unless kept.
struct TempFile(Option<PathBuf>);

impl TempFile {
    fn keep(mut self) -> PathBuf {
        self.0.take().expect("kept once")
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Fetches a large blob into `path` as the chunks the remote splits it
/// into, downloading those the disk cache lacks a group at a time (within
/// the downloads' memory `budget`), and stores the layout in the disk
/// cache once the chunks are checked to make it up. False when the remote
/// can't split it, or its chunks aren't the blob.
async fn fetch_chunks(client: &Client, disk: &DiskCache, d: &re::Digest, path: &Path, budget: &tokio::sync::Semaphore) -> Result<bool> {
    let Some(split) = client.split_blob(d).await? else { return Ok(false) };
    let lacking: Vec<re::Digest> = blocking(|| split.chunk_digests.iter().filter(|c| !disk.contains_chunk(c)).cloned().collect());
    for chunks in grouped(lacking, |c| c.size_bytes, CHUNK_GROUP) {
        let _held = budget.acquire_many(budget_kib(chunks.iter().map(|c| c.size_bytes).sum())).await?;
        // Each read is checked to be its digest.
        let read = client.read_blobs(&chunks).await?;
        blocking(|| -> Result<()> {
            for c in &chunks {
                disk.put_chunk_bytes(c, read.get(&c.hash).with_context(|| format!("chunk {} wasn't read", rbe_client::digest_str(c)))?)?;
            }
            Ok(())
        })?;
    }
    Ok(blocking(|| std::fs::File::create(path).and_then(|mut f| disk.put_chunked_into(d, split, &mut f)))?)
}

/// Writes an output atomically, through a temporary file of its own:
/// tasks writing the same output at once (two downloading it, a rewound
/// producer and a consumer) each rename a whole file into place, and none
/// removes another's.
pub(super) fn write_output(path: &std::path::Path, data: &[u8], executable: bool) -> Result<()> {
    use std::io::Write as _;
    write_output_with(path, executable, |f| f.write_all(data))
}

/// What names a temporary file next to an output.
const TEMP_MARK: &str = ".ninja-rs-tmp-";

/// A file next to `path` (its directory created) no other writer uses.
fn temp_next_to(path: &Path) -> std::io::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(path.with_extension(format!("{ext}{TEMP_MARK}{}", crate::disk_cache::unique_name())))
}

/// Moves a fetched file into place as an output.
fn move_output(src: &Path, path: &Path, executable: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(src, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    std::fs::rename(src, path)
}

/// `write_output`, with the contents `fill` writes.
fn write_output_with(path: &std::path::Path, executable: bool, fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>) -> Result<()> {
    let tmp = temp_next_to(path)?;
    let written = std::fs::File::options().write(true).create_new(true).open(&tmp).and_then(|mut f| fill(&mut f));
    #[cfg(unix)]
    let written = written.and_then(|()| {
        use std::os::unix::fs::PermissionsExt;
        if executable { std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)) } else { Ok(()) }
    });
    #[cfg(not(unix))]
    let _ = executable;
    // Windows doesn't replace a file opened without FILE_SHARE_DELETE:
    // removing it first may still work.
    let placed = written.and_then(|()| std::fs::rename(&tmp, path).or_else(|_| std::fs::remove_file(path).and_then(|()| std::fs::rename(&tmp, path))));
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::from(e).context(format!("writing {}", path.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tasks writing one output at once (a failed claim's waiters, a
    /// rewound producer): the file ends whole, and no write fails.
    #[test]
    fn concurrent_output_writes() {
        let dir = test_dirs::base().join(format!("ninja-rs-outputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("obj").join("a.o");
        for _ in 0..50 {
            std::thread::scope(|s| {
                for _ in 0..3 {
                    s.spawn(|| write_output(&out, b"object", false).unwrap());
                }
            });
            assert_eq!(std::fs::read(&out).unwrap(), b"object");
        }
        assert_eq!(std::fs::read_dir(dir.join("obj")).unwrap().count(), 1, "temporary files left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Temporary files interrupted writes left next to outputs go once
    /// old; those of writes going on, and outputs, stay.
    #[test]
    fn sweeps_old_temporary_files() {
        let dir = std::env::temp_dir().join(format!("ninja-rs-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = temp_next_to(&dir.join("a.o")).unwrap();
        let fresh = temp_next_to(&dir.join("b.o")).unwrap();
        for f in [&old, &fresh, &dir.join("a.o")] {
            std::fs::write(f, "x").unwrap();
        }
        let past = std::time::SystemTime::now() - 2 * STALE_TEMP;
        for f in [&old, &dir.join("a.o")] {
            std::fs::File::options().write(true).open(f).unwrap().set_modified(past).unwrap();
        }
        sweep_temp_files(&dir, STALE_TEMP);
        assert!(!old.exists() && fresh.exists() && dir.join("a.o").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_groups() {
        let sizes = |groups: Vec<Vec<i64>>| groups.iter().map(|g| g.iter().sum::<i64>()).collect::<Vec<_>>();
        assert_eq!(sizes(grouped(vec![3, 3, 3, 10, 1], |s| *s, 6)), [6, 3, 10, 1]);
        assert!(grouped(Vec::<i64>::new(), |s| *s, 6).is_empty());
        assert_eq!((budget_kib(0), budget_kib(1025), budget_kib(i64::MAX)), (1, 2, DOWNLOAD_BUDGET_KIB as u32));
    }
}
