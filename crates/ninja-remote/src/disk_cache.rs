//! The local disk cache (`--disk_cache`): an action cache and a CAS on disk,
//! with the same keys as a REAPI service (action digests, blob digests), and
//! entries in the same form (paths relative to the action's working
//! directory), so that they are interchangeable with a remote cache's.
//!
//! Layout, under the cache directory:
//!
//! ```text
//! VERSION              format version ("1")
//! cas/<hh>/<hash>      blobs and chunks (<hh>: the hash's first two hex digits)
//! chunked/<hh>/<hash>  layouts of chunked blobs: SplitBlobResponse messages
//! ac/<hh>/<hash>       ActionResult messages, by action digest
//! tmp/                 files being written (renamed into place when done)
//! size                 estimated total size of cas/, chunked/ and ac/, in bytes
//! gc/lock              held while collecting garbage
//! cpp-deps/            compiler profiles of the preprocessor (`cpp_deps::driver`),
//!                      which removes those unused for a month itself
//! ```
//!
//! Content-defined chunking: a blob larger than [`CHUNK_MAX`] is stored as
//! its FastCDC 2020 chunks (REAPI's `FAST_CDC_2020` with its recommended
//! parameters, so that chunks match those of a server that splits blobs the
//! same way), each a blob of `cas/`, plus its layout, the list of their
//! digests, in `chunked/`. Blobs that differ by a few edits, such as two
//! builds of an executable, then share most of their chunks, and the cache
//! stores those once. A blob downloaded from a remote cache that splits
//! blobs keeps the remote's chunks ([`DiskCache::put_chunked`]). Every blob goes through [`DiskCache::read_blob`] and
//! the `put_*` methods, which split and reassemble them: callers don't see
//! chunks. A blob is present when it is stored whole or when its layout and
//! all its chunks are, so a layout may outlive some of its chunks.
//!
//! Eviction is least recently used, as in Bazel's disk cache: reads update
//! a file's modification time, and [`DiskCache::collect_garbage`] removes
//! the oldest files once the cache exceeds its size limit. Using a chunked
//! blob marks its layout and its chunks, so a chunk shared by several blobs
//! stays as long as one of them is used. Finding the oldest files walks the
//! whole cache, so it only happens when the `size` estimate (bumped by each
//! process's writes) passes the limit; the walk then rewrites the estimate
//! with the real size. One process collects at a time (`gc/lock`); a file
//! used while the walk was going on is kept.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use prost::Message;
use rbe_client::FastCdc;
use rbe_client::proto as re;
use rbe_client::tree::DigestCache;
use sha2::{Digest as _, Sha256};

const VERSION: &str = "1";

/// How blobs are split: REAPI's recommended FastCDC 2020 parameters.
const CHUNKER: FastCdc = FastCdc::RECOMMENDED;
/// Largest chunk. Blobs up to this size are stored whole, as REAPI advises.
pub const CHUNK_MAX: u32 = CHUNKER.avg * 4;

const FAST_CDC: i32 = re::chunking_function::Value::FastCdc2020 as i32;

/// Whether a blob is stored as chunks.
pub fn is_chunked(d: &re::Digest) -> bool {
    CHUNKER.splits(d.size_bytes)
}

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
                return Err(std::io::Error::other(format!("{} has format version {}, this ninja-rs uses {VERSION}", root.display(), v.trim())));
            }
            Err(_) => {
                std::fs::create_dir_all(root)?;
                std::fs::write(&version, format!("{VERSION}\n"))?;
            }
        }
        for d in ["cas", "chunked", "ac", "tmp", "gc"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        Ok(Self { root: root.to_path_buf(), written: AtomicU64::new(0), counter: AtomicU64::new(0) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the preprocessor saves compiler profiles.
    pub fn profile_dir(&self) -> PathBuf {
        self.root.join("cpp-deps")
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        shard(&self.root.join("cas"), hash)
    }

    fn layout_path(&self, hash: &str) -> PathBuf {
        shard(&self.root.join("chunked"), hash)
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
        if d.size_bytes == 0 || self.contains_whole(d) {
            return true;
        }
        self.layout(d).is_some_and(|chunks| chunks.iter().all(|c| self.contains_whole(c)))
    }

    /// Whether `cas/` holds the blob as a single file (marking it used).
    fn contains_whole(&self, d: &re::Digest) -> bool {
        let p = self.blob_path(&d.hash);
        match std::fs::metadata(&p) {
            Ok(m) if m.len() as i64 == d.size_bytes => {
                touch(&p);
                true
            }
            _ => false,
        }
    }

    /// The digests of a chunked blob's chunks, if its layout is stored
    /// (marking it used).
    fn layout(&self, d: &re::Digest) -> Option<Vec<re::Digest>> {
        if !is_chunked(d) {
            return None;
        }
        let p = self.layout_path(&d.hash);
        let layout = re::SplitBlobResponse::decode(std::fs::read(&p).ok()?.as_slice()).ok()?;
        if layout.chunk_digests.iter().map(|c| c.size_bytes).sum::<i64>() != d.size_bytes {
            return None;
        }
        touch(&p);
        Some(layout.chunk_digests)
    }

    /// The blob's contents, if the CAS holds it.
    pub fn read_blob(&self, d: &re::Digest) -> Option<Vec<u8>> {
        if d.size_bytes == 0 {
            return Some(Vec::new());
        }
        if let Some(data) = self.read_whole(d) {
            return Some(data);
        }
        let chunks = self.layout(d)?;
        let mut data = Vec::with_capacity(d.size_bytes as usize);
        for c in &chunks {
            data.extend_from_slice(&self.read_whole(c)?);
        }
        Some(data)
    }

    fn read_whole(&self, d: &re::Digest) -> Option<Vec<u8>> {
        let p = self.blob_path(&d.hash);
        let data = std::fs::read(&p).ok()?;
        if data.len() as i64 != d.size_bytes {
            return None;
        }
        touch(&p);
        Some(data)
    }

    /// Stores a blob (no-op when present), as chunks when it's large.
    pub fn put_bytes(&self, d: &re::Digest, data: &[u8]) -> std::io::Result<()> {
        if d.size_bytes == 0 || self.contains_blob(d) {
            return Ok(());
        }
        if !is_chunked(d) {
            return self.write_atomic(&self.blob_path(&d.hash), data);
        }
        let mut chunks = Vec::new();
        for chunk in CHUNKER.chunks(data) {
            chunks.push(self.put_chunk(chunk)?);
        }
        self.put_layout(d, re::SplitBlobResponse { chunk_digests: chunks, chunking_function: FAST_CDC })
    }

    /// Stores a file's contents, returning their digest. A large file is
    /// split as it's read, without loading it whole.
    pub fn put_file(&self, path: &Path, digests: Option<&dyn DigestCache>) -> anyhow::Result<re::Digest> {
        let d = rbe_client::tree::file_digest(path, digests)?;
        if self.contains_blob(&d) {
            return Ok(d);
        }
        if !is_chunked(&d) {
            let data = std::fs::read(path)?;
            anyhow::ensure!(data.len() as i64 == d.size_bytes, "{} changed while being stored", path.display());
            self.write_atomic(&self.blob_path(&d.hash), &data)?;
            return Ok(d);
        }
        let file = std::fs::File::open(path)?;
        let mut whole = Sha256::new();
        let mut chunks = Vec::new();
        for c in CHUNKER.stream(file) {
            let c = c.map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            whole.update(&c.data);
            chunks.push(self.put_chunk(&c.data)?);
        }
        // The layout must describe the blob `d` names: check the file
        // didn't change since it was hashed.
        anyhow::ensure!(hex::encode(whole.finalize()) == d.hash, "{} changed while being stored", path.display());
        self.put_layout(&d, re::SplitBlobResponse { chunk_digests: chunks, chunking_function: FAST_CDC })?;
        Ok(d)
    }

    fn put_chunk(&self, chunk: &[u8]) -> std::io::Result<re::Digest> {
        let d = rbe_client::digest_of(chunk);
        if !self.contains_whole(&d) {
            self.write_atomic(&self.blob_path(&d.hash), chunk)?;
        }
        Ok(d)
    }

    /// Records a blob's layout. Its chunks must be stored first.
    fn put_layout(&self, d: &re::Digest, layout: re::SplitBlobResponse) -> std::io::Result<()> {
        self.write_atomic(&self.layout_path(&d.hash), &layout.encode_to_vec())
    }

    // Chunks of a blob split elsewhere (by a remote cache), for downloads
    // that only fetch the chunks the cache lacks.

    /// Whether the cache holds a chunk (marking it used).
    pub fn contains_chunk(&self, d: &re::Digest) -> bool {
        d.size_bytes == 0 || self.contains_whole(d)
    }

    /// A chunk's contents.
    pub fn read_chunk(&self, d: &re::Digest) -> Option<Vec<u8>> {
        if d.size_bytes == 0 { Some(Vec::new()) } else { self.read_whole(d) }
    }

    /// Stores a chunk, whatever its size. Its digest must have been checked.
    pub fn put_chunk_bytes(&self, d: &re::Digest, data: &[u8]) -> std::io::Result<()> {
        if self.contains_chunk(d) {
            return Ok(());
        }
        self.write_atomic(&self.blob_path(&d.hash), data)
    }

    /// Records how a large blob (see [`is_chunked`]) was split. Its chunks
    /// must be stored first, and have been checked to make it up.
    pub fn put_chunked(&self, d: &re::Digest, split: re::SplitBlobResponse) -> std::io::Result<()> {
        debug_assert!(is_chunked(d));
        self.put_layout(d, split)
    }

    /// The action's result, if cached and if the CAS still holds all its
    /// outputs (eviction may have removed some).
    pub fn get_action_result(&self, action: &re::Digest) -> Option<re::ActionResult> {
        self.get_action_result_and_absent(action).filter(|(_, absent)| absent.is_empty()).map(|(r, _)| r)
    }

    /// An action's result, and the blobs it references that the cache
    /// doesn't hold: outputs left in a remote CAS (minimal downloads).
    pub fn get_action_result_and_absent(&self, action: &re::Digest) -> Option<(re::ActionResult, Vec<re::Digest>)> {
        let p = self.ac_path(&action.hash);
        let data = std::fs::read(&p).ok()?;
        let result = re::ActionResult::decode(data.as_slice()).ok()?;
        let blobs = result.output_files.iter().filter_map(|f| f.digest.as_ref()).chain(result.stdout_digest.as_ref()).chain(result.stderr_digest.as_ref());
        // The blobs are refreshed after the result, so that eviction, which
        // removes results first among files of the same age, never leaves a
        // result whose blobs it removed.
        touch(&p);
        let absent = blobs.filter(|d| d.size_bytes > 0 && !self.contains_blob(d)).cloned().collect();
        Some((result, absent))
    }

    /// Forgets an action's result (its outputs proved gone).
    pub fn remove_action_result(&self, action: &re::Digest) {
        let _ = std::fs::remove_file(self.ac_path(&action.hash));
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
        let mut files: Vec<(SystemTime, usize, u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        // Of files of the same age, action results go first, then layouts,
        // then blobs, so that a kept entry rarely lacks what it refers to.
        for (rank, top) in ["ac", "chunked", "cas"].into_iter().enumerate() {
            let Ok(shards) = std::fs::read_dir(self.root.join(top)) else { continue };
            for shard in shards.flatten() {
                let Ok(entries) = std::fs::read_dir(shard.path()) else { continue };
                for e in entries.flatten() {
                    if let Ok(m) = e.metadata()
                        && m.is_file()
                    {
                        total += m.len();
                        files.push((m.modified().unwrap_or(SystemTime::UNIX_EPOCH), rank, m.len(), e.path()));
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
        let result =
            re::ActionResult { output_files: vec![re::OutputFile { path: "a.o".into(), digest: Some(d.clone()), ..Default::default() }], ..Default::default() };
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

    /// Incompressible, reproducible data (xorshift).
    fn noise(len: usize, mut state: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn stored_layout(c: &DiskCache, d: &re::Digest) -> Vec<re::Digest> {
        let data = std::fs::read(c.layout_path(&d.hash)).unwrap();
        re::SplitBlobResponse::decode(data.as_slice()).unwrap().chunk_digests
    }

    /// Ages every file of the cache, as if last used long ago.
    fn age_all(c: &DiskCache) {
        let past = SystemTime::now() - std::time::Duration::from_secs(10_000);
        for top in ["cas", "chunked", "ac"] {
            for shard in std::fs::read_dir(c.root.join(top)).unwrap().flatten() {
                for e in std::fs::read_dir(shard.path()).unwrap().flatten() {
                    std::fs::File::options().write(true).open(e.path()).unwrap().set_modified(past).unwrap();
                }
            }
        }
    }

    #[test]
    fn large_blobs_share_chunks() {
        let (c, dir) = cache("chunks");
        let a = noise(8 << 20, 1);
        let da = rbe_client::digest_of(&a);
        c.put_bytes(&da, &a).unwrap();
        assert!(!c.blob_path(&da.hash).exists());
        let chunks_a = stored_layout(&c, &da);
        assert!(chunks_a.len() > 4);
        assert!(chunks_a.iter().all(|d| d.size_bytes <= i64::from(CHUNK_MAX)));
        assert_eq!(c.read_blob(&da).unwrap(), a);
        assert!(c.contains_blob(&da));

        // An insertion only changes the chunks around it.
        let mut b = a.clone();
        b.splice(4 << 20..4 << 20, noise(100, 2));
        let db = rbe_client::digest_of(&b);
        let before = c.bytes_written();
        c.put_bytes(&db, &b).unwrap();
        assert!(c.bytes_written() - before < 3 * u64::from(CHUNK_MAX), "{} bytes written", c.bytes_written() - before);
        assert_eq!(c.read_blob(&db).unwrap(), b);

        // A blob lacking a chunk is a miss; the other one is unaffected.
        let only_b = stored_layout(&c, &db).into_iter().find(|d| !chunks_a.contains(d)).unwrap();
        std::fs::remove_file(c.blob_path(&only_b.hash)).unwrap();
        assert!(!c.contains_blob(&db));
        assert!(c.read_blob(&db).is_none());
        assert_eq!(c.read_blob(&da).unwrap(), a);
        // Storing it again restores the chunk.
        c.put_bytes(&db, &b).unwrap();
        assert_eq!(c.read_blob(&db).unwrap(), b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_split_like_bytes() {
        let (c, dir) = cache("chunked-file");
        let data = noise(5 << 20, 3);
        let f = dir.join("big.bin");
        std::fs::write(&f, &data).unwrap();
        let d = c.put_file(&f, None).unwrap();
        assert_eq!(d, rbe_client::digest_of(&data));
        let expected: Vec<_> = CHUNKER.chunks(&data).map(rbe_client::digest_of).collect();
        assert_eq!(stored_layout(&c, &d), expected);
        assert_eq!(c.read_blob(&d).unwrap(), data);
        // A blob at the limit is stored whole.
        let small = noise(CHUNK_MAX as usize, 4);
        std::fs::write(&f, &small).unwrap();
        let d = c.put_file(&f, None).unwrap();
        assert!(c.blob_path(&d.hash).exists() && !c.layout_path(&d.hash).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicts_chunks_no_longer_used() {
        let (c, dir) = cache("chunk-evict");
        let a = noise(6 << 20, 5);
        let da = rbe_client::digest_of(&a);
        c.put_bytes(&da, &a).unwrap();
        age_all(&c);
        // Storing a similar blob marks the chunks it shares with `a` as used.
        let mut b = a.clone();
        b.splice(3 << 20..3 << 20, noise(100, 6));
        let db = rbe_client::digest_of(&b);
        c.put_bytes(&db, &b).unwrap();
        let (removed, _) = c.collect_garbage(1, db.size_bytes as u64 + 4096).unwrap().unwrap();
        assert!(removed >= 2);
        assert!(!c.contains_blob(&da));
        assert_eq!(c.read_blob(&db).unwrap(), b);
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
