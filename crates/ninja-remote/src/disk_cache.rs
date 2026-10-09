//! The local disk cache (`--disk_cache`): an action cache and a CAS on disk,
//! with the same keys as a REAPI service (action digests, blob digests), and
//! entries in the same form (paths relative to the action's working
//! directory), so that they are interchangeable with a remote cache's.
//!
//! Layout, under the cache directory:
//!
//! ```text
//! VERSION              format version ("1")
//! cas/<hh>/<hash>      blobs (<hh>: the hash's first two hex digits)
//! ac/<hh>/<hash>       ActionResult messages, by action digest
//! tmp/                 files being written (renamed into place when done)
//! size                 estimated total size of cas/ and ac/, in bytes
//! gc/lock              held while collecting garbage
//! ```
//!
//! Every blob goes through [`DiskCache::read_blob`] and the `put_*`
//! methods. Blobs are stored whole today; with content-defined chunking
//! (REAPI `SplitBlob` / `SpliceBlob`), a large blob will be stored as the
//! list of its chunks, themselves blobs of this CAS, and these methods will
//! split and reassemble them, so that callers don't change and a download
//! only fetches the chunks the cache lacks. Eviction then works on chunks.
//!
//! Eviction is least recently used, as in Bazel's disk cache: reads update
//! a file's modification time, and [`DiskCache::collect_garbage`] removes
//! the oldest files once the cache exceeds its size limit. Finding them
//! walks the whole cache, so it only happens when the `size` estimate
//! (bumped by each process's writes) passes the limit; the walk then
//! rewrites the estimate with the real size. One process collects at a
//! time (`gc/lock`); a file used while the walk was going on is kept.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use prost::Message;
use rbe_client::proto as re;
use rbe_client::tree::DigestCache;

const VERSION: &str = "1";

pub struct DiskCache {
    root: PathBuf,
    /// Bytes written by this process (eviction only runs after writes).
    written: AtomicU64,
    counter: AtomicU64,
}

fn shard(dir: &Path, hash: &str) -> PathBuf {
    dir.join(&hash[..2.min(hash.len())]).join(hash)
}

/// Marks a file as recently used.
fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

impl DiskCache {
    /// Opens (creating it if needed) the cache at `root`.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let version = root.join("VERSION");
        match std::fs::read_to_string(&version) {
            Ok(v) if v.trim() == VERSION => {}
            Ok(v) => {
                return Err(std::io::Error::other(format!(
                    "{} has format version {}, this ninja-rs uses {VERSION}",
                    root.display(),
                    v.trim()
                )));
            }
            Err(_) => {
                std::fs::create_dir_all(root)?;
                std::fs::write(&version, format!("{VERSION}\n"))?;
            }
        }
        for d in ["cas", "ac", "tmp", "gc"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        Ok(Self { root: root.to_path_buf(), written: AtomicU64::new(0), counter: AtomicU64::new(0) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        shard(&self.root.join("cas"), hash)
    }

    fn ac_path(&self, hash: &str) -> PathBuf {
        shard(&self.root.join("ac"), hash)
    }

    /// Writes `data` to `dest` atomically (through a file in `tmp/`).
    fn write_atomic(&self, dest: &Path, data: &[u8]) -> std::io::Result<()> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join("tmp").join(format!("{}-{n}", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Err(e) = std::fs::rename(&tmp, dest) {
            let _ = std::fs::remove_file(&tmp);
            // Another process stored the same blob meanwhile.
            if !dest.exists() {
                return Err(e);
            }
        }
        self.written.fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Whether the CAS holds the blob (marking it used).
    pub fn contains_blob(&self, d: &re::Digest) -> bool {
        if d.size_bytes == 0 {
            return true;
        }
        let p = self.blob_path(&d.hash);
        match std::fs::metadata(&p) {
            Ok(m) if m.len() as i64 == d.size_bytes => {
                touch(&p);
                true
            }
            _ => false,
        }
    }

    /// The blob's contents, if the CAS holds it.
    pub fn read_blob(&self, d: &re::Digest) -> Option<Vec<u8>> {
        if d.size_bytes == 0 {
            return Some(Vec::new());
        }
        let p = self.blob_path(&d.hash);
        let data = std::fs::read(&p).ok()?;
        if data.len() as i64 != d.size_bytes {
            return None;
        }
        touch(&p);
        Some(data)
    }

    /// Stores a blob (no-op when present).
    pub fn put_bytes(&self, d: &re::Digest, data: &[u8]) -> std::io::Result<()> {
        if d.size_bytes == 0 || self.contains_blob(d) {
            return Ok(());
        }
        self.write_atomic(&self.blob_path(&d.hash), data)
    }

    /// Stores a file's contents, returning their digest.
    pub fn put_file(&self, path: &Path, digests: Option<&dyn DigestCache>) -> anyhow::Result<re::Digest> {
        let d = rbe_client::tree::file_digest(path, digests)?;
        if !self.contains_blob(&d) {
            let data = std::fs::read(path)?;
            self.put_bytes(&d, &data)?;
        }
        Ok(d)
    }

    /// The action's result, if cached and if the CAS still holds all its
    /// outputs (eviction may have removed some).
    pub fn get_action_result(&self, action: &re::Digest) -> Option<re::ActionResult> {
        let p = self.ac_path(&action.hash);
        let data = std::fs::read(&p).ok()?;
        let result = re::ActionResult::decode(data.as_slice()).ok()?;
        let blobs = result
            .output_files
            .iter()
            .filter_map(|f| f.digest.as_ref())
            .chain(result.stdout_digest.as_ref())
            .chain(result.stderr_digest.as_ref());
        for d in blobs {
            if !self.contains_blob(d) {
                return None;
            }
        }
        touch(&p);
        Some(result)
    }

    /// Records an action's result. Its blobs must be stored first.
    pub fn put_action_result(&self, action: &re::Digest, result: &re::ActionResult) -> std::io::Result<()> {
        self.write_atomic(&self.ac_path(&action.hash), &result.encode_to_vec())
    }

    pub fn bytes_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    fn size_path(&self) -> PathBuf {
        self.root.join("size")
    }

    fn read_size_estimate(&self) -> Option<u64> {
        std::fs::read_to_string(self.size_path()).ok()?.trim().parse().ok()
    }

    fn write_size_estimate(&self, size: u64) -> std::io::Result<()> {
        self.write_meta(&self.size_path(), format!("{size}\n").as_bytes())
    }

    /// Like `write_atomic`, for the cache's own files (not counted as data).
    fn write_meta(&self, dest: &Path, data: &[u8]) -> std::io::Result<()> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join("tmp").join(format!("{}-{n}", std::process::id()));
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, dest).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// Keeps the cache within `max` bytes: adds this process's writes to the
    /// size estimate and, when that passes `max`, walks the cache to remove
    /// the least recently used files until it takes at most `target` bytes.
    /// Returns the number of files and bytes removed, or `None` when another
    /// process is collecting (this one's writes are counted next time).
    pub fn collect_garbage(&self, max: u64, target: u64) -> std::io::Result<Option<(usize, u64)>> {
        let lock = std::fs::File::options().create(true).truncate(false).write(true).open(self.root.join("gc").join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        let written = self.written.swap(0, Ordering::Relaxed);
        if let Some(estimate) = self.read_size_estimate().map(|s| s + written)
            && estimate <= max
        {
            self.write_size_estimate(estimate)?;
            return Ok(Some((0, 0)));
        }
        // Over the limit, or no estimate yet: walk the cache.
        let mut files: Vec<(SystemTime, bool, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        for top in ["ac", "cas"] {
            let Ok(shards) = std::fs::read_dir(self.root.join(top)) else { continue };
            for shard in shards.flatten() {
                let Ok(entries) = std::fs::read_dir(shard.path()) else { continue };
                for e in entries.flatten() {
                    if let Ok(m) = e.metadata()
                        && m.is_file()
                    {
                        total += m.len();
                        // Action results before blobs of the same age, so
                        // that a kept result rarely lacks its outputs.
                        files.push((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), top == "cas", m.len(), e.path()));
                    }
                }
            }
        }
        let (mut removed, mut freed) = (0usize, 0u64);
        if total > max {
            files.sort();
            for (mtime, _, len, path) in files {
                if total - freed <= target {
                    break;
                }
                // Used by a build since the walk: keep it.
                if std::fs::metadata(&path).and_then(|m| m.modified()).ok() != Some(mtime) {
                    continue;
                }
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                    freed += len;
                }
            }
        }
        self.write_size_estimate(total - freed)?;
        Ok(Some((removed, freed)))
    }
}

/// Parses a size such as `10G`, `512M`, `64k` or `1000` (bytes).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last()? {
        (i, 'k' | 'K') => (&s[..i], 1u64 << 10),
        (i, 'm' | 'M') => (&s[..i], 1 << 20),
        (i, 'g' | 'G') => (&s[..i], 1 << 30),
        (i, 't' | 'T') => (&s[..i], 1 << 40),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(name: &str) -> (DiskCache, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ninja-rs-disk-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (DiskCache::open(&dir).unwrap(), dir)
    }

    #[test]
    fn blobs_and_action_results() {
        let (c, dir) = cache("roundtrip");
        let data = b"hello".to_vec();
        let d = rbe_client::digest_of(&data);
        assert!(c.read_blob(&d).is_none());
        c.put_bytes(&d, &data).unwrap();
        assert_eq!(c.read_blob(&d).unwrap(), data);
        assert!(c.contains_blob(&d));
        // A wrong size is a miss.
        assert!(c.read_blob(&re::Digest { hash: d.hash.clone(), size_bytes: 3 }).is_none());

        let action = rbe_client::digest_of(b"action");
        let result = re::ActionResult {
            output_files: vec![re::OutputFile { path: "a.o".into(), digest: Some(d.clone()), ..Default::default() }],
            ..Default::default()
        };
        c.put_action_result(&action, &result).unwrap();
        assert_eq!(c.get_action_result(&action).unwrap(), result);
        // Without its output, the result is a miss.
        std::fs::remove_file(c.blob_path(&d.hash)).unwrap();
        assert!(c.get_action_result(&action).is_none());

        // A file's digest and contents.
        let f = dir.join("input.txt");
        std::fs::write(&f, "file contents").unwrap();
        let fd = c.put_file(&f, None).unwrap();
        assert_eq!(c.read_blob(&fd).unwrap(), b"file contents");
        assert!(c.bytes_written() >= 5 + 13);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicts_least_recently_used() {
        let (c, dir) = cache("evict");
        let blobs: Vec<_> = (0..4u8)
            .map(|i| {
                let data = vec![i; 1000];
                let d = rbe_client::digest_of(&data);
                c.put_bytes(&d, &data).unwrap();
                let past = SystemTime::now() - std::time::Duration::from_secs(1000 - u64::from(i) * 100);
                std::fs::File::options().write(true).open(c.blob_path(&d.hash)).unwrap().set_modified(past).unwrap();
                d
            })
            .collect();
        // Reading the oldest makes it the most recently used.
        assert!(c.read_blob(&blobs[0]).is_some());
        assert_eq!(c.collect_garbage(10_000, 5_000).unwrap(), Some((0, 0)));
        assert_eq!(c.read_size_estimate(), Some(4000));
        let (removed, freed) = c.collect_garbage(3_500, 2_000).unwrap().unwrap();
        assert_eq!((removed, freed), (2, 2000));
        assert_eq!(c.read_size_estimate(), Some(2000));
        assert!(c.contains_blob(&blobs[0]) && c.contains_blob(&blobs[3]));
        assert!(!c.contains_blob(&blobs[1]) && !c.contains_blob(&blobs[2]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walks_only_when_the_estimate_passes_the_limit() {
        let (c, dir) = cache("estimate");
        let small = vec![1u8; 100];
        c.put_bytes(&rbe_client::digest_of(&small), &small).unwrap();
        assert_eq!(c.collect_garbage(1000, 900).unwrap(), Some((0, 0)));
        assert_eq!(c.read_size_estimate(), Some(100));
        // A file the estimate doesn't know about (another process's, say)
        // isn't seen until the estimate passes the limit.
        let big = vec![2u8; 2000];
        let big_digest = rbe_client::digest_of(&big);
        std::fs::create_dir_all(c.blob_path(&big_digest.hash).parent().unwrap()).unwrap();
        std::fs::write(c.blob_path(&big_digest.hash), &big).unwrap();
        assert_eq!(c.collect_garbage(1000, 900).unwrap(), Some((0, 0)));
        assert_eq!(c.read_size_estimate(), Some(100));
        // This process's writes push it over: the walk finds the real size.
        let more = vec![3u8; 950];
        c.put_bytes(&rbe_client::digest_of(&more), &more).unwrap();
        let (removed, _) = c.collect_garbage(1000, 900).unwrap().unwrap();
        assert!(removed >= 1);
        assert!(c.read_size_estimate().unwrap() <= 900);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_collector_at_a_time() {
        let (c, dir) = cache("lock");
        let data = vec![1u8; 10];
        c.put_bytes(&rbe_client::digest_of(&data), &data).unwrap();
        let other = std::fs::File::options().create(true).truncate(false).write(true).open(dir.join("gc").join("lock")).unwrap();
        other.lock().unwrap();
        assert_eq!(c.collect_garbage(0, 0).unwrap(), None);
        // The writes are kept for the next collection.
        assert_eq!(c.bytes_written(), 10);
        other.unlock().unwrap();
        assert!(c.collect_garbage(1000, 900).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_mismatch() {
        let (_, dir) = cache("version");
        std::fs::write(dir.join("VERSION"), "99\n").unwrap();
        assert!(DiskCache::open(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10G"), Some(10 << 30));
        assert_eq!(parse_size("512m"), Some(512 << 20));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
    }
}
