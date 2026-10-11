//! The deps log: dependencies discovered by `deps = gcc` / `deps = msvc`
//! edges (ninja's `.ninja_deps`).
//!
//! ninja-rs uses its own text format in `.ninja_rs_deps`. Like ninja's binary
//! log, each path is stored once and deps refer to paths by id, which keeps
//! the file (and memory) small when thousands of objects share headers:
//!
//! ```text
//! # ninja-rs deps v2
//! p\t<path>                         defines the next path id (0, 1, ...)
//! d\t<output id>\t<mtime ns>\t<id> <id> ...
//! ```
//!
//! Path records are appended just before the first deps record using them.

use std::fs::{File, OpenOptions};
use std::io::Write;

use crate::graph::TimeStamp;
use crate::hash::HashMap;

pub const DEPS_LOG_FILE: &str = ".ninja_rs_deps";
const SIGNATURE: &str = "# ninja-rs deps v2\n";

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

    pub fn load(&mut self, path: &str) -> Result<Option<String>, String> {
        self.path = path.to_string();
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("loading deps log {path}: {e}")),
        };
        let text = String::from_utf8_lossy(&data);
        if !text.starts_with(SIGNATURE) {
            self.needs_recompaction = true;
            return Ok(Some("deps log version is too old or invalid; starting over".into()));
        }
        let mut total = 0usize;
        for line in text[SIGNATURE.len()..].split('\n') {
            if let Some(p) = line.strip_prefix("p\t") {
                self.intern(p);
            } else if let Some(d) = line.strip_prefix("d\t") {
                // A truncated or corrupt record is skipped, like ninja does.
                if let Some(record) = self.parse_record(d) {
                    total += 1;
                    self.deps.insert(record.0, record.1);
                }
            }
        }
        // Everything read is on disk.
        self.paths_written = self.paths.len();
        if total > 1000 && total > self.deps.len() * 3 {
            self.needs_recompaction = true;
        }
        Ok(None)
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

    pub fn open_for_write(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool) -> Result<(), String> {
        self.path = path.to_string();
        if self.needs_recompaction {
            self.recompact(path, is_live)?;
        }
        Ok(())
    }

    fn ensure_open(&mut self) -> Result<&mut File, String> {
        if self.file.is_none() {
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
            for p in &self.paths[self.paths_written..] {
                text.push_str("p\t");
                text.push_str(p);
                text.push('\n');
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
            for p in &self.paths[first_new..] {
                out.push_str("p\t");
                out.push_str(p);
                out.push('\n');
            }
            let record = Record { mtime: record.mtime, nodes };
            push_record(&mut out, out_id, &record);
            self.deps.insert(out_id, record);
        }
        let tmp = format!("{path}.recompact");
        std::fs::write(&tmp, out).map_err(|e| format!("writing {tmp}: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("renaming {tmp}: {e}"))?;
        self.paths_written = self.paths.len();
        self.needs_recompaction = false;
        Ok(())
    }
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
        let p = std::env::temp_dir().join(format!("ninja-rs-test-{}-{name}", std::process::id()));
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

    #[test]
    fn corrupt_records_are_skipped_and_old_versions_ignored() {
        let p = tmp("deps-corrupt");
        std::fs::write(&p, format!("{SIGNATURE}p\tout.o\np\ta.h\nd\t0\t5\t1\nd\t0\t6\t9\nd\t7\t1\t\n")).unwrap();
        let mut log = DepsLog::new();
        log.load(&p).unwrap();
        // The record naming path 9 is out of range; the first one stands.
        assert_eq!(log.get("out.o").unwrap().mtime, 5);
        std::fs::write(&p, "# ninja-rs deps v1\nout.o\t1\ta.h\n").unwrap();
        let mut old = DepsLog::new();
        assert!(old.load(&p).unwrap().is_some());
        assert!(old.get("out.o").is_none());
        let _ = std::fs::remove_file(&p);
    }
}
