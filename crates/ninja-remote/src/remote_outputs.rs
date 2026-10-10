//! Outputs of remote actions that were not downloaded
//! (`--remote_download_outputs=minimal`): their digests live in the remote
//! CAS, and this store, persisted in the build directory, records them so
//! that the build treats them as present.
//!
//! [`RemoteDisk`] wraps the build's `Disk`: an output that is missing
//! locally but recorded here stats with its recorded time, so ninja's
//! up-to-date checks, build log and deps log work unchanged. Before a local
//! command runs, the backend downloads the recorded inputs it reads
//! (setting their recorded time, so nothing downstream looks stale); remote
//! actions get them by digest. A local write of a recorded path drops its
//! record, as does a remote action producing it again.
//!
//! File format (`.ninja_remote`): a version line, then one line per output:
//! `<mtime ns> <hash> <size> <x|-> <path>`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ninja_core::disk::Disk;
use ninja_core::graph::TimeStamp;
use rbe_client::proto as re;

pub const REMOTE_OUTPUTS_FILE: &str = ".ninja_remote";
const HEADER: &str = "# ninja-rs remote outputs v1";

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The path as the build names it (relative to the build directory).
    pub path: String,
    pub digest: re::Digest,
    pub executable: bool,
    /// What `stat` reports, in ns (ninja's `TimeStamp`).
    pub mtime: TimeStamp,
}

pub struct RemoteOutputs {
    file: PathBuf,
    entries: Mutex<HashMap<String, Entry>>,
    changed: AtomicBool,
}

impl std::fmt::Debug for RemoteOutputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteOutputs({}, {} entries)", self.file.display(), self.len())
    }
}

/// A path as a lookup key: forward slashes, lexically normalized,
/// lowercase on Windows.
pub fn key(path: &str) -> String {
    let s = path.replace('\\', "/");
    let (c, _) = ninja_core::canon::canonicalize_path_with(&s, false);
    if cfg!(windows) { c.to_ascii_lowercase() } else { c }
}

impl RemoteOutputs {
    /// Loads the store from `file` (empty when missing or unreadable).
    pub fn load(file: &Path) -> Self {
        let mut entries = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(file)
            && text.lines().next() == Some(HEADER)
        {
            for line in text.lines().skip(1) {
                let mut parts = line.splitn(5, ' ');
                let (Some(mtime), Some(hash), Some(size), Some(x), Some(path)) =
                    (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let (Ok(mtime), Ok(size)) = (mtime.parse(), size.parse()) else { continue };
                let e = Entry { path: path.to_string(), digest: re::Digest { hash: hash.to_string(), size_bytes: size }, executable: x == "x", mtime };
                entries.insert(key(path), e);
            }
        }
        Self { file: file.to_path_buf(), entries: Mutex::new(entries), changed: AtomicBool::new(false) }
    }

    /// Writes the store back, if it changed.
    pub fn save(&self) -> std::io::Result<()> {
        if !self.changed.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let entries = self.entries.lock().unwrap();
        if entries.is_empty() {
            return match std::fs::remove_file(&self.file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let mut sorted: Vec<&Entry> = entries.values().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let tmp = self.file.with_extension("tmp");
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            writeln!(f, "{HEADER}")?;
            for e in sorted {
                writeln!(f, "{} {} {} {} {}", e.mtime, e.digest.hash, e.digest.size_bytes, if e.executable { "x" } else { "-" }, e.path)?;
            }
            f.flush()?;
        }
        std::fs::rename(&tmp, &self.file)
    }

    pub fn record(&self, e: Entry) {
        self.entries.lock().unwrap().insert(key(&e.path), e);
        self.changed.store(true, Ordering::Relaxed);
    }

    pub fn get(&self, path: &str) -> Option<Entry> {
        self.entries.lock().unwrap().get(&key(path)).cloned()
    }

    /// Drops the record of a path (written locally, or gone from the CAS).
    pub fn forget(&self, path: &str) {
        if self.entries.lock().unwrap().remove(&key(path)).is_some() {
            self.changed.store(true, Ordering::Relaxed);
        }
    }

    pub fn clear(&self) {
        let mut entries = self.entries.lock().unwrap();
        if !entries.is_empty() {
            entries.clear();
            self.changed.store(true, Ordering::Relaxed);
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.entries.lock().unwrap().values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Downloads a recorded output to its path (blocking).
pub type Fetch = dyn Fn(&str) -> Result<(), String> + Send + Sync;

/// The build's view of the disk: recorded outputs exist (see the module
/// documentation).
pub struct RemoteDisk<'a> {
    pub inner: &'a dyn Disk,
    pub outputs: &'a RemoteOutputs,
    /// Downloads a recorded file that the build itself reads (a dyndep
    /// file, say).
    pub fetch: Option<&'a Fetch>,
}

impl Disk for RemoteDisk<'_> {
    fn stat(&self, path: &str) -> Result<TimeStamp, String> {
        let t = self.inner.stat(path)?;
        if t != 0 {
            return Ok(t);
        }
        Ok(self.outputs.get(path).map_or(0, |e| e.mtime))
    }

    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match self.inner.read_file(path)? {
            Some(data) => Ok(Some(data)),
            None => match (self.outputs.get(path), self.fetch) {
                (Some(_), Some(fetch)) => {
                    fetch(path)?;
                    self.inner.read_file(path)
                }
                _ => Ok(None),
            },
        }
    }

    fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), String> {
        self.outputs.forget(path);
        self.inner.write_file(path, contents)
    }

    fn make_dirs(&self, path: &str) -> Result<(), String> {
        self.inner.make_dirs(path)
    }

    fn remove_file(&self, path: &str) -> Result<bool, String> {
        let recorded = self.outputs.get(path).is_some();
        self.outputs.forget(path);
        Ok(self.inner.remove_file(path)? || recorded)
    }

    fn allow_stat_cache(&self, allow: bool) {
        self.inner.allow_stat_cache(allow)
    }
}

/// Gives a downloaded file its recorded time.
pub fn set_mtime(path: &Path, mtime: TimeStamp) -> std::io::Result<()> {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(mtime.max(0) as u64);
    std::fs::File::options().write(true).open(path)?.set_modified(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninja_core::disk::VirtualDisk;

    fn entry(path: &str, mtime: TimeStamp) -> Entry {
        Entry { path: path.into(), digest: rbe_client::digest_of(path.as_bytes()), executable: false, mtime }
    }

    #[test]
    fn persists() {
        let file = std::env::temp_dir().join(format!("ninja-rs-remote-outputs-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let s = RemoteOutputs::load(&file);
        assert!(s.is_empty());
        s.record(entry("obj/a.o", 5));
        s.record(Entry { executable: true, ..entry("bin dir/app", 7) });
        s.save().unwrap();
        let t = RemoteOutputs::load(&file);
        assert_eq!(t.get("obj/a.o"), Some(entry("obj/a.o", 5)));
        assert!(t.get("bin dir/app").unwrap().executable);
        // Keys ignore separators (and case on Windows).
        assert!(t.get("obj\\a.o").is_some());
        t.forget("obj/a.o");
        t.forget("bin dir/app");
        t.save().unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn disk_view() {
        let file = std::env::temp_dir().join(format!("ninja-rs-remote-outputs-disk-{}", std::process::id()));
        let outputs = RemoteOutputs::load(&file);
        outputs.record(entry("a.o", 42));
        let inner = VirtualDisk::new();
        inner.create("b.o", "b");
        let disk = RemoteDisk { inner: &inner, outputs: &outputs, fetch: None };
        assert_eq!(disk.stat("a.o").unwrap(), 42);
        assert!(disk.stat("b.o").unwrap() > 0);
        assert_eq!(disk.stat("c.o").unwrap(), 0);
        // A local write replaces the record.
        disk.write_file("a.o", b"local").unwrap();
        assert!(outputs.get("a.o").is_none());
        outputs.record(entry("d.o", 9));
        assert!(disk.remove_file("d.o").unwrap());
        assert_eq!(disk.stat("d.o").unwrap(), 0);
    }
}
