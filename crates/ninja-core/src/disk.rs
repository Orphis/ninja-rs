//! Filesystem access used by the build engine (ninja's `DiskInterface`).

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::graph::TimeStamp;
use crate::hash::HashMap;

pub trait Disk: Send + Sync {
    /// mtime in ns since the epoch; 0 if missing.
    fn stat(&self, path: &str) -> Result<TimeStamp, String>;
    /// Reads a file. `Ok(None)` if it does not exist.
    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String>;
    fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), String>;
    /// Creates the parent directories of `path`.
    fn make_dirs(&self, path: &str) -> Result<(), String>;
    /// Removes a file. Returns false if it did not exist.
    fn remove_file(&self, path: &str) -> Result<bool, String>;
    /// Enables or disables (and drops) a stat cache, like ninja's
    /// `AllowStatCache`: callers enable it only while scanning for dirty
    /// nodes, so that restat never sees stale timestamps.
    fn allow_stat_cache(&self, _allow: bool) {}
}

pub fn system_time_to_ns(t: SystemTime) -> TimeStamp {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            let ns = d.as_nanos() as i64;
            // 0 means "missing"; a file really dated at the epoch is 1.
            if ns == 0 { 1 } else { ns }
        }
        Err(_) => 1,
    }
}

pub fn now_ns() -> TimeStamp {
    system_time_to_ns(SystemTime::now())
}

/// Per directory (lowercased): lowercased file name -> mtime.
#[cfg(windows)]
type DirCache = HashMap<String, HashMap<String, TimeStamp>>;

/// Number and duration of stats, for `-d stats` ("node stat").
#[derive(Default)]
pub struct StatCounters {
    pub count: AtomicU64,
    pub nanos: AtomicU64,
}

#[derive(Default)]
pub struct RealDisk {
    /// On Windows, opening every file to stat it is slow; like ninja, list
    /// each directory once instead while the cache is enabled.
    #[cfg(windows)]
    stat_cache: Mutex<Option<DirCache>>,
    counters: Option<Arc<StatCounters>>,
}

impl RealDisk {
    /// A disk that records its stats in `counters`.
    pub fn with_counters(counters: Option<Arc<StatCounters>>) -> Self {
        Self { counters, ..Self::default() }
    }

    fn stat_uncounted(&self, path: &str) -> Result<TimeStamp, String> {
        #[cfg(windows)]
        if let Some(cache) = self.stat_cache.lock().unwrap().as_mut() {
            let (dir, base) = match path.rfind(['/', '\\']) {
                // Keep the separator of a root: "/x" lists "/", "C:/x" lists "C:/".
                Some(i) if i == 0 || path[..i].ends_with(':') => (&path[..=i], &path[i + 1..]),
                Some(i) => (&path[..i], &path[i + 1..]),
                None => ("", path),
            };
            // A listing has no entry for these; ask for the file itself.
            if base.is_empty() || base == "." || base == ".." {
                return stat_single(path);
            }
            let dir = dir.to_ascii_lowercase();
            if !cache.contains_key(&dir) {
                let stamps = stat_all_files_in_dir(&dir)?;
                cache.insert(dir.clone(), stamps);
            }
            return Ok(cache[&dir].get(&base.to_ascii_lowercase()).copied().unwrap_or(0));
        }
        stat_single(path)
    }
}

fn stat_single(path: &str) -> Result<TimeStamp, String> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(system_time_to_ns(m.modified().map_err(|e| format!("stat({path}): {e}"))?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        // ENOTDIR: a path component is a file.
        Err(e) if e.raw_os_error() == Some(20) && cfg!(unix) => Ok(0),
        // ERROR_PATH_NOT_FOUND / ERROR_INVALID_NAME on Windows.
        Err(e) if cfg!(windows) && matches!(e.raw_os_error(), Some(3) | Some(123) | Some(161)) => Ok(0),
        Err(e) => Err(format!("stat({path}): {e}")),
    }
}

/// Lists `dir` and records the mtime of each entry. Windows returns the
/// times with the listing, so this costs no system call per file.
#[cfg(windows)]
fn stat_all_files_in_dir(dir: &str) -> Result<HashMap<String, TimeStamp>, String> {
    let mut stamps = HashMap::default();
    let entries = match std::fs::read_dir(if dir.is_empty() { "." } else { dir }) {
        Ok(entries) => entries,
        // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_DIRECTORY: no files.
        Err(e) if e.kind() == io::ErrorKind::NotFound || matches!(e.raw_os_error(), Some(2) | Some(3) | Some(267)) => {
            return Ok(stamps);
        }
        Err(e) => return Err(format!("FindFirstFileExA({dir}): {e}")),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("FindNextFile({dir}): {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // A symlink's listing has the link's times: stat its target.
        let stamp = if entry.file_type().is_ok_and(|t| t.is_symlink()) {
            stat_single(&format!("{}\\{name}", if dir.is_empty() { "." } else { dir }))?
        } else {
            let modified = entry.metadata().and_then(|m| m.modified()).map_err(|e| format!("stat({dir}): {e}"))?;
            system_time_to_ns(modified)
        };
        stamps.insert(name.to_ascii_lowercase(), stamp);
    }
    Ok(stamps)
}

impl Disk for RealDisk {
    fn stat(&self, path: &str) -> Result<TimeStamp, String> {
        let Some(counters) = &self.counters else { return self.stat_uncounted(path) };
        let start = Instant::now();
        let result = self.stat_uncounted(path);
        counters.count.fetch_add(1, Ordering::Relaxed);
        counters.nanos.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        result
    }

    fn allow_stat_cache(&self, _allow: bool) {
        #[cfg(windows)]
        {
            *self.stat_cache.lock().unwrap() = _allow.then(DirCache::default);
        }
    }

    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{e}")),
        }
    }

    fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), String> {
        std::fs::write(path, contents).map_err(|e| format!("WriteFile({path}): {e}"))
    }

    fn make_dirs(&self, path: &str) -> Result<(), String> {
        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir({}): {e}", parent.display()))?;
        }
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<bool, String> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => {
                // Read-only files on Windows: clear the attribute and retry.
                #[cfg(windows)]
                if e.kind() == io::ErrorKind::PermissionDenied
                    && let Ok(m) = std::fs::metadata(path)
                {
                    let mut p = m.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    p.set_readonly(false);
                    let _ = std::fs::set_permissions(path, p);
                    return std::fs::remove_file(path).map(|_| true).map_err(|e| format!("remove({path}): {e}"));
                }
                Err(format!("remove({path}): {e}"))
            }
        }
    }
}

/// In-memory filesystem with a manual clock (tests).
#[derive(Default)]
pub struct VirtualDisk {
    inner: Mutex<VirtualInner>,
}

#[derive(Default)]
struct VirtualInner {
    now: TimeStamp,
    files: HashMap<String, (TimeStamp, Vec<u8>)>,
    dirs_made: Vec<String>,
    removed: Vec<String>,
    stat_errors: HashMap<String, String>,
}

impl VirtualDisk {
    pub fn new() -> Self {
        let d = Self::default();
        d.inner.lock().unwrap().now = 1;
        d
    }
    pub fn tick(&self) -> TimeStamp {
        let mut i = self.inner.lock().unwrap();
        i.now += 1;
        i.now
    }
    pub fn now(&self) -> TimeStamp {
        self.inner.lock().unwrap().now
    }
    pub fn create(&self, path: &str, contents: &str) {
        let mut i = self.inner.lock().unwrap();
        let now = i.now;
        i.files.insert(path.to_string(), (now, contents.as_bytes().to_vec()));
    }
    pub fn set_mtime(&self, path: &str, t: TimeStamp) {
        let mut i = self.inner.lock().unwrap();
        if let Some(f) = i.files.get_mut(path) {
            f.0 = t;
        }
    }
    pub fn contents(&self, path: &str) -> Option<String> {
        self.inner.lock().unwrap().files.get(path).map(|(_, c)| String::from_utf8_lossy(c).into_owned())
    }
    pub fn exists(&self, path: &str) -> bool {
        self.inner.lock().unwrap().files.contains_key(path)
    }
    pub fn removed(&self) -> Vec<String> {
        self.inner.lock().unwrap().removed.clone()
    }
    pub fn set_stat_error(&self, path: &str, err: &str) {
        self.inner.lock().unwrap().stat_errors.insert(path.into(), err.into());
    }
}

impl Disk for VirtualDisk {
    fn stat(&self, path: &str) -> Result<TimeStamp, String> {
        let i = self.inner.lock().unwrap();
        if let Some(e) = i.stat_errors.get(path) {
            return Err(e.clone());
        }
        Ok(i.files.get(path).map(|f| f.0).unwrap_or(0))
    }
    fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.inner.lock().unwrap().files.get(path).map(|f| f.1.clone()))
    }
    fn write_file(&self, path: &str, contents: &[u8]) -> Result<(), String> {
        let mut i = self.inner.lock().unwrap();
        let now = i.now;
        i.files.insert(path.to_string(), (now, contents.to_vec()));
        Ok(())
    }
    fn make_dirs(&self, path: &str) -> Result<(), String> {
        self.inner.lock().unwrap().dirs_made.push(path.to_string());
        Ok(())
    }
    fn remove_file(&self, path: &str) -> Result<bool, String> {
        let mut i = self.inner.lock().unwrap();
        i.removed.push(path.to_string());
        Ok(i.files.remove(path).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_cache_matches_uncached_stats() {
        let dir = std::env::temp_dir().join(format!("ninja-rs-test-{}-statcache", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sub").join("B.txt"), "b").unwrap();
        let root = dir.to_string_lossy().replace('\\', "/");
        let paths = [
            format!("{root}/a.txt"),
            format!("{root}/sub/B.txt"),
            format!("{root}\\sub\\B.txt"),
            format!("{root}/sub"),
            format!("{root}/missing.txt"),
            format!("{root}/nodir/missing.txt"),
            format!("{root}/sub/.."),
        ];
        let disk = RealDisk::default();
        let uncached: Vec<_> = paths.iter().map(|p| disk.stat(p).unwrap()).collect();
        disk.allow_stat_cache(true);
        let cached: Vec<_> = paths.iter().map(|p| disk.stat(p).unwrap()).collect();
        assert_eq!(cached, uncached);
        assert!(cached[0] > 0 && cached[1] > 0 && cached[3] > 0);
        assert_eq!((cached[4], cached[5]), (0, 0));
        if cfg!(windows) {
            // Case-insensitive, like the filesystem.
            assert_eq!(disk.stat(&format!("{root}/SUB/b.TXT")).unwrap(), cached[1]);
        }
        // Disabling drops the cache: new files become visible.
        std::fs::write(dir.join("new.txt"), "n").unwrap();
        if cfg!(windows) {
            assert_eq!(disk.stat(&format!("{root}/new.txt")).unwrap(), 0, "listing predates new.txt");
        }
        disk.allow_stat_cache(false);
        assert!(disk.stat(&format!("{root}/new.txt")).unwrap() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stat_follows_symlinks() {
        let dir = std::env::temp_dir().join(format!("ninja-rs-test-{}-symlink", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.txt");
        std::fs::write(&target, "t").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::File::options().write(true).open(&target).unwrap().set_modified(old).unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&target, dir.join("link.txt"));
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&target, dir.join("link.txt"));
        // Windows needs developer mode or a privilege for symlinks.
        if linked.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let link = format!("{}/link.txt", dir.to_string_lossy().replace('\\', "/"));
        let disk = RealDisk::default();
        let want = system_time_to_ns(old);
        assert_eq!(disk.stat(&link).unwrap(), want);
        disk.allow_stat_cache(true);
        assert_eq!(disk.stat(&link).unwrap(), want);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
