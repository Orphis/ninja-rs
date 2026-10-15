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
//! File format (`.ninja_remote`): a version line, then a journal of the
//! changes, read in order: per output recorded,
//! `<mtime ns> <hash> <size> <x|-> <path>` (replacing an earlier record of
//! the path), followed for a thin archive by one `+ <name>` line per
//! member, as the archive stores it (see `archive`), and for a side output
//! by `= <path>`, the output it goes with (which earlier versions skip);
//! per record dropped, `- <path>`. Each change is appended as it is made:
//! a build and the builds it runs in the same directory (a nested build,
//! sharing its lock: see `ninja_core::build_log::Access`) keep each
//! other's changes, in the order they were made; a build rewriting it
//! whole dropped the others'. The build holding the directory's lock
//! rewrites it without the records dropped when they get many (see
//! `load_compacting`). Versions 1 and 2, without `- ` lines, are read too.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ninja_core::disk::Disk;
use ninja_core::graph::TimeStamp;
use rbe_client::proto as re;

pub const REMOTE_OUTPUTS_FILE: &str = ".ninja_remote";
const HEADER: &str = "# ninja-rs remote outputs v3";
const HEADER_V2: &str = "# ninja-rs remote outputs v2";
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
}

/// The records, by key, and the side outputs recorded with each output;
/// the file changes are appended to.
#[derive(Default)]
struct Records {
    entries: HashMap<String, Entry>,
    side: HashMap<String, Vec<String>>,
    /// Opened at the first change.
    journal: Option<File>,
    /// The first change that couldn't be appended, which `save` reports.
    error: Option<std::io::Error>,
    /// The file's lines of records and of records dropped, when loaded.
    lines: usize,
}

impl Records {
    fn insert(&mut self, e: Entry) {
        let k = key(&e.path);
        if let Some(owner) = &e.owner {
            self.side.entry(key(owner)).or_default().push(k.clone());
        }
        self.entries.insert(k, e);
    }

    /// Appends `text`, changes in the file's format, to `file`.
    fn append(&mut self, file: &Path, text: &str) {
        if self.error.is_some() {
            return;
        }
        let appended = match &self.journal {
            Some(j) => (&*j).write_all(text.as_bytes()),
            None => open_journal(file).and_then(|j| (&j).write_all(text.as_bytes()).map(|()| self.journal = Some(j))),
        };
        if let Err(e) = appended {
            self.error = Some(e);
        }
    }
}

/// Opens `file` to append changes to, with its version line if it is new,
/// and a torn last line (a write cut short) ended, so that the changes
/// start on a line of their own.
fn open_journal(file: &Path) -> std::io::Result<File> {
    let mut f = OpenOptions::new().read(true).append(true).create(true).open(file)?;
    let len = f.metadata()?.len();
    if len == 0 {
        f.write_all(format!("{HEADER}\n").as_bytes())?;
    } else {
        let mut last = [0u8];
        f.seek(SeekFrom::Start(len - 1))?;
        f.read_exact(&mut last)?;
        if last[0] != b'\n' {
            f.write_all(b"\n")?;
        }
    }
    Ok(f)
}

/// An entry's lines, in the file's format.
fn entry_lines(e: &Entry) -> String {
    use std::fmt::Write as _;
    let mut text = format!("{} {} {} {} {}\n", e.mtime, e.digest.hash, e.digest.size_bytes, if e.executable { "x" } else { "-" }, e.path);
    for m in e.members.iter().flat_map(|m| m.iter()) {
        let _ = writeln!(text, "+ {m}");
    }
    if let Some(owner) = &e.owner {
        let _ = writeln!(text, "= {owner}");
    }
    text
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
        let mut records = Records::default();
        if let Ok(text) = std::fs::read_to_string(file)
            && matches!(text.lines().next(), Some(HEADER | HEADER_V2 | HEADER_V1))
        {
            // The last entry read, while the lines after it describe it.
            let mut last: Option<Entry> = None;
            for line in text.lines().skip(1) {
                if let Some(name) = line.strip_prefix("+ ") {
                    if let Some(e) = &mut last {
                        Arc::make_mut(e.members.get_or_insert_with(Default::default)).push(name.to_string());
                    }
                    continue;
                }
                if let Some(owner) = line.strip_prefix("= ") {
                    if let Some(e) = &mut last {
                        e.owner = Some(owner.to_string());
                    }
                    continue;
                }
                if let Some(e) = last.take() {
                    records.insert(e);
                }
                if let Some(path) = line.strip_prefix("- ") {
                    records.entries.remove(&key(path));
                    records.lines += 1;
                    continue;
                }
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
                last = Some(Entry { path: path.to_string(), digest, executable: x == "x", mtime, members: None, owner: None });
                records.lines += 1;
            }
            if let Some(e) = last {
                records.insert(e);
            }
        }
        Self { file: file.to_path_buf(), records: Mutex::new(records) }
    }

    /// `load`, for the build holding the build directory's lock (none other
    /// appends meanwhile): the file is rewritten first with the records
    /// kept only, when there are many more lines than those (records
    /// replaced, dropped), or none is left.
    pub fn load_compacting(file: &Path) -> Self {
        let outputs = Self::load(file);
        let records = outputs.records.lock().unwrap();
        let live = records.entries.len();
        if records.lines > 0 && (live == 0 || records.lines > 2 * live + 100) {
            let _ = outputs.rewrite(&records);
        }
        drop(records);
        outputs
    }

    /// Writes `records` alone to the file, through a temporary file of this
    /// process's (another build may rewrite it too), renamed into place;
    /// with none, the file goes.
    fn rewrite(&self, records: &Records) -> std::io::Result<()> {
        if records.entries.is_empty() {
            return match std::fs::remove_file(&self.file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let mut sorted: Vec<&Entry> = records.entries.values().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let text: String = std::iter::once(format!("{HEADER}\n")).chain(sorted.into_iter().map(entry_lines)).collect();
        let mut name = self.file.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.tmp", std::process::id()));
        let tmp = self.file.with_file_name(name);
        let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &self.file));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    /// Reports a change that couldn't be appended to the file (each is
    /// appended as it is made).
    pub fn save(&self) -> std::io::Result<()> {
        match self.records.lock().unwrap().error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn record(&self, e: Entry) {
        let mut records = self.records.lock().unwrap();
        records.append(&self.file, &entry_lines(&e));
        records.insert(e);
    }

    pub fn get(&self, path: &str) -> Option<Entry> {
        self.records.lock().unwrap().entries.get(&key(path)).cloned()
    }

    /// Drops the record of a path (written locally, or gone from the CAS).
    pub fn forget(&self, path: &str) {
        let mut records = self.records.lock().unwrap();
        if let Some(e) = records.entries.remove(&key(path)) {
            records.append(&self.file, &format!("- {}\n", e.path));
        }
    }

    /// Drops the record of an output of an action, and those of the side
    /// outputs recorded with it: the action ran again (locally), or its
    /// output was removed. The side outputs it wrote replace theirs; those
    /// it didn't write are gone.
    pub fn forget_output(&self, path: &str) {
        let mut records = self.records.lock().unwrap();
        let k = key(path);
        let mut dropped: Vec<Entry> = records.entries.remove(&k).into_iter().collect();
        for side in records.side.remove(&k).unwrap_or_default() {
            // Unless recorded again since, with another output.
            if records.entries.get(&side).is_some_and(|e| e.owner.as_deref().map(key).as_ref() == Some(&k)) {
                dropped.extend(records.entries.remove(&side));
            }
        }
        if !dropped.is_empty() {
            let lines: String = dropped.iter().map(|e| format!("- {}\n", e.path)).collect();
            records.append(&self.file, &lines);
        }
    }

    pub fn clear(&self) {
        let mut records = self.records.lock().unwrap();
        if !records.entries.is_empty() {
            let lines: String = records.entries.values().map(|e| format!("- {}\n", e.path)).collect();
            records.append(&self.file, &lines);
            records.entries.clear();
            records.side.clear();
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
        let dir = test_dirs::TestDir::new("ninja-rs-remote-outputs");
        let file = dir.join(REMOTE_OUTPUTS_FILE);
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
        assert!(RemoteOutputs::load(&file).is_empty());
        // None left: the file goes when the next build loads it.
        assert!(RemoteOutputs::load_compacting(&file).is_empty());
        assert!(!file.exists());
    }

    /// A build and one it runs in the same directory (a nested build) both
    /// change the records: each keeps the other's changes, in the order
    /// they were made. Each wrote the file whole: the last one undid the
    /// other's (the nested build's records lost, or one it dropped, having
    /// rebuilt the output locally, back).
    #[test]
    fn builds_sharing_the_file_keep_each_others_changes() {
        let dir = test_dirs::TestDir::new("ninja-rs-remote-outputs-shared");
        let file = dir.join(REMOTE_OUTPUTS_FILE);
        let earlier = RemoteOutputs::load(&file);
        earlier.record(entry("a.o", 1));
        earlier.save().unwrap();
        let (outer, nested) = (RemoteOutputs::load_compacting(&file), RemoteOutputs::load(&file));
        outer.record(entry("x.o", 2));
        nested.record(entry("y.o", 3));
        nested.forget_output("a.o");
        nested.save().unwrap();
        outer.save().unwrap();
        let read = RemoteOutputs::load(&file);
        assert_eq!((read.get("x.o"), read.get("y.o"), read.get("a.o")), (Some(entry("x.o", 2)), Some(entry("y.o", 3)), None));
        // The next build rewrites it once it holds many more records
        // replaced and dropped than kept, through a temporary file of its
        // own.
        for i in 0..150 {
            outer.record(entry("x.o", i));
        }
        let next = RemoteOutputs::load_compacting(&file);
        assert_eq!((next.get("x.o"), next.get("y.o"), next.len()), (Some(entry("x.o", 149)), Some(entry("y.o", 3)), 2));
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 3);
        let files: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(files, [REMOTE_OUTPUTS_FILE]);
    }

    /// A change cut short (a crash, a full disk) leaves a torn line: the
    /// next one starts on a line of its own.
    #[test]
    fn a_torn_line_is_ended() {
        let dir = test_dirs::TestDir::new("ninja-rs-remote-outputs-torn");
        let file = dir.join(REMOTE_OUTPUTS_FILE);
        std::fs::write(&file, format!("{HEADER}\n{}5 abc", entry_lines(&entry("a.o", 1)))).unwrap();
        let s = RemoteOutputs::load(&file);
        s.record(entry("b.o", 2));
        let read = RemoteOutputs::load(&file);
        assert_eq!((read.get("a.o"), read.get("b.o")), (Some(entry("a.o", 1)), Some(entry("b.o", 2))));
    }

    #[test]
    fn reads_version_1() {
        let dir = test_dirs::TestDir::new("ninja-rs-remote-outputs-v1");
        let file = dir.join(REMOTE_OUTPUTS_FILE);
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
    }

    /// Records with a malformed digest (a hash that isn't one, a negative
    /// size) are dropped, with the lines describing them.
    #[test]
    fn drops_malformed_records() {
        let dir = test_dirs::TestDir::new("ninja-rs-remote-outputs-malformed");
        let file = dir.join(REMOTE_OUTPUTS_FILE);
        let hash = "ab".repeat(32);
        let text = format!("{HEADER}\n5 {hash} -3 - obj/neg.o\n= obj/x.o\n5 ../etc 3 - obj/path.o\n+ ../a.o\n5 {hash} 3 - obj/a.o\n");
        std::fs::write(&file, text).unwrap();
        let t = RemoteOutputs::load(&file);
        assert_eq!(t.entries().iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["obj/a.o"]);
        assert_eq!(t.get("obj/a.o").unwrap().members, None);
    }

    /// Side outputs go with their action's output: when it's written
    /// locally or removed, their records go with its.
    #[test]
    fn side_outputs_go_with_their_output() {
        let dir = test_dirs::TestDir::new("ninja-rs-remote-outputs-side");
        let file = dir.join(REMOTE_OUTPUTS_FILE);
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
