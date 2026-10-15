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
//! log at once would give their new paths the same ids.

use crate::hash::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;

use crate::graph::TimeStamp;

pub const BUILD_LOG_FILE: &str = ".ninja_rs_log";
/// The lock file of a build directory's logs (see [`LogLock`]).
pub const LOCK_FILE: &str = ".ninja_rs_lock";

/// An exclusive OS lock on a build directory's logs, held by the process
/// writing them, and released when dropped or when the process ends,
/// however it ends.
pub struct LogLock {
    _file: File,
}

impl LogLock {
    /// Takes the lock file `path`, unless another process holds it (`None`).
    pub fn try_acquire(path: &str) -> std::io::Result<Option<LogLock>> {
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(LogLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }

    /// Takes the lock file `path`, waiting for the process holding it.
    pub fn acquire(path: &str) -> std::io::Result<LogLock> {
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        file.lock()?;
        Ok(LogLock { _file: file })
    }
}

/// The length of the file `path`, if there is one.
pub(crate) fn file_len(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// Replaces the file `path` with `data`, through a temporary file renamed
/// into place, named for this process and moment: another one recompacting
/// at once, or one that died doing it, has its own.
pub(crate) fn replace_file(path: &str, data: &[u8]) -> Result<(), String> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = format!("{path}.{}-{nanos}-{n}.tmp", std::process::id());
    let written = OpenOptions::new().write(true).create_new(true).open(&tmp).and_then(|mut f| f.write_all(data));
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {path}: {e}"));
    }
    Ok(())
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

    /// Prepares for appending; recompacts first if needed. `is_live` tells
    /// whether an output still exists in the manifest.
    pub fn open_for_write(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool) -> Result<(), String> {
        self.path = path.to_string();
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

    fn ensure_open(&mut self) -> Result<&mut File, String> {
        if self.file.is_none() {
            self.cut_torn_line()?;
            let path = self.path.clone();
            if let Some(parent) = std::path::Path::new(&path).parent()
                && !parent.as_os_str().is_empty()
            {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut f = OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("opening build log: {e}"))?;
            if f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                f.write_all(SIGNATURE.as_bytes()).map_err(|e| e.to_string())?;
            }
            self.file = Some(f);
        }
        Ok(self.file.as_mut().expect("opened"))
    }

    pub fn record(&mut self, entry: LogEntry) -> Result<(), String> {
        if !self.path.is_empty() {
            let line = format_entry(&entry);
            let f = self.ensure_open()?;
            f.write_all(line.as_bytes()).map_err(|e| format!("Error writing to build log: {e}"))?;
        }
        self.entries.insert(entry.output.clone(), entry);
        Ok(())
    }

    pub fn close(&mut self) {
        self.file = None;
    }

    /// Rewrites the log keeping only live entries.
    pub fn recompact(&mut self, path: &str, is_live: &dyn Fn(&str) -> bool) -> Result<(), String> {
        self.close();
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
    /// (`-t restat`). With `outputs` empty, all entries are restat'ed.
    pub fn restat(&mut self, path: &str, disk: &dyn crate::disk::Disk, outputs: &[String]) -> Result<(), String> {
        self.close();
        for (k, e) in self.entries.iter_mut() {
            if outputs.is_empty() || outputs.iter().any(|o| o == k) {
                e.mtime = disk.stat(k)?;
            }
        }
        self.recompact(path, &|_| true)
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

    fn tmp(name: &str) -> String {
        let d = test_dirs::base().join(format!("ninja-rs-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&d);
        d.to_string_lossy().into_owned()
    }

    #[test]
    fn write_read() {
        let p = tmp("buildlog1");
        let mut log = BuildLog::new();
        log.load(&p).unwrap();
        log.open_for_write(&p, &|_| true).unwrap();
        log.record(LogEntry { output: "out".into(), command_hash: hash_command("cat in"), start_time: 15, end_time: 18, mtime: 0 }).unwrap();
        log.record(LogEntry { output: "mid".into(), command_hash: hash_command("cat mid"), start_time: 20, end_time: 21, mtime: 5 }).unwrap();
        log.close();
        let mut log2 = BuildLog::new();
        assert_eq!(log2.load(&p).unwrap(), None);
        assert_eq!(log2.entries().len(), 2);
        assert_eq!(log2.lookup_by_output("out"), log.lookup_by_output("out"));
        assert_eq!(log2.lookup_by_output("mid").unwrap().mtime, 5);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn last_entry_wins_and_recompact() {
        let p = tmp("buildlog2");
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
        for i in 0..200 {
            log.record(LogEntry { output: "out".into(), command_hash: i, start_time: i as i64, end_time: 0, mtime: 0 }).unwrap();
        }
        log.record(LogEntry { output: "dead".into(), command_hash: 1, start_time: 0, end_time: 0, mtime: 0 }).unwrap();
        log.close();
        let mut log2 = BuildLog::new();
        log2.load(&p).unwrap();
        assert_eq!(log2.lookup_by_output("out").unwrap().command_hash, 199);
        assert!(log2.needs_recompaction);
        log2.open_for_write(&p, &|o| o != "dead").unwrap();
        assert!(log2.lookup_by_output("dead").is_none());
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 2);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn invalid_header_ignored() {
        let p = tmp("buildlog3");
        std::fs::write(&p, "# ninja log v5\n1\t2\t3\tout\tdeadbeef\n").unwrap();
        let mut log = BuildLog::new();
        assert!(log.load(&p).unwrap().is_some());
        assert!(log.entries().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn torn_entry_is_ignored_and_cut() {
        let p = tmp("buildlog4");
        let entry = |output: &str| LogEntry { output: output.into(), command_hash: hash_command(output), start_time: 1, end_time: 2, mtime: 3 };
        let mut log = BuildLog::new();
        log.open_for_write(&p, &|_| true).unwrap();
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
        log.open_for_write(&p, &|_| true).unwrap();
        log.record(entry("c")).unwrap();
        log.close();
        let mut log = BuildLog::new();
        log.load(&p).unwrap();
        assert_eq!(log.lookup_by_output("a"), Some(&entry("a")));
        assert!(log.lookup_by_output("b").is_none());
        assert_eq!(log.lookup_by_output("c"), Some(&entry("c")));
        let _ = std::fs::remove_file(&p);
    }
}
