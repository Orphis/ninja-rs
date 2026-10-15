//! The deps log: dependencies discovered by `deps = gcc` / `deps = msvc`
//! edges (ninja's `.ninja_deps`).
//!
//! ninja-rs uses its own text format in `.ninja_rs_deps`. Like ninja's binary
//! log, each path is stored once and deps refer to paths by id, which keeps
//! the file (and memory) small when thousands of objects share headers:
//!
//! ```text
//! # ninja-rs deps v3
//! p\t<id>\t<path>                   defines path id <id> (0, 1, ...)
//! d\t<output id>\t<mtime ns>\t<id> <id> ...
//! ```
//!
//! Path records are appended just before the first deps record using them.
//! A path's id is written with it, as ninja writes a checksum of it: two
//! processes appending at once would give their new paths the same ids, and
//! a log read positionally would then give outputs each other's deps. As in
//! ninja, reading stops at the first record that doesn't fit (a path whose
//! id doesn't follow, a path or id seen before, a malformed line), and the
//! rest is cut off before the next append: those outputs build again. A
//! write cut short (a crash, a kill, a full disk) leaves a torn last line,
//! which is handled the same way.
//!
//! Builds take the build directory's lock
//! ([`LogLock`](crate::build_log::LogLock)) so that one at a time writes
//! the logs, but for the builds the holder runs (see
//! [`Access::Shared`]). Those append at once: each takes a lock on the file
//! itself while it appends, and reads first what the others appended, so
//! that its new paths get the next ids. A log's ids are its own, stable
//! while it is loaded; it maps them to the file's.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::build_log::{Access, file_len, open_for_append, replace_file};
use crate::graph::TimeStamp;
use crate::hash::HashMap;

pub const DEPS_LOG_FILE: &str = ".ninja_rs_deps";
// v3: path records carry their id (v2 had them positional).
const SIGNATURE: &str = "# ninja-rs deps v3\n";
const RECOVERING: &str = "premature end of file; recovering";
/// The file id of a path the file doesn't hold.
const NOT_IN_FILE: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    mtime: TimeStamp,
    nodes: Vec<u32>,
}

/// The deps of one output, borrowed from the log.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub mtime: TimeStamp,
    nodes: &'a [u32],
    log: &'a DepsLog,
}

impl<'a> Deps<'a> {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The ids of the dependency paths (see [`DepsLog::path`]).
    pub fn ids(&self) -> &'a [u32] {
        self.nodes
    }

    /// The dependency paths, canonical, in recorded order.
    pub fn paths(&self) -> impl ExactSizeIterator<Item = &'a str> + 'a {
        let log = self.log;
        self.nodes.iter().map(move |&id| log.paths[id as usize].as_str())
    }
}

#[derive(Default)]
pub struct DepsLog {
    /// Interned paths; ids index this.
    paths: Vec<String>,
    ids: HashMap<String, u32>,
    /// Output id -> its deps.
    deps: HashMap<u32, Record>,
    file: Option<File>,
    path: String,
    needs_recompaction: bool,
    /// Where the records read end, when something follows them (a torn
    /// line, a record that doesn't fit): cut off before the next append.
    truncate_to: Option<u64>,
    /// The file's length when loaded (`None`: there was none).
    loaded_len: Option<u64>,
    /// The file as this log knows it: which one (its identity), the id
    /// each path has there (indexed by this log's ids), this log's ids by
    /// the file's, and where what was read or written of it ends.
    known: Option<(u64, u128)>,
    file_ids: Vec<u32>,
    from_file: Vec<u32>,
    file_end: u64,
    /// The file goes on with what can't be followed (a writer that doesn't
    /// take turns, an end a shared writer may not cut): readers stop
    /// before what would be appended, so records stay in memory.
    stopped: bool,
}

impl DepsLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, output: &str) -> Option<Deps<'_>> {
        let id = *self.ids.get(output)?;
        self.deps.get(&id).map(|r| Deps { mtime: r.mtime, nodes: &r.nodes, log: self })
    }

    /// All outputs with deps, in no particular order.
    pub fn outputs(&self) -> impl Iterator<Item = (&str, Deps<'_>)> {
        self.deps.iter().map(|(&id, r)| (self.paths[id as usize].as_str(), Deps { mtime: r.mtime, nodes: &r.nodes, log: self }))
    }

    /// The path with id `id`. Ids are stable while the log is loaded.
    pub fn path(&self, id: u32) -> &str {
        &self.paths[id as usize]
    }

    /// Number of interned paths: ids are below this.
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    fn intern(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.ids.get(path) {
            return id;
        }
        let id = self.paths.len() as u32;
        self.paths.push(path.to_string());
        self.ids.insert(path.to_string(), id);
        self.file_ids.push(NOT_IN_FILE);
        id
    }

    /// Loads the log; a missing file is not an error. Returns a warning for
    /// an incompatible log (which is then ignored), or one whose end was
    /// left out (see the module's documentation).
    pub fn load(&mut self, path: &str) -> Result<Option<String>, String> {
        self.path = path.to_string();
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("loading deps log {path}: {e}")),
        };
        // Not while another process appends (see `append`): a write in
        // progress would read as a torn line.
        let mut data = Vec::new();
        let read = with_lock(&file, || (&file).read_to_end(&mut data));
        read.map_err(|e| format!("loading deps log {path}: {e}"))?;
        self.known = ninja_os::identity(&file).ok();
        self.loaded_len = Some(data.len() as u64);
        // Created by a process that hasn't appended yet.
        if data.is_empty() {
            return Ok(None);
        }
        if !data.starts_with(SIGNATURE.as_bytes()) {
            self.needs_recompaction = true;
            self.truncate_to = Some(0);
            return Ok(Some("deps log version is too old or invalid; starting over".into()));
        }
        let (end, total) = self.read_records(&data, SIGNATURE.len());
        self.file_end = end as u64;
        if end < data.len() {
            self.truncate_to = Some(end as u64);
            return Ok(Some(RECOVERING.into()));
        }
        if total > 1000 && total > self.deps.len() * 3 {
            self.needs_recompaction = true;
        }
        Ok(None)
    }

    /// Reads the complete lines of `data` from `offset`, up to the first
    /// that doesn't fit. Returns where they end, and how many deps records
    /// they hold.
    fn read_records(&mut self, data: &[u8], mut offset: usize) -> (usize, usize) {
        let mut total = 0;
        while let Some(len) = data[offset..].iter().position(|&b| b == b'\n') {
            let Ok(line) = std::str::from_utf8(&data[offset..offset + len]) else { break };
            let fits = if let Some(p) = line.strip_prefix("p\t") {
                self.read_path(p)
            } else if let Some(d) = line.strip_prefix("d\t") {
                self.read_deps(d).inspect(|()| total += 1).is_some()
            } else {
                false
            };
            if !fits {
                break;
            }
            offset += len + 1;
        }
        (offset, total)
    }

    /// Reads a path record, `<id>\t<path>`: its id must be the file's next
    /// one and its path new there, else another process wrote it with ids
    /// of its own.
    fn read_path(&mut self, record: &str) -> bool {
        let Some((id, path)) = record.split_once('\t') else { return false };
        if id.parse::<usize>().ok() != Some(self.from_file.len()) || path.is_empty() {
            return false;
        }
        let ours = self.intern(path);
        if self.file_ids[ours as usize] != NOT_IN_FILE {
            return false;
        }
        self.file_ids[ours as usize] = self.from_file.len() as u32;
        self.from_file.push(ours);
        true
    }

    /// Reads a deps record, `<output id>\t<mtime>\t<id> <id> ...`, of the
    /// file's ids.
    fn read_deps(&mut self, record: &str) -> Option<()> {
        let mut f = record.split('\t');
        let ours = |id: &str| self.from_file.get(id.parse::<usize>().ok()?).copied();
        let out = ours(f.next()?)?;
        let mtime = f.next()?.parse().ok()?;
        let nodes = f.next().unwrap_or("").split(' ').filter(|s| !s.is_empty()).map(ours).collect::<Option<Vec<u32>>>()?;
        self.deps.insert(out, Record { mtime, nodes });
        Some(())
    }

    /// Prepares for appending, as `access` allows: an exclusive writer
    /// recompacts first if needed (`is_live` tells whose deps stay) and
    /// cuts off what `load` left out; a shared one does neither, and
    /// records nothing on the file if `load` left something out.
    pub fn open_for_write(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool, access: Access) -> Result<(), String> {
        self.path = path.to_string();
        if access == Access::Shared {
            self.stopped = self.truncate_to.take().is_some();
            return Ok(());
        }
        // Written to since it was loaded (by a process that doesn't take the
        // build directory's lock): the ids read may not be the file's any
        // more. It is rewritten from what was read.
        if file_len(path) != self.loaded_len {
            self.needs_recompaction = true;
        }
        if self.needs_recompaction {
            self.recompact(path, is_live)?;
        }
        self.cut_torn_line()
    }

    /// Cuts off what `load` left out, so that appends follow the records
    /// read; only if the file is still the one read, whose end it is.
    fn cut_torn_line(&mut self) -> Result<(), String> {
        if let Some(len) = self.truncate_to.take() {
            if file_len(&self.path) != self.loaded_len {
                return Err(format!("deps log {} changed since it was read: is another build writing to it?", self.path));
            }
            OpenOptions::new().write(true).open(&self.path).and_then(|f| f.set_len(len)).map_err(|e| format!("truncating deps log {}: {e}", self.path))?;
            self.loaded_len = Some(len);
        }
        Ok(())
    }

    /// The file to append to: the one at the log's path, opened again once
    /// another process replaced it (recompacted it without the lock). A
    /// file other than the one read is followed from its start.
    fn ensure_open(&mut self) -> Result<(), String> {
        if self.file.as_ref().is_some_and(|f| ninja_os::replaced(f, Path::new(&self.path))) {
            self.file = None;
        }
        if self.file.is_none() {
            self.cut_torn_line()?;
            let f = open_for_append(&self.path).map_err(|e| format!("opening deps log: {e}"))?;
            let identity = ninja_os::identity(&f).ok();
            if identity.is_none() || identity != self.known {
                self.known = identity;
                self.file_ids.fill(NOT_IN_FILE);
                self.from_file.clear();
                self.file_end = 0;
                self.stopped = false;
            }
            self.file = Some(f);
        }
        Ok(())
    }

    /// Records the deps of `output`; skips the write if nothing changed.
    pub fn record_deps<S: AsRef<str>>(&mut self, output: &str, mtime: TimeStamp, nodes: &[S]) -> Result<(), String> {
        let out = self.intern(output);
        let record = Record { mtime, nodes: nodes.iter().map(|n| self.intern(n.as_ref())).collect() };
        if self.deps.get(&out) == Some(&record) {
            return Ok(());
        }
        if !self.path.is_empty() {
            self.ensure_open()?;
            if !self.stopped {
                self.append(out, &record)?;
            }
        }
        self.deps.insert(out, record);
        Ok(())
    }

    /// Appends the deps of `out`, and the paths the file doesn't hold yet,
    /// holding the file's lock (see the module's documentation).
    fn append(&mut self, out: u32, record: &Record) -> Result<(), String> {
        let file = self.file.take().expect("opened");
        let appended = with_lock(&file, || {
            if !self.follow(&file)? {
                self.stopped = true;
                return Ok(());
            }
            let mut text = String::new();
            if self.file_end == 0 {
                text.push_str(SIGNATURE);
            }
            let out = self.file_id(out, &mut text);
            let nodes: Vec<u32> = record.nodes.iter().map(|&id| self.file_id(id, &mut text)).collect();
            push_record(&mut text, out, record.mtime, &nodes);
            (&file).write_all(text.as_bytes())?;
            self.file_end += text.len() as u64;
            Ok(())
        });
        self.file = Some(file);
        appended.map_err(|e| format!("Error writing to deps log: {e}"))
    }

    /// Reads what other processes appended to `file` since this log last
    /// read or wrote it. False when it doesn't fit (or the file was cut).
    fn follow(&mut self, file: &File) -> std::io::Result<bool> {
        let len = file.metadata()?.len();
        if len == self.file_end {
            return Ok(true);
        }
        if len < self.file_end {
            return Ok(false);
        }
        let mut data = Vec::new();
        let mut f = file;
        f.seek(SeekFrom::Start(self.file_end))?;
        f.take(len - self.file_end).read_to_end(&mut data)?;
        let start = if self.file_end > 0 {
            0
        } else if data.starts_with(SIGNATURE.as_bytes()) {
            SIGNATURE.len()
        } else {
            return Ok(false);
        };
        let (end, _) = self.read_records(&data, start);
        self.file_end += end as u64;
        Ok(end == data.len())
    }

    /// The file's id of the path `id`, appending its record to `text` if
    /// the file doesn't hold it yet.
    fn file_id(&mut self, id: u32, text: &mut String) -> u32 {
        let known = self.file_ids[id as usize];
        if known != NOT_IN_FILE {
            return known;
        }
        let new = self.from_file.len() as u32;
        self.file_ids[id as usize] = new;
        self.from_file.push(id);
        push_path(text, new, &self.paths[id as usize]);
        new
    }

    pub fn close(&mut self) {
        self.file = None;
    }

    /// Rewrites the log with only the live outputs and the paths they use.
    pub fn recompact(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool) -> Result<(), String> {
        self.close();
        let mut live: Vec<(String, Record)> = std::mem::take(&mut self.deps)
            .into_iter()
            .filter(|(id, _)| is_live(&self.paths[*id as usize]))
            .map(|(id, r)| (self.paths[id as usize].clone(), r))
            .collect();
        live.sort_by(|a, b| a.0.cmp(&b.0));
        let old_paths = std::mem::take(&mut self.paths);
        self.ids.clear();
        self.file_ids.clear();
        let mut out = String::from(SIGNATURE);
        for (output, record) in live {
            let first_new = self.paths.len();
            let out_id = self.intern(&output);
            let nodes: Vec<u32> = record.nodes.iter().map(|&id| self.intern(&old_paths[id as usize])).collect();
            for (id, p) in self.paths.iter().enumerate().skip(first_new) {
                push_path(&mut out, id as u32, p);
            }
            push_record(&mut out, out_id, record.mtime, &nodes);
            self.deps.insert(out_id, Record { mtime: record.mtime, nodes });
        }
        replace_file(path, out.as_bytes())?;
        // The file's ids are this log's now.
        self.from_file = (0..self.paths.len() as u32).collect();
        self.file_ids.clone_from(&self.from_file);
        self.file_end = out.len() as u64;
        self.known = File::open(path).and_then(|f| ninja_os::identity(&f)).ok();
        self.stopped = false;
        self.needs_recompaction = false;
        self.truncate_to = None;
        self.loaded_len = Some(out.len() as u64);
        Ok(())
    }
}

/// Runs `f` holding `file`'s lock (see `ninja_os::lock`): while one process
/// appends to the log, another neither appends nor reads it. Without
/// locks (a file system that has none), `f` runs anyway.
fn with_lock<T>(file: &File, f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let locked = ninja_os::lock(file, true).is_ok();
    let result = f();
    if locked {
        let _ = ninja_os::unlock(file);
    }
    result
}

fn push_path(out: &mut String, id: u32, path: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "p\t{id}\t{path}");
}

fn push_record(out: &mut String, output: u32, mtime: TimeStamp, nodes: &[u32]) {
    use std::fmt::Write as _;
    let _ = write!(out, "d\t{output}\t{mtime}\t");
    for (i, id) in nodes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{id}");
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log's path, in a directory of its own (removed when dropped).
    fn tmp(name: &str) -> (test_dirs::TestDir, String) {
        let dir = test_dirs::TestDir::new(&format!("ninja_core_{name}"));
        let p = dir.join("log").to_string_lossy().into_owned();
        (dir, p)
    }

    fn paths(log: &DepsLog, output: &str) -> Vec<String> {
        log.get(output).unwrap().paths().map(str::to_string).collect()
    }

    #[test]
    fn write_read_and_skip_unchanged() {
        let (_dir, p) = tmp("deps");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("out.o", 1, &["foo.h", "bar.h"]).unwrap();
        log.record_deps("out.o", 1, &["foo.h", "bar.h"]).unwrap();
        log.record_deps("out2.o", 2, &[] as &[&str]).unwrap();
        log.record_deps("out3.o", 3, &["bar.h", "baz.h"]).unwrap();
        log.close();
        let text = std::fs::read_to_string(&p).unwrap();
        // Signature, 6 paths (outputs included), 3 records; bar.h once.
        assert_eq!(text.lines().count(), 1 + 6 + 3);
        assert_eq!(text.matches("bar.h").count(), 1);
        let mut log2 = DepsLog::new();
        log2.load(&p).unwrap();
        assert_eq!(paths(&log2, "out.o"), vec!["foo.h", "bar.h"]);
        assert!(log2.get("out2.o").unwrap().is_empty());
        assert_eq!(paths(&log2, "out3.o"), vec!["bar.h", "baz.h"]);
        assert_eq!(log2.get("out3.o").unwrap().mtime, 3);
        assert!(log2.get("foo.h").is_none());

        // Appending after a load reuses the ids already in the file.
        log2.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log2.record_deps("out4.o", 4, &["foo.h", "new.h"]).unwrap();
        log2.close();
        let mut log3 = DepsLog::new();
        log3.load(&p).unwrap();
        assert_eq!(paths(&log3, "out4.o"), vec!["foo.h", "new.h"]);
        assert_eq!(paths(&log3, "out.o"), vec!["foo.h", "bar.h"]);
    }

    #[test]
    fn recompact_keeps_live_outputs_and_their_paths() {
        let (_dir, p) = tmp("deps-recompact");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("live.o", 1, &["a.h", "b.h"]).unwrap();
        log.record_deps("dead.o", 1, &["c.h"]).unwrap();
        log.recompact(&p, &|o| o == "live.o").unwrap();
        log.record_deps("live2.o", 2, &["b.h", "d.h"]).unwrap();
        log.close();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("dead.o") && !text.contains("c.h"));
        let mut log2 = DepsLog::new();
        log2.load(&p).unwrap();
        assert_eq!(paths(&log2, "live.o"), vec!["a.h", "b.h"]);
        assert_eq!(paths(&log2, "live2.o"), vec!["b.h", "d.h"]);
        assert!(log2.get("dead.o").is_none());
    }

    /// Reading stops at the first record that doesn't fit, as in ninja:
    /// what follows was written by another process, or is garbage.
    #[test]
    fn a_record_that_does_not_fit_ends_the_log() {
        let (_dir, p) = tmp("deps-corrupt");
        let read = |text: &str| {
            std::fs::write(&p, format!("{SIGNATURE}{text}")).unwrap();
            let mut log = DepsLog::new();
            let warning = log.load(&p).unwrap();
            (log, warning)
        };
        let recovering = Some(RECOVERING.to_string());
        // The record naming path 9 is out of range: it and what follows are
        // left out.
        let (log, warning) = read("p\t0\tout.o\np\t1\ta.h\nd\t0\t5\t1\nd\t0\t6\t9\nd\t0\t7\t1\n");
        assert_eq!((log.get("out.o").unwrap().mtime, warning.clone()), (5, recovering.clone()));
        // A path whose id doesn't follow, a path seen before, a line of
        // neither kind: the same.
        for bad in ["p\t3\tb.h\n", "p\t2\ta.h\n", "p\tb.h\n", "x\n", "\n"] {
            let (log, warning) = read(&format!("p\t0\tout.o\np\t1\ta.h\nd\t0\t5\t1\n{bad}p\t2\tc.h\nd\t0\t6\t2\n"));
            assert_eq!((log.get("out.o").unwrap().mtime, warning), (5, recovering.clone()), "{bad:?}");
            assert_eq!(log.path_count(), 2, "{bad:?}");
        }
        let (log, warning) = read("p\t0\tout.o\np\t1\ta.h\nd\t0\t5\t1\np\t2\tc.h\nd\t0\t6\t2\n");
        assert_eq!((paths(&log, "out.o"), warning), (vec!["c.h".to_string()], None));
        // Older versions are ignored.
        std::fs::write(&p, "# ninja-rs deps v2\np\tout.o\np\ta.h\nd\t0\t5\t1\n").unwrap();
        let mut old = DepsLog::new();
        assert!(old.load(&p).unwrap().is_some());
        assert!(old.get("out.o").is_none());
    }

    /// A log another process wrote to after it was loaded isn't appended to
    /// with the ids read (nor cut where its end was): it's rewritten.
    #[test]
    fn a_log_changed_since_loaded_is_rewritten() {
        let (_dir, p) = tmp("deps-changed");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("a", 1, &["a.h"]).unwrap();
        log.close();
        let mut text = std::fs::read_to_string(&p).unwrap();
        // A torn last line, which the next append would cut off.
        std::fs::write(&p, &text[..text.len() - 2]).unwrap();
        let mut stale = DepsLog::new();
        assert!(stale.load(&p).unwrap().is_some());
        // Another process completes the line and adds a path of its own.
        text.push_str("p\t2\tother.h\n");
        std::fs::write(&p, &text).unwrap();
        stale.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        stale.record_deps("b", 2, &["b.h"]).unwrap();
        stale.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert!(log.get("a").is_none());
        assert_eq!(paths(&log, "b"), vec!["b.h"]);
    }

    #[test]
    fn torn_record_is_ignored_and_cut() {
        let (_dir, p) = tmp("deps-torn-record");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("b.o", 1, &["b.c"]).unwrap();
        log.record_deps("a.o", 100, &["a.c", "x.h", "y.h", "z.h"]).unwrap();
        log.close();
        // Cut inside the last record's ids (a crash, a full disk): it still
        // parses, as a shorter list of deps.
        let mut data = std::fs::read(&p).unwrap();
        data.truncate(data.iter().rposition(|&b| b == b' ').unwrap());
        std::fs::write(&p, &data).unwrap();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap().as_deref(), Some("premature end of file; recovering"));
        assert!(log.get("a.o").is_none());
        assert_eq!(paths(&log, "b.o"), vec!["b.c"]);
        // A build cuts it off, even one recording nothing.
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.close();
        assert!(std::fs::read(&p).unwrap().ends_with(b"\n"));
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("a.o", 200, &["a.c", "x.h"]).unwrap();
        log.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "a.o"), vec!["a.c", "x.h"]);
        assert_eq!(paths(&log, "b.o"), vec!["b.c"]);
    }

    /// A log as two builds left it: `a` and `b` depend on `old.h`.
    fn two_outputs(p: &str) {
        let mut log = DepsLog::new();
        log.open_for_write(p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("a", 1, &["old.h"]).unwrap();
        log.record_deps("b", 1, &["old.h"]).unwrap();
        log.close();
    }

    /// Another process appending to the log without taking turns (another
    /// tool, an older ninja-rs), having loaded it before this one wrote:
    /// its new paths get the ids this one's got. An output must never get
    /// the other's deps: the log stops at the first path whose id doesn't
    /// follow, as ninja's does.
    #[test]
    fn two_writers_never_mix_their_paths() {
        let (_dir, p) = tmp("deps-two-writers");
        two_outputs(&p);
        let mut a = DepsLog::new();
        assert_eq!(a.load(&p).unwrap(), None);
        a.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        a.record_deps("a", 2, &["ha.h"]).unwrap();
        a.close();
        // The other one's: its new path, and b's deps on it. Read as the
        // next path, it would give b a's header.
        assert_eq!(a.path_count(), 4);
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"p\t3\thb.h\nd\t2\t2\t3\n").unwrap();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap().as_deref(), Some("premature end of file; recovering"));
        assert_eq!(paths(&log, "a"), vec!["ha.h"]);
        // b's new record is dropped: its old one stands, older than b.
        assert_eq!(paths(&log, "b"), vec!["old.h"]);
        assert_eq!(log.get("b").unwrap().mtime, 1);
        // The next build cuts the rest and goes on.
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("b", 3, &["hb.h"]).unwrap();
        log.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "a"), vec!["ha.h"]);
        assert_eq!(paths(&log, "b"), vec!["hb.h"]);
    }

    /// A build and one it runs in the same directory (see `Access::Shared`)
    /// append at once, each having loaded the log before the other wrote:
    /// each reads what the other appended first, so that both keep their
    /// records, and their new paths get ids of their own.
    #[test]
    fn writers_sharing_a_log_take_turns() {
        let (_dir, p) = tmp("deps-sharing");
        two_outputs(&p);
        let (mut outer, mut inner) = (DepsLog::new(), DepsLog::new());
        assert_eq!(outer.load(&p).unwrap(), None);
        assert_eq!(inner.load(&p).unwrap(), None);
        outer.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        inner.open_for_write(&p, &|_| true, Access::Shared).unwrap();
        outer.record_deps("a", 2, &["ha.h", "common.h"]).unwrap();
        inner.record_deps("b", 2, &["hb.h", "common.h"]).unwrap();
        outer.record_deps("c", 2, &["hc.h", "hb.h"]).unwrap();
        inner.record_deps("d", 2, &["hd.h", "ha.h"]).unwrap();
        // Each knows the other's records, with ids of its own.
        assert_eq!(paths(&outer, "b"), vec!["hb.h", "common.h"]);
        assert_eq!(paths(&inner, "c"), vec!["hc.h", "hb.h"]);
        outer.close();
        inner.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "a"), vec!["ha.h", "common.h"]);
        assert_eq!(paths(&log, "b"), vec!["hb.h", "common.h"]);
        assert_eq!(paths(&log, "c"), vec!["hc.h", "hb.h"]);
        assert_eq!(paths(&log, "d"), vec!["hd.h", "ha.h"]);
        assert_eq!(std::fs::read_to_string(&p).unwrap().matches("common.h").count(), 1);
    }

    /// A shared writer neither recompacts the log, which its lock's holder
    /// appends to, nor cuts what it doesn't read: it appends after records
    /// it reads, or records nothing on the file.
    #[test]
    fn a_shared_writer_neither_recompacts_nor_cuts() {
        let (_dir, p) = tmp("deps-shared");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        for i in 0..1100 {
            log.record_deps("dead.o", i, &["a.h"]).unwrap();
        }
        log.record_deps("a.o", 1, &["a.h"]).unwrap();
        log.close();
        let before = std::fs::read(&p).unwrap();
        let mut shared = DepsLog::new();
        assert_eq!(shared.load(&p).unwrap(), None);
        assert!(shared.needs_recompaction);
        shared.open_for_write(&p, &|o| o != "dead.o", Access::Shared).unwrap();
        shared.record_deps("b.o", 1, &["b.h"]).unwrap();
        shared.close();
        let after = std::fs::read(&p).unwrap();
        assert!(after.starts_with(&before), "rewritten");
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(log.get("dead.o").unwrap().mtime, 1099);
        assert_eq!(paths(&log, "b.o"), vec!["b.h"]);
        // A torn record: kept, and nothing appended after it.
        std::fs::write(&p, &after[..after.len() - 2]).unwrap();
        let torn = std::fs::read(&p).unwrap();
        let mut shared = DepsLog::new();
        assert!(shared.load(&p).unwrap().is_some());
        shared.open_for_write(&p, &|_| true, Access::Shared).unwrap();
        shared.record_deps("c.o", 1, &["c.h"]).unwrap();
        shared.close();
        assert_eq!(std::fs::read(&p).unwrap(), torn);
        assert_eq!(paths(&shared, "c.o"), vec!["c.h"]);
    }

    /// A log another process replaced while a build appends to it (a
    /// recompaction without the lock): the build appends to the new file,
    /// with its ids, and its own ids stay as they were.
    #[test]
    fn a_replaced_log_is_opened_again() {
        let (dir, p) = tmp("deps-replaced");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("a", 1, &["a.h"]).unwrap();
        let a_h = log.get("a").unwrap().ids()[0];
        let other = dir.join("other");
        std::fs::write(&other, format!("{SIGNATURE}p\t0\tz.h\np\t1\tx\nd\t1\t5\t0\n")).unwrap();
        std::fs::rename(&other, &p).unwrap();
        log.record_deps("b", 2, &["b.h", "a.h"]).unwrap();
        log.close();
        assert_eq!(log.path(a_h), "a.h");
        assert_eq!(paths(&log, "x"), vec!["z.h"]);
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "x"), vec!["z.h"]);
        assert_eq!(paths(&log, "b"), vec!["b.h", "a.h"]);
        assert!(log.get("a").is_none());
    }

    #[test]
    fn torn_path_does_not_merge_with_the_next_append() {
        let (_dir, p) = tmp("deps-torn-path");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("a.o", 100, &["a.c", "common.h"]).unwrap();
        log.record_deps("b.o", 100, &["b.c", "longheadername.h"]).unwrap();
        log.close();
        // Cut inside the last path line: appended to, it would become
        // another path and shift the ids of the paths after it.
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, &text[..text.find("longhead").unwrap() + 4]).unwrap();
        let mut log = DepsLog::new();
        assert!(log.load(&p).unwrap().is_some());
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record_deps("c.o", 200, &["c.c", "c.h"]).unwrap();
        log.record_deps("d.o", 200, &["d.c", "d.h"]).unwrap();
        log.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "a.o"), vec!["a.c", "common.h"]);
        assert!(log.get("b.o").is_none());
        assert_eq!(paths(&log, "c.o"), vec!["c.c", "c.h"]);
        assert_eq!(paths(&log, "d.o"), vec!["d.c", "d.h"]);
    }
}
