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
//! which is handled the same way. Builds take the build directory's lock
//! ([`LogLock`](crate::build_log::LogLock)) so that one at a time writes
//! the logs.

use std::fs::{File, OpenOptions};
use std::io::Write;

use crate::build_log::{file_len, replace_file};
use crate::graph::TimeStamp;
use crate::hash::HashMap;

pub const DEPS_LOG_FILE: &str = ".ninja_rs_deps";
// v3: path records carry their id (v2 had them positional).
const SIGNATURE: &str = "# ninja-rs deps v3\n";
const RECOVERING: &str = "premature end of file; recovering";

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
    /// Paths with an id `>=` this are not in the file yet.
    paths_written: usize,
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
        id
    }

    /// Loads the log; a missing file is not an error. Returns a warning for
    /// an incompatible log (which is then ignored), or one whose end was
    /// left out (see the module's documentation).
    pub fn load(&mut self, path: &str) -> Result<Option<String>, String> {
        self.path = path.to_string();
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("loading deps log {path}: {e}")),
        };
        self.loaded_len = Some(data.len() as u64);
        if !data.starts_with(SIGNATURE.as_bytes()) {
            self.needs_recompaction = true;
            return Ok(Some("deps log version is too old or invalid; starting over".into()));
        }
        let mut offset = SIGNATURE.len();
        let mut total = 0usize;
        // Complete lines only: a torn one may still parse (a record missing
        // ids, a path cut short).
        while let Some(len) = data[offset..].iter().position(|&b| b == b'\n') {
            let Ok(line) = std::str::from_utf8(&data[offset..offset + len]) else { break };
            let fits = if let Some(p) = line.strip_prefix("p\t") {
                self.read_path(p)
            } else if let Some(d) = line.strip_prefix("d\t") {
                self.parse_record(d)
                    .map(|(out, record)| {
                        total += 1;
                        self.deps.insert(out, record);
                    })
                    .is_some()
            } else {
                false
            };
            if !fits {
                break;
            }
            offset += len + 1;
        }
        // Everything read is on disk.
        self.paths_written = self.paths.len();
        if offset < data.len() {
            self.truncate_to = Some(offset as u64);
            return Ok(Some(RECOVERING.into()));
        }
        if total > 1000 && total > self.deps.len() * 3 {
            self.needs_recompaction = true;
        }
        Ok(None)
    }

    /// Reads a path record, `<id>\t<path>`: its id must be the next one and
    /// its path new, else another process wrote it with ids of its own.
    fn read_path(&mut self, record: &str) -> bool {
        let Some((id, path)) = record.split_once('\t') else { return false };
        if id.parse::<usize>().ok() != Some(self.paths.len()) || path.is_empty() || self.ids.contains_key(path) {
            return false;
        }
        self.intern(path);
        true
    }

    fn parse_record(&self, d: &str) -> Option<(u32, Record)> {
        let mut f = d.split('\t');
        let out: u32 = f.next()?.parse().ok()?;
        let mtime = f.next()?.parse().ok()?;
        let ids = f.next().unwrap_or("");
        let n = self.paths.len() as u32;
        let mut nodes = Vec::new();
        for id in ids.split(' ').filter(|s| !s.is_empty()) {
            let id: u32 = id.parse().ok()?;
            if id >= n {
                return None;
            }
            nodes.push(id);
        }
        (out < n).then_some((out, Record { mtime, nodes }))
    }

    /// Prepares for appending; recompacts first if needed. `is_live` tells
    /// whether an output still exists in the manifest.
    pub fn open_for_write(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool) -> Result<(), String> {
        self.path = path.to_string();
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

    fn ensure_open(&mut self) -> Result<&mut File, String> {
        if self.file.is_none() {
            self.cut_torn_line()?;
            if let Some(parent) = std::path::Path::new(&self.path).parent()
                && !parent.as_os_str().is_empty()
            {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut f = OpenOptions::new().create(true).append(true).open(&self.path).map_err(|e| format!("opening deps log: {e}"))?;
            if f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                f.write_all(SIGNATURE.as_bytes()).map_err(|e| e.to_string())?;
                // A fresh file holds none of the interned paths.
                self.paths_written = 0;
            }
            self.file = Some(f);
        }
        Ok(self.file.as_mut().expect("opened"))
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
            let mut text = String::new();
            for (id, p) in self.paths.iter().enumerate().skip(self.paths_written) {
                push_path(&mut text, id, p);
            }
            push_record(&mut text, out, &record);
            let f = self.file.as_mut().expect("opened");
            f.write_all(text.as_bytes()).map_err(|e| format!("Error writing to deps log: {e}"))?;
            self.paths_written = self.paths.len();
        }
        self.deps.insert(out, record);
        Ok(())
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
        let mut out = String::from(SIGNATURE);
        for (output, record) in live {
            let first_new = self.paths.len();
            let out_id = self.intern(&output);
            let nodes: Vec<u32> = record.nodes.iter().map(|&id| self.intern(&old_paths[id as usize])).collect();
            for (id, p) in self.paths.iter().enumerate().skip(first_new) {
                push_path(&mut out, id, p);
            }
            let record = Record { mtime: record.mtime, nodes };
            push_record(&mut out, out_id, &record);
            self.deps.insert(out_id, record);
        }
        replace_file(path, out.as_bytes())?;
        self.paths_written = self.paths.len();
        self.needs_recompaction = false;
        self.truncate_to = None;
        self.loaded_len = Some(out.len() as u64);
        Ok(())
    }
}

fn push_path(out: &mut String, id: usize, path: &str) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "p\t{id}\t{path}");
}

fn push_record(out: &mut String, output: u32, record: &Record) {
    use std::fmt::Write as _;
    let _ = write!(out, "d\t{output}\t{}\t", record.mtime);
    for (i, id) in record.nodes.iter().enumerate() {
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

    fn tmp(name: &str) -> String {
        let p = test_dirs::base().join(format!("ninja-rs-test-{}-{name}", std::process::id()));
        let p = p.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&p);
        p
    }

    fn paths(log: &DepsLog, output: &str) -> Vec<String> {
        log.get(output).unwrap().paths().map(str::to_string).collect()
    }

    #[test]
    fn write_read_and_skip_unchanged() {
        let p = tmp("deps");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
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
        log2.open_for_write(&p, &|_| true).unwrap();
        log2.record_deps("out4.o", 4, &["foo.h", "new.h"]).unwrap();
        log2.close();
        let mut log3 = DepsLog::new();
        log3.load(&p).unwrap();
        assert_eq!(paths(&log3, "out4.o"), vec!["foo.h", "new.h"]);
        assert_eq!(paths(&log3, "out.o"), vec!["foo.h", "bar.h"]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn recompact_keeps_live_outputs_and_their_paths() {
        let p = tmp("deps-recompact");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
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
        let _ = std::fs::remove_file(&p);
    }

    /// Reading stops at the first record that doesn't fit, as in ninja:
    /// what follows was written by another process, or is garbage.
    #[test]
    fn a_record_that_does_not_fit_ends_the_log() {
        let p = tmp("deps-corrupt");
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
        let _ = std::fs::remove_file(&p);
    }

    /// A log another process wrote to after it was loaded isn't appended to
    /// with the ids read (nor cut where its end was): it's rewritten.
    #[test]
    fn a_log_changed_since_loaded_is_rewritten() {
        let p = tmp("deps-changed");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
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
        stale.open_for_write(&p, &|_| true).unwrap();
        stale.record_deps("b", 2, &["b.h"]).unwrap();
        stale.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert!(log.get("a").is_none());
        assert_eq!(paths(&log, "b"), vec!["b.h"]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn torn_record_is_ignored_and_cut() {
        let p = tmp("deps-torn-record");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
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
        log.open_for_write(&p, &|_| true).unwrap();
        log.close();
        assert!(std::fs::read(&p).unwrap().ends_with(b"\n"));
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        log.open_for_write(&p, &|_| true).unwrap();
        log.record_deps("a.o", 200, &["a.c", "x.h"]).unwrap();
        log.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "a.o"), vec!["a.c", "x.h"]);
        assert_eq!(paths(&log, "b.o"), vec!["b.c"]);
        let _ = std::fs::remove_file(&p);
    }

    /// Two builds appending to one log, each having loaded it before the
    /// other wrote (without the build directory's lock: another tool, an
    /// older ninja-rs): both give their new paths the same ids. An output
    /// must never get the other build's deps: the log stops at the first
    /// path whose id doesn't follow, as ninja's does.
    #[test]
    fn two_writers_never_mix_their_paths() {
        let p = tmp("deps-two-writers");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
        log.record_deps("a", 1, &["old.h"]).unwrap();
        log.record_deps("b", 1, &["old.h"]).unwrap();
        log.close();
        let (mut a, mut b) = (DepsLog::new(), DepsLog::new());
        assert_eq!(a.load(&p).unwrap(), None);
        assert_eq!(b.load(&p).unwrap(), None);
        a.open_for_write(&p, &|_| true).unwrap();
        b.open_for_write(&p, &|_| true).unwrap();
        a.record_deps("a", 2, &["ha.h", "common.h"]).unwrap();
        b.record_deps("b", 2, &["hb.h", "common.h"]).unwrap();
        a.close();
        b.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap().as_deref(), Some("premature end of file; recovering"));
        assert_eq!(paths(&log, "a"), vec!["ha.h", "common.h"]);
        // b's new record is dropped: its old one stands, older than b.
        assert_eq!(paths(&log, "b"), vec!["old.h"]);
        assert_eq!(log.get("b").unwrap().mtime, 1);
        // The next build cuts the rest and goes on.
        log.open_for_write(&p, &|_| true).unwrap();
        log.record_deps("b", 3, &["hb.h", "common.h"]).unwrap();
        log.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "a"), vec!["ha.h", "common.h"]);
        assert_eq!(paths(&log, "b"), vec!["hb.h", "common.h"]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn torn_path_does_not_merge_with_the_next_append() {
        let p = tmp("deps-torn-path");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
        log.record_deps("a.o", 100, &["a.c", "common.h"]).unwrap();
        log.record_deps("b.o", 100, &["b.c", "longheadername.h"]).unwrap();
        log.close();
        // Cut inside the last path line: appended to, it would become
        // another path and shift the ids of the paths after it.
        let text = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, &text[..text.find("longhead").unwrap() + 4]).unwrap();
        let mut log = DepsLog::new();
        assert!(log.load(&p).unwrap().is_some());
        log.open_for_write(&p, &|_| true).unwrap();
        log.record_deps("c.o", 200, &["c.c", "c.h"]).unwrap();
        log.record_deps("d.o", 200, &["d.c", "d.h"]).unwrap();
        log.close();
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "a.o"), vec!["a.c", "common.h"]);
        assert!(log.get("b.o").is_none());
        assert_eq!(paths(&log, "c.o"), vec!["c.c", "c.h"]);
        assert_eq!(paths(&log, "d.o"), vec!["d.c", "d.h"]);
        let _ = std::fs::remove_file(&p);
    }
}
