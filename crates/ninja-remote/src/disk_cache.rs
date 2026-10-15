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
//! A directory without `VERSION` (removed by a cleaner of old files) is
//! ours again when it has the `size` only this cache writes (after every
//! build that wrote to it), or nothing stored in its `ac/` and `cas/`:
//! `VERSION` is written back. Else, one with Bazel's layout (`ac/`, `cas/`,
//! and `tmp/`, `gc/`, but what this cache adds using it) is Bazel's
//! `--disk_cache`, which a shared `.bazelrc` names: it is used as it is,
//! without writing `VERSION` there, its garbage left to its owner, and the
//! build says so once (per invocation tree). Any other directory that isn't
//! empty (a mistyped path) isn't used.
//!
//! Content-defined chunking: a blob larger than [`CHUNK_MAX`] is stored as
//! its FastCDC 2020 chunks (REAPI's `FAST_CDC_2020` with its recommended
//! parameters, so that chunks match those of a server that splits blobs the
//! same way), each a blob of `cas/`, plus its layout, the list of their
//! digests, in `chunked/`. Blobs that differ by a few edits, such as two
//! builds of an executable, then share most of their chunks, and the cache
//! stores those once. A blob downloaded from a remote cache that splits
//! blobs keeps the remote's chunks ([`DiskCache::put_chunked_into`]). Every blob goes through [`DiskCache::read_blob`] and
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
    /// Another tool's cache (see the module's documentation): never
    /// collected.
    foreign: bool,
}

/// A file name no other writer uses: the PID, a counter and a random part.
/// The PID alone isn't enough: containers sharing a cache volume have the
/// same ones.
pub(crate) fn unique_name() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}-{:016x}", std::process::id(), N.fetch_add(1, Ordering::Relaxed), ninja_os::random_u64())
}

/// What a directory without `VERSION` holds (see the module's
/// documentation).
#[derive(Debug, PartialEq, Eq)]
enum Layout {
    /// Nothing: a new cache.
    Empty,
    /// This cache's, its `VERSION` gone.
    Ours,
    /// Bazel's.
    Bazel,
    /// Something else.
    Other,
}

/// The top-level entries of this cache's layout: Bazel's (`ac`, `cas`,
/// `tmp`, and its collector's `gc`), and those this cache adds, even to a
/// Bazel cache it uses.
const LAYOUT: &[&str] = &["VERSION", "ac", "cas", "chunked", "tmp", "gc", "size", "cpp-deps"];

fn layout(root: &Path) -> Layout {
    let Ok(entries) = std::fs::read_dir(root) else { return Layout::Empty };
    let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    // Whether a directory of entries holds one (in its shards).
    let holds_entries = |top: &str| {
        std::fs::read_dir(root.join(top))
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| !e.path().is_dir() || std::fs::read_dir(e.path()).is_ok_and(|mut s| s.next().is_some()))
    };
    if names.is_empty() {
        Layout::Empty
    } else if !names.iter().all(|n| LAYOUT.contains(&n.as_str())) || !names.iter().any(|n| n == "ac" || n == "cas") {
        Layout::Other
    } else if names.iter().any(|n| n == "size") {
        Layout::Ours
    } else if !holds_entries("ac") && !holds_entries("cas") {
        // Nothing stored yet (by this cache, killed before it wrote its
        // `size`, or by Bazel): a new cache.
        Layout::Empty
    } else {
        Layout::Bazel
    }
}

/// The disk caches the invocations running this one said were Bazel's
/// (see `note_bazel_cache`), which its commands get: the invocations they
/// run (CMake's `try_compile`s, a nested build) don't say it again. Paths,
/// separated as in `PATH`.
pub const NOTED_ENV: &str = "NINJA_RS_BAZEL_DISK_CACHES";

/// The disk caches the invocations running this one said were Bazel's.
fn noted_by_parents() -> Vec<PathBuf> {
    std::env::split_paths(&std::env::var_os(NOTED_ENV).unwrap_or_default()).filter(|p| !p.as_os_str().is_empty()).collect()
}

/// Says once per invocation tree that the cache in `root` is Bazel's: not
/// when one running this one (`noted`) did, nor again in this process.
/// Whether it said it.
fn note_bazel_cache(root: &Path, noted: &[PathBuf]) -> bool {
    static NOTED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    let mut here = NOTED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if here.iter().chain(noted).any(|r| r == root) {
        return false;
    }
    here.push(root.to_path_buf());
    ninja_core::status::message(&format!(
        "note: the disk cache {} is Bazel's (no VERSION): used as it is, its old entries left to Bazel to remove. \
         (If it's ninja-rs's, writing {VERSION} to its VERSION file makes it so.)",
        root.display()
    ));
    true
}

/// How old a file in `tmp/` must be for garbage collection to remove it:
/// one left by a writer that died.
const STALE_TMP: std::time::Duration = std::time::Duration::from_secs(3600);

/// Where a blob or result is stored: none for a malformed hash, which
/// would name a file elsewhere (`../`, an absolute path), from a result a
/// server or another cache writer made.
fn shard(dir: &Path, hash: &str) -> Option<PathBuf> {
    rbe_client::valid_hash(hash).then(|| dir.join(&hash[..2]).join(hash))
}

fn malformed(hash: &str) -> std::io::Error {
    std::io::Error::other(format!("malformed digest {hash:?}"))
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
        let mut foreign = false;
        match std::fs::read_to_string(&version) {
            Ok(v) if v.trim() == VERSION => {}
            Ok(v) => {
                return Err(std::io::Error::other(format!("{} has format version {}, this ninja-rs uses {VERSION}", root.display(), v.trim())));
            }
            Err(_) => match layout(root) {
                Layout::Empty | Layout::Ours => {
                    std::fs::create_dir_all(root)?;
                    std::fs::write(&version, format!("{VERSION}\n"))?;
                }
                Layout::Bazel => {
                    foreign = true;
                    note_bazel_cache(root, &noted_by_parents());
                }
                Layout::Other => {
                    return Err(std::io::Error::other(format!("{} isn't empty, and isn't a disk cache", root.display())));
                }
            },
        }
        let dirs: &[&str] = if foreign { &["cas", "chunked", "ac", "tmp"] } else { &["cas", "chunked", "ac", "tmp", "gc"] };
        for d in dirs {
            std::fs::create_dir_all(root.join(d))?;
        }
        Ok(Self { root: root.to_path_buf(), written: AtomicU64::new(0), foreign })
    }

    /// Whether it's another tool's cache, used without collecting garbage.
    pub fn is_foreign(&self) -> bool {
        self.foreign
    }

    /// `NOTED_ENV` for the commands of an invocation using this cache, when
    /// it's Bazel's (said then): with the caches those running it said were.
    pub fn noted_env(&self) -> Option<String> {
        if !self.foreign {
            return None;
        }
        let mut noted = noted_by_parents();
        if !noted.contains(&self.root) {
            noted.push(self.root.clone());
        }
        std::env::join_paths(noted).ok().and_then(|v| v.into_string().ok())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the preprocessor saves compiler profiles.
    pub fn profile_dir(&self) -> PathBuf {
        self.root.join("cpp-deps")
    }

    fn blob_path(&self, hash: &str) -> Option<PathBuf> {
        shard(&self.root.join("cas"), hash)
    }

    fn layout_path(&self, hash: &str) -> Option<PathBuf> {
        shard(&self.root.join("chunked"), hash)
    }

    fn ac_path(&self, hash: &str) -> Option<PathBuf> {
        shard(&self.root.join("ac"), hash)
    }

    /// Writes `data` to `dest` atomically, through a file in `tmp/` no
    /// other writer opens (`create_new`: never another's, truncated).
    fn write_atomic(&self, dest: &Path, data: &[u8]) -> std::io::Result<()> {
        let tmp = self.root.join("tmp").join(unique_name());
        let written = std::fs::File::options().write(true).create_new(true).open(&tmp).and_then(|mut f| f.write_all(data));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
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
        let Some(p) = self.blob_path(&d.hash) else { return false };
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
        let p = self.layout_path(&d.hash)?;
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
        // The layout names the chunks; the whole must still be the blob.
        if rbe_client::verify(d, &data).is_err() {
            self.drop_layout(d);
            return None;
        }
        Some(data)
    }

    /// Writes the blob's contents to `out` a chunk at a time: a large blob
    /// is never whole in memory. False when the CAS doesn't hold it, or it
    /// isn't its digest: what was written is then to be discarded.
    pub fn read_blob_into(&self, d: &re::Digest, out: &mut dyn Write) -> bool {
        if d.size_bytes == 0 {
            return true;
        }
        if let Some(data) = self.read_whole(d) {
            return out.write_all(&data).is_ok();
        }
        let Some(chunks) = self.layout(d) else { return false };
        match self.assemble(&chunks, d, out) {
            Some(true) => true,
            Some(false) => {
                self.drop_layout(d);
                false
            }
            None => false,
        }
    }

    /// Writes chunks to `out`: whether they make up the blob `d`, or None
    /// when one is missing or `out` fails.
    fn assemble(&self, chunks: &[re::Digest], d: &re::Digest, out: &mut dyn Write) -> Option<bool> {
        let mut hasher = Sha256::new();
        for c in chunks {
            let data = self.read_whole(c)?;
            hasher.update(&data);
            out.write_all(&data).ok()?;
        }
        Some(hex::encode(hasher.finalize()) == d.hash)
    }

    /// Removes a layout whose chunks don't make up its blob (a server's
    /// wrong answer, a writer's bug): a miss, which storing the blob again
    /// replaces. Kept, the blob would look present and never be readable.
    fn drop_layout(&self, d: &re::Digest) {
        if let Some(p) = self.layout_path(&d.hash) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// The file of a blob stored whole, unread: for a part of it (a thin
    /// archive's member list), which can't be checked against the digest.
    pub fn blob_file(&self, d: &re::Digest) -> Option<PathBuf> {
        let p = self.blob_path(&d.hash)?;
        std::fs::metadata(&p).is_ok_and(|m| m.len() as i64 == d.size_bytes).then_some(p)
    }

    /// A blob stored whole. One whose contents aren't its digest (a torn
    /// write after a crash, a disk error, a cache shared with another
    /// writer) is removed: a miss, not an output.
    fn read_whole(&self, d: &re::Digest) -> Option<Vec<u8>> {
        let p = self.blob_path(&d.hash)?;
        let data = std::fs::read(&p).ok()?;
        // Another size is another digest (the file is keyed by hash).
        if data.len() as i64 != d.size_bytes {
            return None;
        }
        if rbe_client::verify(d, &data).is_err() {
            let _ = std::fs::remove_file(&p);
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
            return self.write_atomic(&self.blob_path(&d.hash).ok_or_else(|| malformed(&d.hash))?, data);
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
        self.put_file_as(path, &d, true)?;
        Ok(d)
    }

    /// Stores a file's contents, read once, and returns their digest: hashed
    /// as they're stored (a large file split as it's read, never whole in
    /// memory), for a file nothing hashed before (a local result's output).
    pub fn put_file_hashed(&self, path: &Path) -> anyhow::Result<re::Digest> {
        use std::io::Read as _;
        let mut file = std::fs::File::open(path)?;
        if !CHUNKER.splits(file.metadata()?.len() as i64) {
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            let d = rbe_client::digest_of(&data);
            // As chunks should it have grown meanwhile.
            self.put_bytes(&d, &data)?;
            return Ok(d);
        }
        let mut whole = Sha256::new();
        let (mut chunks, mut size) = (Vec::new(), 0);
        for c in CHUNKER.stream(file) {
            let c = c.map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            whole.update(&c.data);
            size += c.data.len() as i64;
            chunks.push(self.put_chunk(&c.data)?);
        }
        let d = re::Digest { hash: hex::encode(whole.finalize()), size_bytes: size };
        // Shrunk meanwhile to a blob stored whole: not from these chunks.
        anyhow::ensure!(is_chunked(&d), "{} changed while being stored", path.display());
        if !self.contains_blob(&d) {
            self.put_layout(&d, re::SplitBlobResponse { chunk_digests: chunks, chunking_function: FAST_CDC })?;
        }
        Ok(d)
    }

    /// A blob in a file of its own, to be read whole (an upload): the file
    /// it's stored in, which nothing changes (a blob's file is only ever
    /// replaced by another holding it), else its chunks put together in
    /// `tmp/` (checked to make it up), removed once dropped. None when the
    /// cache doesn't hold it.
    pub fn blob_as_file(&self, d: &re::Digest) -> Option<BlobFile> {
        if let Some(path) = self.blob_file(d) {
            return Some(BlobFile { path, temporary: false });
        }
        let chunks = self.layout(d)?;
        let file = BlobFile { path: self.root.join("tmp").join(unique_name()), temporary: true };
        let mut out = std::io::BufWriter::new(std::fs::File::options().write(true).create_new(true).open(&file.path).ok()?);
        match self.assemble(&chunks, d, &mut out) {
            Some(true) => {}
            Some(false) => {
                self.drop_layout(d);
                return None;
            }
            None => return None,
        }
        out.flush().ok()?;
        Some(file)
    }

    /// Stores a file known to hold the blob `d` (a download, checked as it
    /// was written, into a file only this process writes): read once, and
    /// hashed only to name its chunks. Should the file not be the blob
    /// after all, its layout is dropped when read (see `read_blob`).
    pub fn put_file_with_digest(&self, path: &Path, d: &re::Digest) -> anyhow::Result<()> {
        self.put_file_as(path, d, false)
    }

    /// Stores `path` as the blob `d`; `check`: that its contents are still
    /// `d`'s, when it was hashed earlier and could have changed since.
    fn put_file_as(&self, path: &Path, d: &re::Digest, check: bool) -> anyhow::Result<()> {
        if self.contains_blob(d) {
            return Ok(());
        }
        if !is_chunked(d) {
            let data = std::fs::read(path)?;
            let changed = data.len() as i64 != d.size_bytes || (check && rbe_client::verify(d, &data).is_err());
            anyhow::ensure!(!changed, "{} changed while being stored", path.display());
            self.write_atomic(&self.blob_path(&d.hash).ok_or_else(|| malformed(&d.hash))?, &data)?;
            return Ok(());
        }
        let file = std::fs::File::open(path)?;
        let mut whole = Sha256::new();
        let (mut chunks, mut size) = (Vec::new(), 0);
        for c in CHUNKER.stream(file) {
            let c = c.map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            if check {
                whole.update(&c.data);
            }
            size += c.data.len() as i64;
            chunks.push(self.put_chunk(&c.data)?);
        }
        // The layout must describe the blob `d` names.
        anyhow::ensure!(size == d.size_bytes && (!check || hex::encode(whole.finalize()) == d.hash), "{} changed while being stored", path.display());
        self.put_layout(d, re::SplitBlobResponse { chunk_digests: chunks, chunking_function: FAST_CDC })?;
        Ok(())
    }

    fn put_chunk(&self, chunk: &[u8]) -> std::io::Result<re::Digest> {
        let d = rbe_client::digest_of(chunk);
        if !self.contains_whole(&d) {
            self.write_atomic(&self.blob_path(&d.hash).ok_or_else(|| malformed(&d.hash))?, chunk)?;
        }
        Ok(d)
    }

    /// Records a blob's layout. Its chunks must be stored first.
    fn put_layout(&self, d: &re::Digest, layout: re::SplitBlobResponse) -> std::io::Result<()> {
        if let Some(c) = layout.chunk_digests.iter().find(|c| !rbe_client::valid_hash(&c.hash)) {
            return Err(malformed(&c.hash));
        }
        self.write_atomic(&self.layout_path(&d.hash).ok_or_else(|| malformed(&d.hash))?, &layout.encode_to_vec())
    }

    // Chunks of a blob split elsewhere (by a remote cache), for downloads
    // that only fetch the chunks the cache lacks.

    /// Whether the cache holds a chunk (marking it used).
    pub fn contains_chunk(&self, d: &re::Digest) -> bool {
        d.size_bytes == 0 || self.contains_whole(d)
    }

    /// Stores a chunk, whatever its size. Its digest must have been checked.
    pub fn put_chunk_bytes(&self, d: &re::Digest, data: &[u8]) -> std::io::Result<()> {
        if self.contains_chunk(d) {
            return Ok(());
        }
        self.write_atomic(&self.blob_path(&d.hash).ok_or_else(|| malformed(&d.hash))?, data)
    }

    /// A chunk's contents (marking it used), if the cache holds it.
    pub fn read_chunk(&self, d: &re::Digest) -> Option<Vec<u8>> {
        if d.size_bytes == 0 { Some(Vec::new()) } else { self.read_whole(d) }
    }

    /// Records the layout a remote split `d` into: its chunks must be
    /// stored, and checked to make up `d` (a server's wrong answer is never
    /// stored). False when the cache keeps `d` whole (it's small).
    pub fn put_split(&self, d: &re::Digest, split: re::SplitBlobResponse) -> std::io::Result<bool> {
        if !is_chunked(d) || split.chunk_digests.iter().map(|c| c.size_bytes).sum::<i64>() != d.size_bytes {
            return Ok(false);
        }
        self.put_layout(d, split)?;
        Ok(true)
    }

    /// The action's result, if cached and if the CAS still holds all its
    /// outputs (eviction may have removed some).
    pub fn get_action_result(&self, action: &re::Digest) -> Option<re::ActionResult> {
        self.get_action_result_and_absent(action).filter(|(_, absent)| absent.is_empty()).map(|(r, _)| r)
    }

    /// An action's result, and the blobs it references that the cache
    /// doesn't hold: outputs left in a remote CAS (minimal downloads).
    pub fn get_action_result_and_absent(&self, action: &re::Digest) -> Option<(re::ActionResult, Vec<re::Digest>)> {
        let p = self.ac_path(&action.hash)?;
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
        if let Some(p) = self.ac_path(&action.hash) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Records an action's result. Its blobs must be stored first.
    pub fn put_action_result(&self, action: &re::Digest, result: &re::ActionResult) -> std::io::Result<()> {
        self.write_atomic(&self.ac_path(&action.hash).ok_or_else(|| malformed(&action.hash))?, &result.encode_to_vec())
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
        let tmp = self.root.join("tmp").join(unique_name());
        std::fs::File::options().write(true).create_new(true).open(&tmp).and_then(|mut f| f.write_all(data))?;
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
        // Another tool's: its entries are its own to collect.
        if self.foreign {
            return Ok(Some((0, 0)));
        }
        let lock = std::fs::File::options().create(true).truncate(false).write(true).open(self.root.join("gc").join("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        // Unlocked when done, not by closing the file: a command forked
        // meanwhile holds it open until it starts its program, and with it
        // the lock (which would make the next collection think another
        // process is collecting).
        struct Unlock(std::fs::File);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let _ = self.0.unlock();
            }
        }
        let _unlock = Unlock(lock);
        // Files writers that died left in `tmp/`.
        let now = SystemTime::now();
        for e in std::fs::read_dir(self.root.join("tmp")).into_iter().flatten().flatten() {
            let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| now.duration_since(t).unwrap_or_default() > STALE_TMP);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
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

/// A blob in a file of its own (see `DiskCache::blob_as_file`).
pub struct BlobFile {
    path: PathBuf,
    /// Its chunks put together: removed when dropped.
    temporary: bool,
}

impl BlobFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BlobFile {
    fn drop(&mut self) {
        if self.temporary {
            let _ = std::fs::remove_file(&self.path);
        }
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

    fn cache(name: &str) -> (DiskCache, test_dirs::TestDir) {
        let dir = test_dirs::TestDir::new(&format!("ninja-rs-disk-cache-{name}"));
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
        assert_eq!(c.read_blob(&d).unwrap(), data);
        // Contents that aren't the digest's (a torn write) are a miss, and go.
        let e = rbe_client::digest_of(b"world");
        c.put_bytes(&e, b"world").unwrap();
        std::fs::write(c.blob_path(&e.hash).unwrap(), b"WORLD").unwrap();
        assert!(c.read_blob(&e).is_none());
        assert!(!c.blob_path(&e.hash).unwrap().exists());
        // A malformed digest (from a result: `..`, an absolute path) names
        // no file, so it never reaches, nor removes, one outside the cache.
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "not a blob").unwrap();
        let abs = victim.to_string_lossy().into_owned();
        for hash in ["../victim.txt", "../../victim.txt", abs.as_str(), "zz", ""] {
            let bad = re::Digest { hash: hash.into(), size_bytes: 10 };
            assert!(c.read_blob(&bad).is_none() && !c.contains_blob(&bad), "{hash}");
            assert!(c.put_bytes(&bad, b"not a blob").is_err(), "{hash}");
            assert!(c.get_action_result_and_absent(&bad).is_none(), "{hash}");
            c.remove_action_result(&bad);
        }
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "not a blob");

        let action = rbe_client::digest_of(b"action");
        let result =
            re::ActionResult { output_files: vec![re::OutputFile { path: "a.o".into(), digest: Some(d.clone()), ..Default::default() }], ..Default::default() };
        c.put_action_result(&action, &result).unwrap();
        assert_eq!(c.get_action_result(&action).unwrap(), result);
        // Without its output, the result is a miss.
        std::fs::remove_file(c.blob_path(&d.hash).unwrap()).unwrap();
        assert!(c.get_action_result(&action).is_none());

        // A file's digest and contents.
        let f = dir.join("input.txt");
        std::fs::write(&f, "file contents").unwrap();
        let fd = c.put_file(&f, None).unwrap();
        assert_eq!(c.read_blob(&fd).unwrap(), b"file contents");
        assert!(c.bytes_written() >= 5 + 13);
    }

    #[test]
    fn evicts_least_recently_used() {
        let (c, _dir) = cache("evict");
        let blobs: Vec<_> = (0..4u8)
            .map(|i| {
                let data = vec![i; 1000];
                let d = rbe_client::digest_of(&data);
                c.put_bytes(&d, &data).unwrap();
                let past = SystemTime::now() - std::time::Duration::from_secs(1000 - u64::from(i) * 100);
                std::fs::File::options().write(true).open(c.blob_path(&d.hash).unwrap()).unwrap().set_modified(past).unwrap();
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
    }

    #[test]
    fn walks_only_when_the_estimate_passes_the_limit() {
        let (c, _dir) = cache("estimate");
        let small = vec![1u8; 100];
        c.put_bytes(&rbe_client::digest_of(&small), &small).unwrap();
        assert_eq!(c.collect_garbage(1000, 900).unwrap(), Some((0, 0)));
        assert_eq!(c.read_size_estimate(), Some(100));
        // A file the estimate doesn't know about (another process's, say)
        // isn't seen until the estimate passes the limit.
        let big = vec![2u8; 2000];
        let big_digest = rbe_client::digest_of(&big);
        std::fs::create_dir_all(c.blob_path(&big_digest.hash).unwrap().parent().unwrap()).unwrap();
        std::fs::write(c.blob_path(&big_digest.hash).unwrap(), &big).unwrap();
        assert_eq!(c.collect_garbage(1000, 900).unwrap(), Some((0, 0)));
        assert_eq!(c.read_size_estimate(), Some(100));
        // This process's writes push it over: the walk finds the real size.
        let more = vec![3u8; 950];
        c.put_bytes(&rbe_client::digest_of(&more), &more).unwrap();
        let (removed, _) = c.collect_garbage(1000, 900).unwrap().unwrap();
        assert!(removed >= 1);
        assert!(c.read_size_estimate().unwrap() <= 900);
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
        // Closed first: Windows doesn't remove open files.
        drop((c, other));
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
        let data = std::fs::read(c.layout_path(&d.hash).unwrap()).unwrap();
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
        let (c, _dir) = cache("chunks");
        let a = noise(8 << 20, 1);
        let da = rbe_client::digest_of(&a);
        c.put_bytes(&da, &a).unwrap();
        assert!(!c.blob_path(&da.hash).unwrap().exists());
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
        std::fs::remove_file(c.blob_path(&only_b.hash).unwrap()).unwrap();
        assert!(!c.contains_blob(&db));
        assert!(c.read_blob(&db).is_none());
        assert_eq!(c.read_blob(&da).unwrap(), a);
        // Storing it again restores the chunk.
        c.put_bytes(&db, &b).unwrap();
        assert_eq!(c.read_blob(&db).unwrap(), b);
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
        assert!(c.blob_path(&d.hash).unwrap().exists() && !c.layout_path(&d.hash).unwrap().exists());
    }

    /// A layout whose chunks are each right and add up to the size, but
    /// don't make up the blob (a server's wrong split): never stored from a
    /// download, and one stored anyway (by another writer) is dropped when
    /// read, so that storing the blob replaces it.
    #[test]
    fn wrong_layouts() {
        let (c, dir) = cache("wrong-layout");
        let real = noise(5 << 20, 7);
        let d = rbe_client::digest_of(&real);
        let other = noise(5 << 20, 8);
        let (a, b) = other.split_at(3 << 20);
        let (da, db) = (rbe_client::digest_of(a), rbe_client::digest_of(b));
        c.put_chunk_bytes(&da, a).unwrap();
        c.put_chunk_bytes(&db, b).unwrap();
        let split = re::SplitBlobResponse { chunk_digests: vec![da, db], chunking_function: FAST_CDC };
        assert!(!c.contains_blob(&d));
        // Stored (by another writer): it looks present until read.
        c.put_layout(&d, split.clone()).unwrap();
        assert!(c.contains_blob(&d));
        assert!(c.read_blob(&d).is_none());
        assert!(!c.contains_blob(&d));
        c.put_bytes(&d, &real).unwrap();
        assert_eq!(c.read_blob(&d).unwrap(), real);
        // The same through `read_blob_into`, then storing a file.
        let f = dir.join("other.bin");
        std::fs::write(&f, &other).unwrap();
        let d2 = rbe_client::digest_of(&other);
        let (x, y) = real.split_at(3 << 20);
        let (dx, dy) = (rbe_client::digest_of(x), rbe_client::digest_of(y));
        c.put_chunk_bytes(&dx, x).unwrap();
        c.put_chunk_bytes(&dy, y).unwrap();
        c.put_layout(&d2, re::SplitBlobResponse { chunk_digests: vec![dx, dy], chunking_function: FAST_CDC }).unwrap();
        assert!(!c.read_blob_into(&d2, &mut Vec::new()));
        assert_eq!(c.put_file(&f, None).unwrap(), d2);
        assert_eq!(c.read_blob(&d2).unwrap(), other);
    }

    /// A download stored with its known digest: the same layout as hashing
    /// it gives.
    #[test]
    fn files_with_known_digests() {
        let (c, dir) = cache("known-digest");
        let data = noise(5 << 20, 9);
        let f = dir.join("big.bin");
        std::fs::write(&f, &data).unwrap();
        let d = rbe_client::digest_of(&data);
        c.put_file_with_digest(&f, &d).unwrap();
        let expected: Vec<_> = CHUNKER.chunks(&data).map(rbe_client::digest_of).collect();
        assert_eq!(stored_layout(&c, &d), expected);
        assert_eq!(c.read_blob(&d).unwrap(), data);
        // A file that isn't the blob's size isn't stored as it.
        let short = re::Digest { hash: "ab".repeat(32), size_bytes: d.size_bytes + 1 };
        assert!(c.put_file_with_digest(&f, &short).is_err());
        assert!(!c.contains_blob(&short));
    }

    #[test]
    fn evicts_chunks_no_longer_used() {
        let (c, _dir) = cache("chunk-evict");
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
    }

    /// Writers sharing a cache and a PID (containers on one volume): each
    /// result lands under its own key, and stale temporary files go.
    #[test]
    fn concurrent_writers() {
        let (c, dir) = cache("writers");
        let other = DiskCache::open(&dir).unwrap();
        let result = |i: u32| re::ActionResult { exit_code: i as i32, ..Default::default() };
        let action = |i: u32| rbe_client::digest_of(format!("action {i}").as_bytes());
        std::thread::scope(|s| {
            for (cache, offset) in [(&c, 0), (&other, 1)] {
                s.spawn(move || {
                    for i in (offset..600).step_by(2) {
                        cache.put_action_result(&action(i), &result(i)).unwrap();
                    }
                });
            }
        });
        for i in 0..600 {
            assert_eq!(c.get_action_result(&action(i)), Some(result(i)), "action {i}");
        }
        let stale = dir.join("tmp").join("stale");
        std::fs::write(&stale, "x").unwrap();
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(SystemTime::now() - 2 * STALE_TMP).unwrap();
        c.collect_garbage(u64::MAX, u64::MAX).unwrap();
        assert!(!stale.exists());
    }

    /// Bazel's cache (named by a shared `.bazelrc`): used as it is, without
    /// a `VERSION` of ours, and its entries never collected. An empty
    /// directory becomes ours; ours without its `VERSION` (removed by a
    /// cleaner) is ours again, collected; another directory isn't one.
    #[test]
    fn another_tools_cache() {
        let dir = test_dirs::TestDir::new("ninja-rs-disk-cache-bazel");
        let theirs = vec![7u8; 1000];
        let d = rbe_client::digest_of(&theirs);
        let blob = dir.join("cas").join(&d.hash[..2]).join(&d.hash);
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, &theirs).unwrap();
        let c = DiskCache::open(&dir).unwrap();
        assert!(c.is_foreign() && !dir.join("VERSION").exists());
        assert_eq!(c.read_blob(&d).unwrap(), theirs);
        let ours = vec![8u8; 1000];
        c.put_bytes(&rbe_client::digest_of(&ours), &ours).unwrap();
        assert_eq!(c.collect_garbage(0, 0).unwrap(), Some((0, 0)));
        assert!(blob.exists() && c.contains_blob(&rbe_client::digest_of(&ours)));
        let empty = test_dirs::TestDir::new("ninja-rs-disk-cache-empty");
        assert!(!DiskCache::open(&empty).unwrap().is_foreign());
        assert!(empty.join("VERSION").exists());
        assert!(!DiskCache::open(&empty.join("new")).unwrap().is_foreign());

        let (c, dir) = cache("version-removed");
        let old = (0..3u8).map(|i| vec![i; 1000]).collect::<Vec<_>>();
        for data in &old {
            c.put_bytes(&rbe_client::digest_of(data), data).unwrap();
        }
        c.collect_garbage(u64::MAX, u64::MAX).unwrap();
        std::fs::remove_file(dir.join("VERSION")).unwrap();
        let c = DiskCache::open(&dir).unwrap();
        assert!(!c.is_foreign() && dir.join("VERSION").exists());
        assert_eq!(c.collect_garbage(1, 0).unwrap().map(|(files, _)| files), Some(3));

        // Ours before it wrote its `size` (a build killed, one that stored
        // nothing), its VERSION gone: still empty, ours.
        let (_, dir) = cache("version-removed-empty");
        std::fs::remove_file(dir.join("VERSION")).unwrap();
        std::fs::create_dir_all(dir.join("cas").join("ab")).unwrap();
        let c = DiskCache::open(&dir).unwrap();
        assert!(!c.is_foreign() && dir.join("VERSION").exists());
        assert_eq!(c.noted_env(), None);

        let elsewhere = test_dirs::TestDir::new("ninja-rs-disk-cache-mistyped");
        std::fs::write(elsewhere.join("main.c"), "int main() {}").unwrap();
        assert!(DiskCache::open(&elsewhere).is_err());
        assert!(!elsewhere.join("VERSION").exists() && !elsewhere.join("cas").exists());
        // Bazel's has `ac/` or `cas/`.
        std::fs::create_dir(elsewhere.join("tmp")).unwrap();
        std::fs::remove_file(elsewhere.join("main.c")).unwrap();
        assert!(DiskCache::open(&elsewhere).is_err());
    }

    /// That a cache is Bazel's is said once per invocation tree: not by an
    /// invocation an invocation saying it runs (it gets the caches it said
    /// were, see `noted_env`), nor twice in a process.
    #[test]
    fn bazel_cache_noted_once() {
        let (root, other) = (PathBuf::from("/caches/bazel-noted-once"), PathBuf::from("/caches/bazel-noted-by-parent"));
        assert!(note_bazel_cache(&root, &[]));
        assert!(!note_bazel_cache(&root, &[]));
        assert!(!note_bazel_cache(&other, std::slice::from_ref(&other)));
        let dir = test_dirs::TestDir::new("ninja-rs-disk-cache-noted");
        let d = rbe_client::digest_of(b"theirs");
        std::fs::create_dir_all(dir.join("ac").join(&d.hash[..2])).unwrap();
        std::fs::write(dir.join("ac").join(&d.hash[..2]).join(&d.hash), "").unwrap();
        let c = DiskCache::open(&dir).unwrap();
        let noted: Vec<PathBuf> = std::env::split_paths(&c.noted_env().unwrap()).collect();
        assert!(noted.contains(&dir.to_path_buf()), "{noted:?}");
    }

    #[test]
    fn version_mismatch() {
        let (_, dir) = cache("version");
        std::fs::write(dir.join("VERSION"), "99\n").unwrap();
        assert!(DiskCache::open(&dir).is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10G"), Some(10 << 30));
        assert_eq!(parse_size("512m"), Some(512 << 20));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("x"), None);
    }
}
