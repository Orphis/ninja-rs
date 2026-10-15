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
//! rest is cut off before the next append: those outputs build again.
//!
//! A write cut short (a crash, a kill, a full disk) leaves a torn last
//! line, which may still parse (ids cut short), and which readers leave
//! out. The next build cuts it off; a writer appending meanwhile ends it
//! with a NUL and a newline first, which no path or number holds: readers
//! skip such a line, and its record (whose output builds again), and go on.
//!
//! Builds take the build directory's lock
//! ([`LogLock`](crate::build_log::LogLock)) so that one at a time writes
//! the logs, but for the builds the holder runs (see
//! [`Access::Shared`]). Those append at once: each takes a lock on the file
//! itself while it appends, and reads first what the others appended, so
//! that its new paths get the next ids. A log's ids are its own, stable
//! while it is loaded; it maps them to the file's.
//!
//! Records are written in batches ([`DepsLog::flush`]): those of the
//! commands one turn of the build loop reaped, under one lock, in one
//! write. When the file goes on with what can't be read (a process
//! appending without taking turns), the lock's holder rewrites it from
//! what it knows, and the builds it runs write their records to the new
//! file; until then, theirs wait in memory. A record waiting to be written
//! is the build's newest: one read meanwhile doesn't replace it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;

use crate::build_log::{Access, REPLACED_CHECK_INTERVAL, file_len, open_for_append, replace_file};
use crate::graph::TimeStamp;
use crate::hash::HashMap;

pub const DEPS_LOG_FILE: &str = ".ninja_rs_deps";
// v3: path records carry their id (v2 had them positional).
const SIGNATURE: &str = "# ninja-rs deps v3\n";
const RECOVERING: &str = "premature end of file; recovering";
/// The file id of a path the file doesn't hold.
const NOT_IN_FILE: u32 = u32::MAX;
/// What ends a torn last line another writer left (see the module's
/// documentation): readers skip a line ending with a NUL.
const VOID: &str = "\0\n";
/// How many times a build rewrites a file it keeps finding unreadable (a
/// process appending without taking turns all along, a file system without
/// locks) before it stops writing to it: each rewrite writes it all.
const MAX_REWRITES: u32 = 10;

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
    /// What follows them is a torn last line only, which a shared writer
    /// ends (see `follow`) rather than stop.
    torn_tail: bool,
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
    /// before what would be appended, so records stay in memory, until the
    /// file is replaced (see `cant_follow`).
    stopped: bool,
    /// How this log may write the file (see `open_for_write`).
    access: Access,
    /// The outputs whose records `flush` writes next, and by this log's
    /// ids whether one is among them: a record read from the file doesn't
    /// replace it (see `read_deps`).
    pending: Vec<u32>,
    is_pending: Vec<bool>,
    /// The outputs recorded since the log was opened for writing: a file
    /// replacing the one they went to gets them again.
    recorded: Vec<u32>,
    /// When `ensure_open` last checked that the file wasn't replaced (None:
    /// at the next flush).
    checked: Option<Instant>,
    /// The rewrites made (see `cant_follow`), and whether they, and a
    /// stop, were said.
    rewrites: u32,
    said_rewritten: bool,
    said_stopped: bool,
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
        self.is_pending.push(false);
        id
    }

    /// Makes `outputs` the records `flush` writes next.
    fn set_pending(&mut self, outputs: Vec<u32>) {
        for &out in &self.pending {
            self.is_pending[out as usize] = false;
        }
        for &out in &outputs {
            self.is_pending[out as usize] = true;
        }
        self.pending = outputs;
    }

    /// Loads the log; a missing file is not an error. Returns a warning for
    /// an incompatible log (which is then ignored), or one whose end was
    /// left out (see the module's documentation).
    pub fn load(&mut self, path: &str) -> Result<Option<String>, String> {
        self.path = path.to_string();
        // Opened for writing too, if this user may: on NFS, a lock (see
        // `with_lock`) is taken on files open for writing only.
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Err(e) if matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) => File::open(path),
            opened => opened,
        };
        let file = match file {
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
            self.torn_tail = !data[end..].contains(&b'\n');
            return Ok(Some(RECOVERING.into()));
        }
        if total > 1000 && total > self.deps.len() * 3 {
            self.needs_recompaction = true;
        }
        Ok(None)
    }

    /// Reads the complete lines of `data` from `offset`, up to the first
    /// that doesn't fit, skipping torn lines a writer ended (see `VOID`).
    /// Returns where they end, and how many deps records they hold.
    fn read_records(&mut self, data: &[u8], mut offset: usize) -> (usize, usize) {
        let mut total = 0;
        while let Some(len) = data[offset..].iter().position(|&b| b == b'\n') {
            if len > 0 && data[offset + len - 1] == 0 {
                offset += len + 1;
                continue;
            }
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
    /// file's ids. It doesn't replace a record of this log's waiting to be
    /// written, which is newer: than another writer's made meanwhile, or
    /// the holder's older one, in the file it rewrote while this log was
    /// stopped.
    fn read_deps(&mut self, record: &str) -> Option<()> {
        let mut f = record.split('\t');
        let ours = |id: &str| self.from_file.get(id.parse::<usize>().ok()?).copied();
        let out = ours(f.next()?)?;
        let mtime = f.next()?.parse().ok()?;
        let nodes = f.next().unwrap_or("").split(' ').filter(|s| !s.is_empty()).map(ours).collect::<Option<Vec<u32>>>()?;
        if !self.is_pending[out as usize] {
            self.deps.insert(out, Record { mtime, nodes });
        }
        Some(())
    }

    /// Prepares for appending, as `access` allows: an exclusive writer
    /// recompacts first if needed (`is_live` tells whose deps stay) and
    /// cuts off what `load` left out; a shared one does neither: it ends a
    /// torn last line (see `follow`), and if `load` left out more, writes
    /// nothing to the file until the holder of the lock rewrites it (see
    /// `cant_follow`).
    pub fn open_for_write(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool, access: Access) -> Result<(), String> {
        self.path = path.to_string();
        self.access = access;
        if access == Access::Shared {
            self.stopped = self.truncate_to.take().is_some() && !self.torn_tail;
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
    /// another process replaced it (rewrote it: the lock's holder, see
    /// `cant_follow`, or a process without the lock). That is checked once
    /// per `REPLACED_CHECK_INTERVAL`, at each flush once stopped, or at the
    /// next after `recheck`. A file other than the one written to is
    /// followed from its start, and gets every record made since the log
    /// was opened for writing.
    fn ensure_open(&mut self) -> Result<(), String> {
        let due = self.stopped || self.checked.is_none_or(|t| t.elapsed() >= REPLACED_CHECK_INTERVAL);
        if due && let Some(f) = &self.file {
            self.checked = Some(Instant::now());
            if ninja_os::replaced(f, Path::new(&self.path)) {
                self.file = None;
            }
        }
        if self.file.is_none() {
            self.cut_torn_line()?;
            let f = open_for_append(&self.path).map_err(|e| format!("opening deps log: {e}"))?;
            self.checked = Some(Instant::now());
            let identity = ninja_os::identity(&f).ok();
            if identity.is_none() || identity != self.known {
                self.known = identity;
                self.file_ids.fill(NOT_IN_FILE);
                self.from_file.clear();
                self.file_end = 0;
                self.stopped = false;
                self.set_pending(self.recorded.clone());
            }
            self.file = Some(f);
        }
        Ok(())
    }

    /// Makes the next `flush` check first that the file wasn't replaced
    /// (a generator ran, which may have rewritten the logs; the build
    /// ends).
    pub fn recheck(&mut self) {
        self.checked = None;
    }

    /// Records the deps of `output`, which `flush` writes; nothing if they
    /// didn't change.
    pub fn record_deps<S: AsRef<str>>(&mut self, output: &str, mtime: TimeStamp, nodes: &[S]) -> Result<(), String> {
        let out = self.intern(output);
        let record = Record { mtime, nodes: nodes.iter().map(|n| self.intern(n.as_ref())).collect() };
        if self.deps.get(&out) == Some(&record) {
            return Ok(());
        }
        if !self.path.is_empty() {
            self.pending.push(out);
            self.is_pending[out as usize] = true;
            self.recorded.push(out);
        }
        self.deps.insert(out, record);
        Ok(())
    }

    /// Writes the records made since the last flush, and the paths the
    /// file doesn't hold yet, holding the file's lock (see the module's
    /// documentation), in one write: the build loop flushes once per turn,
    /// for the commands it reaped. Returns a warning, once, when the file
    /// goes on with what this log can't read (see `cant_follow`).
    pub fn flush(&mut self) -> Result<Option<String>, String> {
        // With nothing to write, but after `recheck`: records written to a
        // file replaced since go to the new one.
        let recheck = self.checked.is_none() && self.file.is_some();
        if self.path.is_empty() || (self.pending.is_empty() && !recheck) {
            return Ok(None);
        }
        self.ensure_open()?;
        if self.pending.is_empty() {
            return Ok(None);
        }
        if self.stopped {
            return Ok(self.say_stopped());
        }
        let file = self.file.take().expect("opened");
        let written = with_lock(&file, || {
            let mut text = String::new();
            match self.follow(&file)? {
                Followed::All => {}
                Followed::Torn => text.push_str(VOID),
                Followed::Not => return Ok(false),
            }
            if self.file_end == 0 {
                text.push_str(SIGNATURE);
            }
            let mut nodes = Vec::new();
            for out in std::mem::take(&mut self.pending) {
                self.is_pending[out as usize] = false;
                let record = &self.deps[&out];
                let mtime = record.mtime;
                nodes.clone_from(&record.nodes);
                let out = self.file_id(out, &mut text);
                for id in &mut nodes {
                    *id = self.file_id(*id, &mut text);
                }
                push_record(&mut text, out, mtime, &nodes);
            }
            (&file).write_all(text.as_bytes())?;
            self.file_end += text.len() as u64;
            Ok(true)
        });
        self.file = Some(file);
        if written.map_err(|e| format!("Error writing to deps log: {e}"))? { Ok(None) } else { self.cant_follow() }
    }

    /// The file goes on with what this log can't read (a process appending
    /// without taking turns, on a file system without locks too), or was
    /// cut. Appended to, its records would be lost to
    /// every reader. The build holding the directory's lock rewrites it
    /// from what it read and recorded, and goes on (up to `MAX_REWRITES`
    /// times); a build it runs (see `Access::Shared`), which may not, stops
    /// writing to it until the holder has: its records wait in memory, and
    /// go to the new file (see `ensure_open`).
    fn cant_follow(&mut self) -> Result<Option<String>, String> {
        if self.access == Access::Exclusive && self.rewrites < MAX_REWRITES {
            self.rewrites += 1;
            self.rewrite()?;
            if std::mem::replace(&mut self.said_rewritten, true) {
                return Ok(None);
            }
            return Ok(Some(format!("deps log {}: what was appended to it can't be read ({UNREADABLE}): rewritten", self.path)));
        }
        self.stopped = true;
        Ok(self.say_stopped())
    }

    /// What a log that stopped writing says, once.
    fn say_stopped(&mut self) -> Option<String> {
        if std::mem::replace(&mut self.said_stopped, true) {
            return None;
        }
        let until = match self.access {
            Access::Exclusive => "not written to any more: the outputs built meanwhile build again next time",
            Access::Shared => "not written to until the build holding its lock rewrites it (else the outputs built meanwhile build again next time)",
        };
        Some(format!("deps log {}: what was appended to it can't be read ({UNREADABLE}): {until}", self.path))
    }

    /// Rewrites the file from this log: what it read of the file and what
    /// it recorded, with its own ids (stable while the build runs), every
    /// path (those of outputs whose deps are gone too).
    fn rewrite(&mut self) -> Result<(), String> {
        self.file = None;
        let mut text = String::from(SIGNATURE);
        for (id, p) in self.paths.iter().enumerate() {
            push_path(&mut text, id as u32, p);
        }
        let mut outputs: Vec<u32> = self.deps.keys().copied().collect();
        outputs.sort_unstable();
        for out in outputs {
            let r = &self.deps[&out];
            push_record(&mut text, out, r.mtime, &r.nodes);
        }
        replace_file(&self.path, text.as_bytes())?;
        self.from_file = (0..self.paths.len() as u32).collect();
        self.file_ids.clone_from(&self.from_file);
        self.file_end = text.len() as u64;
        self.known = File::open(&self.path).and_then(|f| ninja_os::identity(&f)).ok();
        self.set_pending(Vec::new());
        self.stopped = false;
        self.truncate_to = None;
        self.loaded_len = Some(text.len() as u64);
        Ok(())
    }

    /// Reads what other processes appended to `file` since this log last
    /// read or wrote it.
    fn follow(&mut self, file: &File) -> std::io::Result<Followed> {
        let mut f = file;
        // Where it ends, as a seek there tells: its metadata, asked at
        // every write, takes three times as long. Appends go to the end
        // wherever the position is.
        let len = f.seek(SeekFrom::End(0))?;
        if len == self.file_end {
            return Ok(Followed::All);
        }
        if len < self.file_end {
            return Ok(Followed::Not);
        }
        let mut data = Vec::new();
        f.seek(SeekFrom::Start(self.file_end))?;
        f.take(len - self.file_end).read_to_end(&mut data)?;
        let start = if self.file_end > 0 {
            0
        } else if data.starts_with(SIGNATURE.as_bytes()) {
            SIGNATURE.len()
        } else {
            return Ok(Followed::Not);
        };
        let (end, _) = self.read_records(&data, start);
        self.file_end += end as u64;
        if end == data.len() {
            return Ok(Followed::All);
        }
        if data[end..].contains(&b'\n') {
            return Ok(Followed::Not);
        }
        // What the write ending it follows.
        self.file_end = len;
        Ok(Followed::Torn)
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

    /// Writes the records left (see `flush`, whose errors are lost: the
    /// build flushes before) and closes the file.
    pub fn close(&mut self) {
        self.recheck();
        let _ = self.flush();
        self.file = None;
    }

    /// Rewrites the log with only the live outputs and the paths they use.
    pub fn recompact(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool) -> Result<(), String> {
        self.close();
        // Its ids change: the records are in the new file, or dead.
        self.recorded.clear();
        self.pending.clear();
        let mut live: Vec<(String, Record)> = std::mem::take(&mut self.deps)
            .into_iter()
            .filter(|(id, _)| is_live(&self.paths[*id as usize]))
            .map(|(id, r)| (self.paths[id as usize].clone(), r))
            .collect();
        live.sort_by(|a, b| a.0.cmp(&b.0));
        let old_paths = std::mem::take(&mut self.paths);
        self.ids.clear();
        self.file_ids.clear();
        self.is_pending.clear();
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

impl Drop for DepsLog {
    /// Writes the records left: a build flushes its own, but for a panic.
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            self.close();
        }
    }
}

/// How much of what other processes appended `DepsLog::follow` read.
enum Followed {
    /// All of it.
    All,
    /// All but a torn last line (one without its newline), which the next
    /// write ends with `VOID`.
    Torn,
    /// Not all: what follows doesn't fit, or the file was cut.
    Not,
}

/// What a writer finding the file going on with what it can't read says
/// may have happened.
const UNREADABLE: &str = "a write cut short, or a process appending without taking turns";

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

    /// Records the deps of `output` and writes them, as a turn of the build
    /// loop does, without a warning.
    fn record(log: &mut DepsLog, output: &str, mtime: TimeStamp, deps: &[&str]) {
        log.record_deps(output, mtime, deps).unwrap();
        assert_eq!(log.flush(), Ok(None));
    }

    /// Records wait for `flush`, which writes them all at once.
    #[test]
    fn records_are_written_at_each_flush() {
        let (_dir, p) = tmp("deps-flush");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        record(&mut log, "a", 1, &["a.h"]);
        let before = std::fs::read(&p).unwrap();
        for out in ["b", "c", "d"] {
            log.record_deps(out, 2, &["common.h"]).unwrap();
        }
        assert_eq!(std::fs::read(&p).unwrap(), before);
        assert_eq!(log.flush(), Ok(None));
        let mut read = DepsLog::new();
        assert_eq!(read.load(&p).unwrap(), None);
        for out in ["b", "c", "d"] {
            assert_eq!(paths(&read, out), vec!["common.h"]);
        }
        // Nothing left: nothing written.
        let written = std::fs::read(&p).unwrap();
        assert_eq!(log.flush(), Ok(None));
        assert_eq!(std::fs::read(&p).unwrap(), written);
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
        record(&mut outer, "a", 2, &["ha.h", "common.h"]);
        record(&mut inner, "b", 2, &["hb.h", "common.h"]);
        record(&mut outer, "c", 2, &["hc.h", "hb.h"]);
        record(&mut inner, "d", 2, &["hd.h", "ha.h"]);
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
        // A torn record: kept, ended, and skipped.
        std::fs::write(&p, &after[..after.len() - 2]).unwrap();
        let torn = std::fs::read(&p).unwrap();
        let mut shared = DepsLog::new();
        assert!(shared.load(&p).unwrap().is_some());
        shared.open_for_write(&p, &|_| true, Access::Shared).unwrap();
        shared.record_deps("c.o", 1, &["c.h"]).unwrap();
        shared.close();
        assert!(std::fs::read(&p).unwrap().starts_with(&torn), "cut");
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        assert_eq!(paths(&log, "c.o"), vec!["c.h"]);
        assert!(log.get("b.o").is_none());
        assert_eq!(log.get("dead.o").unwrap().mtime, 1099);
    }

    /// A log another process replaced while a build appends to it (a
    /// recompaction without the lock, by a generator): the build appends
    /// to the new file, with its ids, every record it made; its own ids
    /// stay as they were.
    #[test]
    fn a_replaced_log_is_opened_again() {
        let (dir, p) = tmp("deps-replaced");
        let mut log = DepsLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        record(&mut log, "a", 1, &["a.h"]);
        let a_h = log.get("a").unwrap().ids()[0];
        let other = dir.join("other");
        std::fs::write(&other, format!("{SIGNATURE}p\t0\tz.h\np\t1\tx\nd\t1\t5\t0\n")).unwrap();
        std::fs::rename(&other, &p).unwrap();
        log.recheck();
        record(&mut log, "b", 2, &["b.h", "a.h"]);
        assert_eq!(log.path(a_h), "a.h");
        assert_eq!(paths(&log, "x"), vec!["z.h"]);
        let read = |p: &str| {
            let mut log = DepsLog::new();
            assert_eq!(log.load(p).unwrap(), None);
            log
        };
        let new = read(&p);
        assert_eq!(paths(&new, "x"), vec!["z.h"]);
        assert_eq!(paths(&new, "b"), vec!["b.h", "a.h"]);
        assert_eq!(paths(&new, "a"), vec!["a.h"]);
        // Replaced again before the end, a record written to the old file
        // since (the check waits a while): closing checks, and writes them
        // all again.
        std::fs::write(&other, format!("{SIGNATURE}p\t0\ty\nd\t0\t5\t\n")).unwrap();
        std::fs::rename(&other, &p).unwrap();
        record(&mut log, "c", 3, &["c.h"]);
        log.close();
        let last = read(&p);
        assert!(last.get("y").is_some_and(|d| d.is_empty()));
        assert_eq!(["a", "b", "c"].map(|o| paths(&last, o)), [vec!["a.h"], vec!["b.h", "a.h"], vec!["c.h"]]);
    }

    /// A process appending without taking turns: the holder of the lock
    /// rewrites the file from what it read and recorded, says so once, and
    /// goes on. It stopped writing for the rest of its build, silently:
    /// those outputs built again next time.
    #[test]
    fn the_holder_rewrites_a_log_it_cant_read() {
        let (_dir, p) = tmp("deps-rewrite");
        two_outputs(&p);
        let mut log = DepsLog::new();
        assert_eq!(log.load(&p).unwrap(), None);
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        record(&mut log, "c", 2, &["c.h", "old.h"]);
        let ids: Vec<u32> = log.get("c").unwrap().ids().to_vec();
        // A record of its ids: path 9 isn't the file's.
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"d\t1\t5\t1 9\n").unwrap();
        log.record_deps("d", 2, &["d.h"]).unwrap();
        let warning = log.flush().unwrap().unwrap_or_default();
        assert!(warning.starts_with(&format!("deps log {p}: what was appended to it can't be read (")) && warning.ends_with("): rewritten"), "{warning}");
        // Its ids stay.
        assert_eq!(log.get("c").unwrap().ids(), ids);
        // Again: a rewrite, without a warning.
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"p\t9\tx.h\n").unwrap();
        log.record_deps("e", 2, &["e.h"]).unwrap();
        assert_eq!(log.flush(), Ok(None));
        record(&mut log, "f", 2, &["f.h", "c.h"]);
        let mut read = DepsLog::new();
        assert_eq!(read.load(&p).unwrap(), None);
        for (out, deps) in
            [("a", vec!["old.h"]), ("b", vec!["old.h"]), ("c", vec!["c.h", "old.h"]), ("d", vec!["d.h"]), ("e", vec!["e.h"]), ("f", vec!["f.h", "c.h"])]
        {
            assert_eq!(paths(&read, out), deps, "{out}");
        }
        assert!(read.get("x.h").is_none() && !std::fs::read_to_string(&p).unwrap().contains("x.h"));
    }

    /// A build the holder of the lock runs, finding a log it can't read,
    /// may not rewrite it: it says so, once, and its records wait until the
    /// holder has. Then they go to the new file, those it wrote to the old
    /// one before too.
    #[test]
    fn a_shared_writer_waits_for_the_rewrite() {
        let (_dir, p) = tmp("deps-shared-rewrite");
        two_outputs(&p);
        let (mut outer, mut inner) = (DepsLog::new(), DepsLog::new());
        assert_eq!(outer.load(&p).unwrap(), None);
        assert_eq!(inner.load(&p).unwrap(), None);
        outer.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        inner.open_for_write(&p, &|_| true, Access::Shared).unwrap();
        record(&mut inner, "c", 2, &["c.h"]);
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"p\t9\tx.h\n").unwrap();
        inner.record_deps("d", 2, &["d.h"]).unwrap();
        let warning = inner.flush().unwrap().unwrap_or_default();
        assert!(
            warning.ends_with("not written to until the build holding its lock rewrites it (else the outputs built meanwhile build again next time)"),
            "{warning}"
        );
        inner.record_deps("e", 2, &["e.h"]).unwrap();
        assert_eq!(inner.flush(), Ok(None));
        let mut read = DepsLog::new();
        assert!(read.load(&p).unwrap().is_some());
        assert!(read.get("d").is_none() && read.get("e").is_none());
        // The holder rewrites it, with c (which it read), not d and e.
        outer.record_deps("f", 2, &["f.h"]).unwrap();
        assert!(outer.flush().unwrap().is_some());
        record(&mut inner, "g", 2, &["g.h"]);
        for log in [&mut outer, &mut inner] {
            log.close();
        }
        let mut read = DepsLog::new();
        assert_eq!(read.load(&p).unwrap(), None);
        for out in ["a", "b", "c", "d", "e", "f", "g"] {
            assert!(read.get(out).is_some(), "{out} isn't recorded");
        }
        assert_eq!(paths(&read, "e"), vec!["e.h"]);
    }

    /// A build the holder of the lock runs, stopped by a line it can't
    /// read, builds an output again, whose older record the holder then
    /// rewrites the log with: its own, newer, stays, in the file and in its
    /// view. (Read again, the holder's replaced it: the output built again
    /// next time, or its stale deps were trusted.)
    #[test]
    fn a_shared_writer_keeps_its_newer_records_after_the_rewrite() {
        let (_dir, p) = tmp("deps-shared-newer");
        two_outputs(&p);
        let (mut outer, mut inner) = (DepsLog::new(), DepsLog::new());
        assert_eq!(outer.load(&p).unwrap(), None);
        assert_eq!(inner.load(&p).unwrap(), None);
        outer.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        inner.open_for_write(&p, &|_| true, Access::Shared).unwrap();
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"p\t9\tx.h\n").unwrap();
        // `a` reads new.h now.
        inner.record_deps("a", 7, &["new.h"]).unwrap();
        assert!(inner.flush().unwrap().is_some());
        // The holder rewrites the log, with the record of `a` it loaded.
        outer.record_deps("c", 2, &["c.h"]).unwrap();
        assert!(outer.flush().unwrap().is_some());
        record(&mut inner, "d", 2, &["d.h"]);
        let newer = (vec!["new.h".to_string()], 7);
        assert_eq!((paths(&inner, "a"), inner.get("a").unwrap().mtime), newer);
        for log in [&mut outer, &mut inner] {
            log.close();
        }
        let mut read = DepsLog::new();
        assert_eq!(read.load(&p).unwrap(), None);
        assert_eq!((paths(&read, "a"), read.get("a").unwrap().mtime), newer);
        assert_eq!((paths(&read, "c"), paths(&read, "d")), (vec!["c.h".to_string()], vec!["d.h".to_string()]));
    }

    /// A torn line (a writer killed, a full disk) stops no writer: the next
    /// one ends it, and readers skip it, and its record. (A build the
    /// holder of the lock runs stopped, its records waiting for a rewrite
    /// the holder made only when it had records of its own to write: lost
    /// when it had none, its only command being that build.)
    #[test]
    fn a_torn_line_stops_no_writer() {
        let (_dir, p) = tmp("deps-torn-shared");
        two_outputs(&p);
        let (mut outer, mut inner) = (DepsLog::new(), DepsLog::new());
        assert_eq!(outer.load(&p).unwrap(), None);
        assert_eq!(inner.load(&p).unwrap(), None);
        outer.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        inner.open_for_write(&p, &|_| true, Access::Shared).unwrap();
        record(&mut inner, "c", 2, &["c.h"]);
        // A writer killed in its write: a new path, and a record of b cut
        // inside its ids (`5 1` of `5 12`), which would still parse.
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"p\t5\tk.h\nd\t2\t9\t5 1").unwrap();
        record(&mut inner, "d", 2, &["d.h", "k.h"]);
        record(&mut outer, "e", 2, &["e.h", "k.h"]);
        for log in [&mut outer, &mut inner] {
            log.close();
        }
        let mut read = DepsLog::new();
        assert_eq!(read.load(&p).unwrap(), None);
        assert_eq!((paths(&read, "b"), read.get("b").unwrap().mtime), (vec!["old.h".to_string()], 1));
        for (out, deps) in [("a", vec!["old.h"]), ("c", vec!["c.h"]), ("d", vec!["d.h", "k.h"]), ("e", vec!["e.h", "k.h"])] {
            assert_eq!(paths(&read, out), deps, "{out}");
        }
        // Ended once.
        assert_eq!(std::fs::read(&p).unwrap().iter().filter(|&&b| b == 0).count(), 1);
        // The next build cuts nothing: the line ended is one it skips.
        let len = std::fs::metadata(&p).unwrap().len();
        read.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        read.close();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), len);
    }

    /// What a writer of `writers_in_processes` (see `writer`) does: the
    /// environment variable naming it, set in its process.
    const WRITER_ENV: &str = "NINJA_CORE_TEST_DEPS_WRITER";

    /// The deps writer `w` gives output `i`: headers all share, and its own.
    fn expected(w: usize, i: usize) -> Vec<String> {
        (0..3 + (i * 7 + w) % 5)
            .map(|j| if j % 2 == 0 { format!("common/h{}.h", (i * 31 + j * 17) % 300) } else { format!("w{w}/h{}_{j}.h", i % 50) })
            .collect()
    }

    /// Runs `test` as `writers` processes appending to the log `path` at
    /// once (this test binary again, see `writer`): writer 0 holds the
    /// lock, the others are builds it runs. Returns, when they are done,
    /// what each found wrong in the records it read (the others' included).
    fn writers_in_processes(test: &str, path: &str, writers: usize, records: usize, torn: Option<usize>) -> Vec<String> {
        let mut children: Vec<_> = (0..writers)
            .map(|w| {
                let spec = format!("{path}|{w}|{writers}|{records}|{}", torn.map_or(String::new(), |t| t.to_string()));
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([test, "--exact", "--nocapture"])
                    .env(WRITER_ENV, spec)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        // All at once.
        let ready = |w: usize| Path::new(&format!("{path}.ready{w}")).exists();
        let start = Instant::now();
        while !(0..writers).all(ready) && start.elapsed() < std::time::Duration::from_secs(60) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::fs::write(format!("{path}.go"), "").unwrap();
        for c in &mut children {
            assert!(c.wait().unwrap().success());
        }
        (0..writers).map(|w| std::fs::read_to_string(format!("{path}.{w}")).unwrap_or_else(|e| format!("writer {w}: {e}"))).collect()
    }

    /// A writer of `writers_in_processes`, as `WRITER_ENV` says: records
    /// `records` outputs, writing them in batches of its own size; writer
    /// `torn` writes each, and leaves a torn line a third of the way (a
    /// writer killed), after which the holder writes its last batch, and
    /// the others close once it closed.
    /// It then says what it found wrong in the records it knows, in
    /// `<path>.<writer>`.
    fn writer(spec: &str) {
        let parts: Vec<&str> = spec.split('|').collect();
        let [path, w, writers, records, torn] = parts[..] else { panic!("{spec}") };
        let (w, writers, records): (usize, usize, usize) = (w.parse().unwrap(), writers.parse().unwrap(), records.parse().unwrap());
        let torn: Option<usize> = torn.parse().ok();
        let marker = |name: &str| format!("{path}.{name}");
        let wait_for = |name: &str| {
            let start = Instant::now();
            while !Path::new(&marker(name)).exists() {
                assert!(start.elapsed() < std::time::Duration::from_secs(60), "{name}");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        let mut log = DepsLog::new();
        log.load(path).unwrap();
        log.open_for_write(path, &|_| true, if w == 0 { Access::Exclusive } else { Access::Shared }).unwrap();
        std::fs::write(marker(&format!("ready{w}")), "").unwrap();
        wait_for("go");
        for i in 0..records {
            if torn == Some(w) && i == records / 3 {
                std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(b"d\t1\t5\t1 2").unwrap();
                std::fs::write(marker("torn"), "").unwrap();
                return;
            }
            if torn.is_some() && w == 0 && i == records - 1 {
                wait_for("torn");
            }
            log.record_deps(&format!("w{w}/o{i}.o"), i as i64 + 1, &expected(w, i)).unwrap();
            if i % (w + 1) == 0 || torn == Some(w) {
                log.flush().unwrap();
            }
        }
        if torn.is_some() && w > 0 {
            wait_for("holder-done");
        }
        log.close();
        if w == 0 {
            std::fs::write(marker("holder-done"), "").unwrap();
        }
        let mut wrong = Vec::new();
        for other in 0..writers {
            for i in 0..records {
                if let Some(d) = log.get(&format!("w{other}/o{i}.o"))
                    && d.paths().map(str::to_string).collect::<Vec<_>>() != expected(other, i)
                {
                    wrong.push(format!("w{other}/o{i}.o"));
                }
            }
        }
        std::fs::write(marker(&w.to_string()), wrong.join(" ")).unwrap();
    }

    /// The records of `writers` writers of `records` outputs, read back:
    /// those missing or wrong.
    fn missing_or_wrong(path: &str, writers: usize, records: usize, lost: impl Fn(usize, usize) -> bool) -> Vec<String> {
        let mut log = DepsLog::new();
        assert_eq!(log.load(path).unwrap(), None);
        let mut bad = Vec::new();
        for w in 0..writers {
            for i in (0..records).filter(|&i| !lost(w, i)) {
                let out = format!("w{w}/o{i}.o");
                match log.get(&out) {
                    None => bad.push(format!("{out} missing")),
                    Some(d) if d.paths().map(str::to_string).collect::<Vec<_>>() != expected(w, i) => bad.push(format!("{out} wrong")),
                    Some(_) => {}
                }
            }
        }
        bad
    }

    /// The lock's holder and builds it runs, in processes of their own,
    /// append at once, in batches of different sizes: every record reads
    /// back, right, from the file and in each writer's view of the
    /// others'. (Before, only two logs of one process took turns.)
    #[test]
    fn writers_in_processes_take_turns() {
        if let Ok(spec) = std::env::var(WRITER_ENV) {
            return writer(&spec);
        }
        let (_dir, p) = tmp("deps-processes");
        two_outputs(&p);
        let (writers, records) = (4, 2000);
        let found = writers_in_processes("deps_log::tests::writers_in_processes_take_turns", &p, writers, records, None);
        assert!(found.iter().all(String::is_empty), "{found:?}");
        let bad = missing_or_wrong(&p, writers, records, |_, _| false);
        assert!(bad.is_empty(), "{} records: {:?}", bad.len(), &bad[..bad.len().min(5)]);
    }

    /// One of the builds the holder of the lock runs is killed while it
    /// writes (a torn line): the next writer ends the line, and all go on.
    #[test]
    fn writers_in_processes_survive_a_torn_line() {
        if let Ok(spec) = std::env::var(WRITER_ENV) {
            return writer(&spec);
        }
        let (_dir, p) = tmp("deps-processes-torn");
        two_outputs(&p);
        let (writers, records, torn) = (4, 300, 2);
        let found = writers_in_processes("deps_log::tests::writers_in_processes_survive_a_torn_line", &p, writers, records, Some(torn));
        assert!(found.iter().enumerate().all(|(w, f)| f.is_empty() || w == torn), "{found:?}");
        // But the killed writer's, made after.
        let bad = missing_or_wrong(&p, writers, records, |w, i| w == torn && i >= records / 3);
        assert!(bad.is_empty(), "{} records: {:?}", bad.len(), &bad[..bad.len().min(5)]);
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
