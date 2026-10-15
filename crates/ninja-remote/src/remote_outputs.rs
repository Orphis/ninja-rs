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
//! record (ninja's own writes, and outputs of commands run on the host),
//! as does a remote action producing it again. Side outputs (a compile's
//! split DWARF, a link's PDB), which the build doesn't know of, are
//! recorded with the output of their action: their records go with its.
//! A build without the remote service drops them all: they can't be
//! fetched, and local runs replace them.
//!
//! File format (`.ninja_remote`): a version line, then one line per output:
//! `<mtime ns> <hash> <size> <x|-> <path>`, followed for a thin archive by
//! one `+ <name>` line per member, as the archive stores it (see
//! `archive`), and for a side output by `= <path>`, the output it goes
//! with (which earlier versions skip). Version 1 files, without member
//! lines, are read too.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ninja_core::disk::Disk;
use ninja_core::graph::TimeStamp;
use rbe_client::proto as re;

pub const REMOTE_OUTPUTS_FILE: &str = ".ninja_remote";
const HEADER: &str = "# ninja-rs remote outputs v2";
const HEADER_V1: &str = "# ninja-rs remote outputs v1";

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The path as the build names it (relative to the build directory).
    pub path: String,
    pub digest: re::Digest,
    pub executable: bool,
    /// What `stat` reports, in ns (ninja's `TimeStamp`).
    pub mtime: TimeStamp,
    /// For a thin archive, its members as stored (read with ranged reads
    /// when the action produced it), so that actions reading it get them
    /// without downloading it.
    pub members: Option<Arc<Vec<String>>>,
    /// For a side output, the output of its action it goes with.
    pub owner: Option<String>,
}

pub struct RemoteOutputs {
    file: PathBuf,
    records: Mutex<Records>,
    changed: AtomicBool,
}

/// The records, by key, and the side outputs recorded with each output.
#[derive(Default)]
struct Records {
    entries: HashMap<String, Entry>,
    side: HashMap<String, Vec<String>>,
}

impl Records {
    fn insert(&mut self, e: Entry) {
        let k = key(&e.path);
        if let Some(owner) = &e.owner {
            self.side.entry(key(owner)).or_default().push(k.clone());
        }
        self.entries.insert(k, e);
    }
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
        let mut entries: Vec<Entry> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(file)
            && matches!(text.lines().next(), Some(HEADER | HEADER_V1))
        {
            // Whether the last entry line was kept: the lines after it
            // describe it.
            let mut kept = false;
            for line in text.lines().skip(1) {
                if let Some(name) = line.strip_prefix("+ ") {
                    if let Some(e) = entries.last_mut().filter(|_| kept) {
                        Arc::make_mut(e.members.get_or_insert_with(Default::default)).push(name.to_string());
                    }
                    continue;
                }
                if let Some(owner) = line.strip_prefix("= ") {
                    if let Some(e) = entries.last_mut().filter(|_| kept) {
                        e.owner = Some(owner.to_string());
                    }
                    continue;
                }
                kept = false;
                let mut parts = line.splitn(5, ' ');
                let (Some(mtime), Some(hash), Some(size), Some(x), Some(path)) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else {
                    continue;
                };
                let (Ok(mtime), Ok(size)) = (mtime.parse(), size.parse()) else { continue };
                let digest = re::Digest { hash: hash.to_string(), size_bytes: size };
                // A digest names a disk cache file, and its size bounds what
                // a read allocates: a malformed one is dropped.
                if !rbe_client::valid_digest(&digest) {
                    continue;
                }
                entries.push(Entry { path: path.to_string(), digest, executable: x == "x", mtime, members: None, owner: None });
                kept = true;
            }
        }
        let mut records = Records::default();
        for e in entries {
            records.insert(e);
        }
        Self { file: file.to_path_buf(), records: Mutex::new(records), changed: AtomicBool::new(false) }
    }

    /// Writes the store back, if it changed.
    pub fn save(&self) -> std::io::Result<()> {
        if !self.changed.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let records = self.records.lock().unwrap();
        let entries = &records.entries;
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
                for m in e.members.iter().flat_map(|m| m.iter()) {
                    writeln!(f, "+ {m}")?;
                }
                if let Some(owner) = &e.owner {
                    writeln!(f, "= {owner}")?;
                }
            }
            f.flush()?;
        }
        std::fs::rename(&tmp, &self.file)
    }

    pub fn record(&self, e: Entry) {
        self.records.lock().unwrap().insert(e);
        self.changed.store(true, Ordering::Relaxed);
    }

    pub fn get(&self, path: &str) -> Option<Entry> {
        self.records.lock().unwrap().entries.get(&key(path)).cloned()
    }

    /// Drops the record of a path (written locally, or gone from the CAS).
    pub fn forget(&self, path: &str) {
        if self.records.lock().unwrap().entries.remove(&key(path)).is_some() {
            self.changed.store(true, Ordering::Relaxed);
        }
    }

    /// Drops the record of an output of an action, and those of the side
    /// outputs recorded with it: the action ran again (locally), or its
    /// output was removed. The side outputs it wrote replace theirs; those
    /// it didn't write are gone.
    pub fn forget_output(&self, path: &str) {
        let mut records = self.records.lock().unwrap();
        let k = key(path);
        let mut dropped = records.entries.remove(&k).is_some();
        for side in records.side.remove(&k).unwrap_or_default() {
            // Unless recorded again since, with another output.
            if records.entries.get(&side).is_some_and(|e| e.owner.as_deref().map(key).as_ref() == Some(&k)) {
                records.entries.remove(&side);
                dropped = true;
            }
        }
        if dropped {
            self.changed.store(true, Ordering::Relaxed);
        }
    }

    pub fn clear(&self) {
        let mut records = self.records.lock().unwrap();
        if !records.entries.is_empty() {
            *records = Records::default();
            self.changed.store(true, Ordering::Relaxed);
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.records.lock().unwrap().entries.values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.records.lock().unwrap().entries.len()
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

    /// A local run (a local edge, a fallback, a raced run that won) wrote
    /// them: the records are stale, those of their side outputs too, and
    /// would bring the old outputs back should the files be removed.
    fn outputs_written(&self, paths: &[&str]) {
        for p in paths {
            self.outputs.forget_output(p);
        }
    }

    fn make_dirs(&self, path: &str) -> Result<(), String> {
        self.inner.make_dirs(path)
    }

    fn remove_file(&self, path: &str) -> Result<bool, String> {
        let recorded = self.outputs.get(path).is_some();
        self.outputs.forget_output(path);
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
        Entry { path: path.into(), digest: rbe_client::digest_of(path.as_bytes()), executable: false, mtime, members: None, owner: None }
    }

    #[test]
    fn persists() {
        let file = test_dirs::base().join(format!("ninja-rs-remote-outputs-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let s = RemoteOutputs::load(&file);
        assert!(s.is_empty());
        s.record(entry("obj/a.o", 5));
        s.record(Entry { executable: true, ..entry("bin dir/app", 7) });
        let thin = Entry { members: Some(Arc::new(vec!["../obj/a.o".into(), "../obj/b c.o".into()])), ..entry("lib/libt.a", 8) };
        s.record(thin.clone());
        s.record(Entry { members: Some(Arc::new(vec![])), ..entry("lib/empty.a", 8) });
        s.save().unwrap();
        let t = RemoteOutputs::load(&file);
        assert_eq!(t.get("obj/a.o"), Some(entry("obj/a.o", 5)));
        assert!(t.get("bin dir/app").unwrap().executable);
        assert_eq!(t.get("lib/libt.a"), Some(thin));
        // An empty thin archive has no member lines: indistinguishable from
        // any other output, which is harmless.
        assert_eq!(t.get("lib/empty.a").unwrap().members, None);
        t.forget("lib/libt.a");
        t.forget("lib/empty.a");
        // Keys ignore separators (and case on Windows).
        assert!(t.get("obj\\a.o").is_some());
        t.forget("obj/a.o");
        t.forget("bin dir/app");
        t.save().unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn reads_version_1() {
        let file = test_dirs::base().join(format!("ninja-rs-remote-outputs-v1-{}", std::process::id()));
        std::fs::write(
            &file,
            format!(
                "{HEADER_V1}
5 {} 3 - obj/a.o
",
                "ab".repeat(32)
            ),
        )
        .unwrap();
        let t = RemoteOutputs::load(&file);
        assert_eq!(t.get("obj/a.o").unwrap().mtime, 5);
        let _ = std::fs::remove_file(&file);
    }

    /// Records with a malformed digest (a hash that isn't one, a negative
    /// size) are dropped, with the lines describing them.
    #[test]
    fn drops_malformed_records() {
        let file = std::env::temp_dir().join(format!("ninja-rs-remote-outputs-malformed-{}", std::process::id()));
        let hash = "ab".repeat(32);
        let text = format!("{HEADER}\n5 {hash} -3 - obj/neg.o\n= obj/x.o\n5 ../etc 3 - obj/path.o\n+ ../a.o\n5 {hash} 3 - obj/a.o\n");
        std::fs::write(&file, text).unwrap();
        let t = RemoteOutputs::load(&file);
        assert_eq!(t.entries().iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["obj/a.o"]);
        assert_eq!(t.get("obj/a.o").unwrap().members, None);
        let _ = std::fs::remove_file(&file);
    }

    /// Side outputs go with their action's output: when it's written
    /// locally or removed, their records go with its.
    #[test]
    fn side_outputs_go_with_their_output() {
        let file = std::env::temp_dir().join(format!("ninja-rs-remote-outputs-side-{}", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let s = RemoteOutputs::load(&file);
        s.record(entry("obj/a.o", 5));
        s.record(Entry { owner: Some("obj/a.o".into()), ..entry("obj/a.dwo", 5) });
        s.record(Entry { owner: Some("obj/b.o".into()), ..entry("obj/b.dwo", 5) });
        s.save().unwrap();
        let t = RemoteOutputs::load(&file);
        assert_eq!(t.get("obj/a.dwo").unwrap().owner.as_deref(), Some("obj/a.o"));
        let inner = VirtualDisk::new();
        let disk = RemoteDisk { inner: &inner, outputs: &t, fetch: None };
        disk.outputs_written(&["obj/a.o"]);
        assert!(t.get("obj/a.o").is_none() && t.get("obj/a.dwo").is_none());
        // An output that wasn't recorded itself (downloaded: named on the
        // command line), removed.
        disk.remove_file("obj/b.o").unwrap();
        assert!(t.get("obj/b.dwo").is_none());
        // A side output recorded again since with another output stays.
        t.record(Entry { owner: Some("obj/a.o".into()), ..entry("obj/c.dwo", 6) });
        t.record(Entry { owner: Some("obj/c.o".into()), ..entry("obj/c.dwo", 6) });
        t.forget_output("obj/a.o");
        assert!(t.get("obj/c.dwo").is_some());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn disk_view() {
        let dir = test_dirs::TestDir::new("ninja-rs-remote-outputs-disk");
        let file = dir.join(REMOTE_OUTPUTS_FILE);
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
