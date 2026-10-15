//! The build log: per output, the hash of the command that produced it and
//! its recorded mtime (ninja's `.ninja_log`).
//!
//! ninja-rs uses its own text format in `.ninja_rs_log` so that ninja and
//! ninja-rs can share a build directory without corrupting each other's state.
//!
//! ```text
//! # ninja-rs log v1
//! <start ms>\t<end ms>\t<mtime ns>\t<output>\t<command hash, hex>
//! ```
//!
//! Only complete lines count: a torn last line (a crash, a kill, a full
//! disk) is ignored, as ninja does, and cut off before the next append.
//!
//! A build writes the logs holding the build directory's lock
//! ([`LogLock`]), which ninja doesn't have: two builds appending to the deps
//! log at once would give their new paths the same ids. A build the lock
//! holder runs (a generator's `-t restat`, a build of another
//! configuration in the same directory) writes them too, appending only
//! (see [`Access`]).

use crate::hash::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::graph::TimeStamp;

pub const BUILD_LOG_FILE: &str = ".ninja_rs_log";
/// The lock file of a build directory's logs (see [`LogLock`]).
pub const LOCK_FILE: &str = ".ninja_rs_lock";

/// How often a writer checks that the log it appends to wasn't replaced (by
/// a process rewriting it without the lock, which ninja-rs doesn't do): the
/// records it appends to the old one meanwhile are lost. Not at every
/// write, which would double their cost; but after a generator ran (see
/// `BuildLog::recheck`) and at the end of a build.
pub(crate) const REPLACED_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// How a process may write a build directory's logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Access {
    /// It holds the build directory's lock (or none can be taken): it may
    /// rewrite them (recompact them, cut off what it doesn't read).
    #[default]
    Exclusive,
    /// A build it runs for holds the lock and appends to them meanwhile:
    /// it appends only. A rewrite would undo what the holder appends after
    /// it (to the file it has open), and a cut could take off a record the
    /// holder is writing.
    Shared,
}

/// An exclusive OS lock on a build directory's logs, held by the process
/// writing them, and released when dropped or when the process ends,
/// however it ends. The lock file says who holds it (see [`Holder`]).
pub struct LogLock {
    file: File,
    path: String,
    locked: bool,
}

/// Who holds a build directory's lock, as its lock file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// When it took the lock, in ms since the Unix epoch.
    pub since_ms: u64,
}

impl Holder {
    /// `pid 123, started 5 s ago`.
    pub fn describe(&self) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let secs = now.saturating_sub(self.since_ms) / 1000;
        let ago = if secs < 120 {
            format!("{secs} s")
        } else if secs < 7200 {
            format!("{} min", secs / 60)
        } else {
            format!("{} h", secs / 3600)
        };
        format!("pid {}, started {ago} ago", self.pid)
    }
}

impl LogLock {
    /// Opens the lock file `path`, created if needed, without locking it
    /// yet. On Unix read-only when this user can't write it (another
    /// user's, in a build directory a group shares): a lock needs no more,
    /// but on NFS, whose locks need a file open for writing.
    pub fn open(path: &str) -> std::io::Result<LogLock> {
        let file = match OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path) {
            #[cfg(unix)]
            Err(e) if matches!(e.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem) => File::open(path)?,
            opened => opened?,
        };
        Ok(LogLock { file, path: path.to_string(), locked: false })
    }

    /// Takes the lock, waiting for the process holding it (`wait`), or
    /// not: false while another holds it.
    pub fn lock(&mut self, wait: bool) -> std::io::Result<bool> {
        if !ninja_os::lock(&self.file, wait)? {
            return Ok(false);
        }
        self.locked = true;
        let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_millis();
        self.say(format!("{} {since}\n", std::process::id()).as_bytes());
        Ok(true)
    }

    /// The lock file's identity (see `ninja_os::identity`): what builds
    /// this one runs recognize it by.
    pub fn identity(&self) -> std::io::Result<(u64, u128)> {
        ninja_os::identity(&self.file)
    }

    /// Who holds the lock, as the lock file says (unless its holder
    /// couldn't write it: an older ninja-rs, another user's file).
    pub fn holder(&self) -> Option<Holder> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let (pid, since) = text.lines().next()?.split_once(' ')?;
        Some(Holder { pid: pid.parse().ok()?, since_ms: since.parse().ok()? })
    }

    /// Writes `text` into the lock file, if this process may (through a
    /// handle of its own: the lock's may be read-only). Its lock is on no
    /// data (see `ninja_os::lock`): others read it.
    fn say(&self, text: &[u8]) {
        if let Ok(mut f) = OpenOptions::new().write(true).open(&self.path) {
            let _ = f.write_all(text).and_then(|()| f.set_len(text.len() as u64));
        }
    }
}

impl Drop for LogLock {
    /// The holder it named is gone.
    fn drop(&mut self) {
        if self.locked {
            self.say(b"");
        }
    }
}

/// The length of the file `path`, if there is one.
pub(crate) fn file_len(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// Replaces the file `path` with `data`, through a temporary file renamed
/// into place. Its name is fixed (as ninja's): the lock holder alone
/// recompacts, and one killed while writing leaves a file the next one
/// replaces.
pub(crate) fn replace_file(path: &str, data: &[u8]) -> Result<(), String> {
    let tmp = format!("{path}.recompact");
    let written = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp).and_then(|mut f| f.write_all(data));
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {path}: {e}"));
    }
    Ok(())
}

/// Opens a log for appending (and reading, which locking it on Windows
/// needs), creating it and its directory if needed.
pub(crate) fn open_for_append(path: &str) -> std::io::Result<File> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    OpenOptions::new().read(true).append(true).create(true).open(path)
}

/// Opens the build log `path` for appending, with its signature if it is
/// new or empty. A torn last line another process left (which an
/// exclusive writer cuts off first) is ended, so that appends start on a
/// line of their own; the line is then malformed, or a record whose hash
/// is cut short, as good as none.
fn open_log(path: &str) -> std::io::Result<File> {
    let mut f = open_for_append(path)?;
    let len = f.metadata()?.len();
    if len == 0 {
        f.write_all(SIGNATURE.as_bytes())?;
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

// v2: command hashes are ninja's rapidhash (v1 used FNV-1a).
const SIGNATURE: &str = "# ninja-rs log v2\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub output: String,
    pub command_hash: u64,
    pub start_time: i64,
    pub end_time: i64,
    pub mtime: TimeStamp,
}

/// ninja's command hash (rapidhash, as in ninja >= 1.13's `.ninja_log` v7):
/// stable across hosts and versions, equal to ninja's own hashes, and fast on
/// multi-kilobyte commands.
pub fn hash_command(command: &str) -> u64 {
    rapidhash(command.as_bytes())
}

/// rapidhash, "fast" variant, ported from ninja's
/// `src/third_party/rapidhash/rapidhash.h`.
/// Copyright (C) 2024 Nicolas De Carli, based on wyhash by Wang Yi.
/// BSD 2-Clause License (https://www.opensource.org/licenses/bsd-license.php).
fn rapidhash(data: &[u8]) -> u64 {
    const SEED: u64 = 0xbdd8_9aa9_8270_4029;
    const S: [u64; 3] = [0x2d35_8dcc_aa6c_78a5, 0x8bb8_4b93_962e_acc9, 0x4b33_a62e_d433_d4a3];
    fn mum(a: u64, b: u64) -> (u64, u64) {
        let r = (a as u128) * (b as u128);
        (r as u64, (r >> 64) as u64)
    }
    fn mix(a: u64, b: u64) -> u64 {
        let (lo, hi) = mum(a, b);
        lo ^ hi
    }
    let read64 = |i: usize| u64::from_le_bytes(data[i..i + 8].try_into().expect("8 bytes"));
    let read32 = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().expect("4 bytes")) as u64;

    let len = data.len();
    let mut seed = SEED;
    seed ^= mix(seed ^ S[0], S[1]) ^ len as u64;
    let (a, b);
    if len <= 16 {
        if len >= 4 {
            let last = len - 4;
            let delta = (len & 24) >> (len >> 3);
            a = (read32(0) << 32) | read32(last);
            b = (read32(delta) << 32) | read32(last - delta);
        } else if len > 0 {
            a = ((data[0] as u64) << 56) | ((data[len >> 1] as u64) << 32) | data[len - 1] as u64;
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut p = 0;
        let mut i = len;
        if i > 48 {
            let (mut see1, mut see2) = (seed, seed);
            while i >= 48 {
                seed = mix(read64(p) ^ S[0], read64(p + 8) ^ seed);
                see1 = mix(read64(p + 16) ^ S[1], read64(p + 24) ^ see1);
                see2 = mix(read64(p + 32) ^ S[2], read64(p + 40) ^ see2);
                p += 48;
                i -= 48;
            }
            seed ^= see1 ^ see2;
        }
        if i > 16 {
            seed = mix(read64(p) ^ S[2], read64(p + 8) ^ seed ^ S[1]);
            if i > 32 {
                seed = mix(read64(p + 16) ^ S[2], read64(p + 24) ^ seed);
            }
        }
        // May reread bytes before `p`; len > 16 keeps this in bounds.
        a = read64(p + i - 16);
        b = read64(p + i - 8);
    }
    let (a, b) = mum(a ^ S[1], b ^ seed);
    mix(a ^ S[0] ^ len as u64, b ^ S[1])
}

#[derive(Default)]
pub struct BuildLog {
    entries: HashMap<String, LogEntry>,
    file: Option<File>,
    path: String,
    needs_recompaction: bool,
    /// The length of the file's complete lines, when it ends with a torn one.
    truncate_to: Option<u64>,
    /// The file's length when loaded (`None`: there was none).
    loaded_len: Option<u64>,
    /// The records `flush` writes next.
    pending: String,
    /// The outputs recorded since the log was opened: a file replacing the
    /// one their records went to gets them again.
    recorded: Vec<String>,
    /// When `ensure_open` last checked that the file wasn't replaced (None:
    /// at the next flush).
    checked: Option<Instant>,
}

impl BuildLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> &HashMap<String, LogEntry> {
        &self.entries
    }

    pub fn lookup_by_output(&self, path: &str) -> Option<&LogEntry> {
        self.entries.get(path)
    }

    /// Loads the log; a missing file is not an error. Returns a warning for
    /// an unreadable or incompatible log (which is then ignored).
    pub fn load(&mut self, path: &str) -> Result<Option<String>, String> {
        self.path = path.to_string();
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("loading build log {path}: {e}")),
        };
        self.loaded_len = Some(data.len() as u64);
        // A torn last line may still parse (a shorter hash), and the next
        // append would extend it.
        let complete = data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let text = String::from_utf8_lossy(&data[..complete]);
        if !text.starts_with(SIGNATURE) {
            self.needs_recompaction = true;
            return Ok(Some("build log version is too old or invalid; starting over".into()));
        }
        self.truncate_to = (complete < data.len()).then_some(complete as u64);
        let mut total = 0usize;
        for line in text[SIGNATURE.len()..].lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 5 {
                continue;
            }
            let (Ok(start), Ok(end), Ok(mtime), Ok(hash)) = (f[0].parse(), f[1].parse(), f[2].parse(), u64::from_str_radix(f[4], 16)) else {
                continue;
            };
            total += 1;
            self.entries.insert(f[3].to_string(), LogEntry { output: f[3].to_string(), command_hash: hash, start_time: start, end_time: end, mtime });
        }
        // Rebuild the log if there are too many dead lines.
        const MIN_COMPACTION_ENTRY_COUNT: usize = 100;
        const COMPACTION_RATIO: usize = 3;
        if total > MIN_COMPACTION_ENTRY_COUNT && total > self.entries.len() * COMPACTION_RATIO {
            self.needs_recompaction = true;
        }
        Ok(None)
    }

    /// Prepares for appending, as `access` allows: an exclusive writer
    /// recompacts first if needed (`is_live` tells whose records stay) and
    /// cuts off a torn last line; a shared one does neither.
    pub fn open_for_write(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool, access: Access) -> Result<(), String> {
        self.path = path.to_string();
        if access == Access::Shared {
            // A torn line is ended instead (see `open_log`).
            self.truncate_to = None;
            return Ok(());
        }
        // Written to since it was loaded (by a process that doesn't take the
        // build directory's lock): its end may not be the torn line read.
        // It is rewritten from what was read.
        if file_len(path) != self.loaded_len {
            self.needs_recompaction = true;
        }
        if self.needs_recompaction {
            self.recompact(path, is_live)?;
        }
        self.cut_torn_line()
    }

    /// Cuts off a torn last line found by `load`, so that appends start on
    /// a line of their own; only if the file is still the one read.
    fn cut_torn_line(&mut self) -> Result<(), String> {
        if let Some(len) = self.truncate_to.take() {
            if file_len(&self.path) != self.loaded_len {
                return Err(format!("build log {} changed since it was read: is another build writing to it?", self.path));
            }
            OpenOptions::new().write(true).open(&self.path).and_then(|f| f.set_len(len)).map_err(|e| format!("truncating build log {}: {e}", self.path))?;
            self.loaded_len = Some(len);
        }
        Ok(())
    }

    /// The file to append to: the one at the log's path, opened again once
    /// another process replaced it (recompacted it without the lock), as
    /// checked once per `REPLACED_CHECK_INTERVAL`, or at the next flush
    /// after `recheck`. The new file gets every record made since the log
    /// was opened (those written to the old one meanwhile were lost).
    fn ensure_open(&mut self) -> Result<&mut File, String> {
        if self.checked.is_none_or(|t| t.elapsed() >= REPLACED_CHECK_INTERVAL)
            && let Some(f) = &self.file
        {
            self.checked = Some(Instant::now());
            if ninja_os::replaced(f, Path::new(&self.path)) {
                self.file = None;
                self.pending = self.recorded.iter().filter_map(|o| self.entries.get(o)).map(format_entry).collect();
            }
        }
        if self.file.is_none() {
            self.cut_torn_line()?;
            self.file = Some(open_log(&self.path).map_err(|e| format!("opening build log: {e}"))?);
            self.checked = Some(Instant::now());
        }
        Ok(self.file.as_mut().expect("opened"))
    }

    /// Makes the next `flush` check first that the file wasn't replaced
    /// (a generator ran, which may have rewritten the logs; the build
    /// ends).
    pub fn recheck(&mut self) {
        self.checked = None;
    }

    /// Records `entry`, which `flush` writes.
    pub fn record(&mut self, entry: LogEntry) -> Result<(), String> {
        if !self.path.is_empty() {
            self.pending.push_str(&format_entry(&entry));
            self.recorded.push(entry.output.clone());
        }
        self.entries.insert(entry.output.clone(), entry);
        Ok(())
    }

    /// Writes the records made since the last flush, in one write: the
    /// build loop flushes once per turn, for the commands it reaped. After
    /// `recheck`, with nothing to write too: records written to a file
    /// replaced since go to the new one.
    pub fn flush(&mut self) -> Result<(), String> {
        let recheck = self.checked.is_none() && self.file.is_some();
        if self.pending.is_empty() && !recheck {
            return Ok(());
        }
        self.ensure_open()?;
        let lines = std::mem::take(&mut self.pending);
        if lines.is_empty() {
            return Ok(());
        }
        let f = self.file.as_mut().expect("opened");
        f.write_all(lines.as_bytes()).map_err(|e| format!("Error writing to build log: {e}"))
    }

    /// Writes the records left (see `flush`, whose errors are lost: the
    /// build flushes before) and closes the file.
    pub fn close(&mut self) {
        self.recheck();
        let _ = self.flush();
        self.file = None;
    }

    /// Rewrites the log keeping only live entries.
    pub fn recompact(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool) -> Result<(), String> {
        self.close();
        // In the new file, or dead.
        self.recorded.clear();
        self.entries.retain(|k, _| is_live(k));
        let mut out = String::from(SIGNATURE);
        let mut keys: Vec<&String> = self.entries.keys().collect();
        keys.sort();
        for k in keys {
            out.push_str(&format_entry(&self.entries[k]));
        }
        replace_file(path, out.as_bytes())?;
        self.needs_recompaction = false;
        self.truncate_to = None;
        self.loaded_len = Some(out.len() as u64);
        Ok(())
    }

    /// Updates recorded mtimes to the current mtimes of the outputs
    /// (`-t restat`). With `outputs` empty, all entries are restat'ed. An
    /// exclusive writer rewrites the log; a shared one appends the records
    /// that changed, which win over the earlier ones.
    pub fn restat(&mut self, path: &str, disk: &dyn crate::disk::Disk, outputs: &[String], access: Access) -> Result<(), String> {
        self.close();
        let wanted: std::collections::HashSet<&str> = outputs.iter().map(String::as_str).collect();
        let mut changed = Vec::new();
        for (k, e) in self.entries.iter_mut() {
            if wanted.is_empty() || wanted.contains(k.as_str()) {
                let mtime = disk.stat(k)?;
                if mtime != e.mtime {
                    e.mtime = mtime;
                    changed.push(k.clone());
                }
            }
        }
        if access == Access::Exclusive {
            return self.recompact(path, &|_| true);
        }
        self.open_for_write(path, &|_| true, access)?;
        changed.sort();
        self.pending = changed.iter().map(|k| format_entry(&self.entries[k])).collect();
        self.flush()?;
        self.close();
        Ok(())
    }
}

impl Drop for BuildLog {
    /// Writes the records left: a build flushes its own, but for a panic.
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            self.close();
        }
    }
}

fn format_entry(e: &LogEntry) -> String {
    format!("{}\t{}\t{}\t{}\t{:x}\n", e.start_time, e.end_time, e.mtime, e.output, e.command_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_command_matches_ninja() {
        // Hashes from a .ninja_log written by ninja 1.13.2 for these commands.
        assert_eq!(hash_command("cmd.exe /c type nul > obj/t0/s0.cpp.obj && echo obj/t0/s0.cpp.obj: src/common.h> obj/t0/s0.cpp.obj.d"), 0x9c46_9d71_333c_72b6);
        assert_eq!(hash_command("cmd.exe /c type nul > bin/t0.exe"), 0x68c9_7677_6b0c_9780);
    }

    /// A log's path, in a directory of its own (removed when dropped).
    fn tmp(name: &str) -> (test_dirs::TestDir, String) {
        let dir = test_dirs::TestDir::new(&format!("ninja_core_{name}"));
        let p = dir.join("log").to_string_lossy().into_owned();
        (dir, p)
    }

    #[test]
    fn write_read() {
        let (_dir, p) = tmp("buildlog1");
        let mut log = BuildLog::new();
        log.load(&p).unwrap();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record(LogEntry { output: "out".into(), command_hash: hash_command("cat in"), start_time: 15, end_time: 18, mtime: 0 }).unwrap();
        log.record(LogEntry { output: "mid".into(), command_hash: hash_command("cat mid"), start_time: 20, end_time: 21, mtime: 5 }).unwrap();
        log.close();
        let mut log2 = BuildLog::new();
        assert_eq!(log2.load(&p).unwrap(), None);
        assert_eq!(log2.entries().len(), 2);
        assert_eq!(log2.lookup_by_output("out"), log.lookup_by_output("out"));
        assert_eq!(log2.lookup_by_output("mid").unwrap().mtime, 5);
    }

    #[test]
    fn last_entry_wins_and_recompact() {
        let (_dir, p) = tmp("buildlog2");
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        for i in 0..200 {
            log.record(LogEntry { output: "out".into(), command_hash: i, start_time: i as i64, end_time: 0, mtime: 0 }).unwrap();
        }
        log.record(LogEntry { output: "dead".into(), command_hash: 1, start_time: 0, end_time: 0, mtime: 0 }).unwrap();
        log.close();
        let mut log2 = BuildLog::new();
        log2.load(&p).unwrap();
        assert_eq!(log2.lookup_by_output("out").unwrap().command_hash, 199);
        assert!(log2.needs_recompaction);
        log2.open_for_write(&p, &|o| o != "dead", Access::Exclusive).unwrap();
        assert!(log2.lookup_by_output("dead").is_none());
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn invalid_header_ignored() {
        let (_dir, p) = tmp("buildlog3");
        std::fs::write(&p, "# ninja log v5\n1\t2\t3\tout\tdeadbeef\n").unwrap();
        let mut log = BuildLog::new();
        assert!(log.load(&p).unwrap().is_some());
        assert!(log.entries().is_empty());
    }

    #[test]
    fn torn_entry_is_ignored_and_cut() {
        let (_dir, p) = tmp("buildlog4");
        let entry = |output: &str| LogEntry { output: output.into(), command_hash: hash_command(output), start_time: 1, end_time: 2, mtime: 3 };
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record(entry("a")).unwrap();
        log.record(entry("b")).unwrap();
        log.close();
        // Cut inside the last entry's hash: it would parse, with another hash,
        // and the next append would extend it.
        let mut data = std::fs::read(&p).unwrap();
        data.truncate(data.len() - 4);
        std::fs::write(&p, &data).unwrap();
        let mut log = BuildLog::new();
        log.load(&p).unwrap();
        assert!(log.lookup_by_output("b").is_none());
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record(entry("c")).unwrap();
        log.close();
        let mut log = BuildLog::new();
        log.load(&p).unwrap();
        assert_eq!(log.lookup_by_output("a"), Some(&entry("a")));
        assert!(log.lookup_by_output("b").is_none());
        assert_eq!(log.lookup_by_output("c"), Some(&entry("c")));
    }

    fn entry(output: &str, mtime: TimeStamp) -> LogEntry {
        LogEntry { output: output.into(), command_hash: hash_command(output), start_time: 1, end_time: 2, mtime }
    }

    /// A shared writer (a build the lock's holder runs, which appends
    /// meanwhile) neither recompacts the log nor cuts a torn line: it ends
    /// the line, and appends.
    #[test]
    fn a_shared_writer_neither_recompacts_nor_cuts() {
        let (_dir, p) = tmp("buildlog-shared");
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        for i in 0..400 {
            log.record(entry("dead", i)).unwrap();
        }
        log.record(entry("a", 1)).unwrap();
        log.close();
        let mut data = std::fs::read(&p).unwrap();
        data.truncate(data.len() - 4);
        std::fs::write(&p, &data).unwrap();
        let mut shared = BuildLog::new();
        shared.load(&p).unwrap();
        assert!(shared.needs_recompaction && shared.truncate_to.is_some());
        shared.open_for_write(&p, &|o| o != "dead", Access::Shared).unwrap();
        shared.record(entry("b", 2)).unwrap();
        shared.close();
        let after = std::fs::read(&p).unwrap();
        assert!(after.starts_with(&data), "rewritten or cut");
        let mut log = BuildLog::new();
        log.load(&p).unwrap();
        assert_eq!(log.lookup_by_output("dead"), Some(&entry("dead", 399)));
        assert_eq!(log.lookup_by_output("b"), Some(&entry("b", 2)));
    }

    /// A log another process replaced while a build appends to it: the
    /// build's records go to the new one.
    #[test]
    fn a_replaced_log_is_opened_again() {
        let (dir, p) = tmp("buildlog-replaced");
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record(entry("a", 1)).unwrap();
        log.flush().unwrap();
        let other = dir.join("other");
        let replace = |e: LogEntry| {
            std::fs::write(&other, format!("{SIGNATURE}{}", format_entry(&e))).unwrap();
            std::fs::rename(&other, &p).unwrap();
        };
        // A generator ran.
        replace(entry("x", 5));
        log.recheck();
        log.record(entry("b", 2)).unwrap();
        log.flush().unwrap();
        let read = || {
            let mut log = BuildLog::new();
            log.load(&p).unwrap();
            log
        };
        // The new file has every record of this log's.
        let new = read();
        assert_eq!(
            (new.lookup_by_output("x"), new.lookup_by_output("a"), new.lookup_by_output("b")),
            (Some(&entry("x", 5)), Some(&entry("a", 1)), Some(&entry("b", 2)))
        );
        // Before the end: closing checks, with nothing left to write too.
        replace(entry("y", 6));
        log.close();
        let last = read();
        assert_eq!(
            (last.lookup_by_output("y"), last.lookup_by_output("a"), last.lookup_by_output("b")),
            (Some(&entry("y", 6)), Some(&entry("a", 1)), Some(&entry("b", 2)))
        );
    }

    /// Records wait for `flush`, which writes them all at once.
    #[test]
    fn records_are_written_at_each_flush() {
        let (_dir, p) = tmp("buildlog-flush");
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record(entry("a", 1)).unwrap();
        log.flush().unwrap();
        let before = std::fs::read(&p).unwrap();
        log.record(entry("b", 2)).unwrap();
        log.record(entry("c", 3)).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), before);
        log.flush().unwrap();
        let after = std::fs::read(&p).unwrap();
        assert_eq!(String::from_utf8_lossy(&after[before.len()..]), format_entry(&entry("b", 2)) + &format_entry(&entry("c", 3)));
    }

    /// `-t restat`: an exclusive writer rewrites the log, a shared one
    /// appends the records that changed.
    #[test]
    fn restat_appends_when_shared() {
        use crate::disk::Disk;
        let (dir, p) = tmp("buildlog-restat");
        let disk = crate::disk::RealDisk::default();
        let out = dir.join("out").to_string_lossy().into_owned();
        std::fs::write(&out, "").unwrap();
        let mtime = disk.stat(&out).unwrap();
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record(entry(&out, 1)).unwrap();
        log.record(entry("other", 1)).unwrap();
        log.close();
        let before = std::fs::read(&p).unwrap();
        let mut shared = BuildLog::new();
        shared.load(&p).unwrap();
        shared.restat(&p, &disk, std::slice::from_ref(&out), Access::Shared).unwrap();
        let after = std::fs::read(&p).unwrap();
        assert!(after.starts_with(&before));
        assert_eq!(String::from_utf8_lossy(&after[before.len()..]), format_entry(&entry(&out, mtime)));
        let mut exclusive = BuildLog::new();
        exclusive.load(&p).unwrap();
        exclusive.restat(&p, &disk, &[], Access::Exclusive).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 3);
        let mut log = BuildLog::new();
        log.load(&p).unwrap();
        assert_eq!(log.lookup_by_output(&out).unwrap().mtime, mtime);
    }

    /// A recompaction killed while writing leaves its temporary file, which
    /// the next one replaces: none stays.
    #[test]
    fn recompaction_reuses_its_temporary_file() {
        let (dir, p) = tmp("buildlog-tmp");
        std::fs::write(format!("{p}.recompact"), "left by a recompaction killed while writing").unwrap();
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true, Access::Exclusive).unwrap();
        log.record(entry("a", 1)).unwrap();
        log.recompact(&p, &|_| true).unwrap();
        let files: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(files, ["log"]);
    }

    /// One process at a time holds the lock; the lock file says which, and
    /// stops saying it when it lets go.
    #[test]
    fn the_lock_names_its_holder() {
        let (_dir, p) = tmp("lock");
        let mut held = LogLock::open(&p).unwrap();
        assert!(held.lock(false).unwrap());
        let mut other = LogLock::open(&p).unwrap();
        assert!(!other.lock(false).unwrap());
        let holder = other.holder().unwrap();
        assert_eq!(holder.pid, std::process::id());
        assert!(holder.describe().starts_with(&format!("pid {}, started ", std::process::id())), "{}", holder.describe());
        assert_eq!(held.identity().unwrap(), other.identity().unwrap());
        drop(held);
        assert_eq!(other.holder(), None);
        assert!(other.lock(false).unwrap());
    }

    /// A lock file this user can't write (another user's, in a build
    /// directory a group shares) locks all the same.
    #[cfg(unix)]
    #[test]
    fn a_read_only_lock_file_locks() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, p) = tmp("lock-read-only");
        std::fs::write(&p, "").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();
        let mut lock = LogLock::open(&p).unwrap();
        assert!(lock.lock(false).unwrap());
        assert!(!LogLock::open(&p).unwrap().lock(false).unwrap());
    }
}
