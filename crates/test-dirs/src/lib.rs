//! Directories for tests: under Bazel's `TEST_TMPDIR` when it is set (the
//! system's temporary directory otherwise), one per test and process, and
//! removed when the test is done with them. A test that panicked leaves its
//! directory, to look into.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Where tests' directories go.
#[allow(clippy::disallowed_methods)] // The one place tests ask for it.
pub fn base() -> PathBuf {
    std::env::var_os("TEST_TMPDIR").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// A test's directory, removed when dropped (unless the thread panics).
#[derive(Debug)]
pub struct TestDir {
    path: Option<PathBuf>,
}

impl TestDir {
    /// A new, empty directory named after `name`, the process and a counter
    /// (tests run in parallel threads, and processes).
    pub fn new(name: &str) -> TestDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = base().join(format!("{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        // Left by a process that had the same id.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap_or_else(|e| panic!("creating {}: {e}", path.display()));
        TestDir { path: Some(path) }
    }

    pub fn path(&self) -> &Path {
        self.path.as_deref().expect("kept")
    }

    /// Leaves the directory in place.
    pub fn keep(mut self) -> PathBuf {
        self.path.take().expect("kept")
    }
}

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.path()
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else { return };
        if std::thread::panicking() {
            eprintln!("test directory kept: {}", path.display());
            return;
        }
        if let Err(e) = remove_all(&path) {
            eprintln!("warning: removing {}: {e}", path.display());
        }
    }
}

/// Removes a directory and what it holds. On Windows, files of processes
/// that just exited (or that a scanner opened) can't be removed for a
/// moment: it tries again for a few seconds.
pub fn remove_all(path: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) if cfg!(windows) && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_when_dropped() {
        let dir = TestDir::new("test-dirs");
        std::fs::write(dir.join("file"), "x").unwrap();
        let path = dir.to_path_buf();
        assert!(path.starts_with(base()));
        assert_ne!(TestDir::new("test-dirs").path(), path);
        drop(dir);
        assert!(!path.exists());
        let kept = TestDir::new("test-dirs").keep();
        assert!(kept.is_dir());
        remove_all(&kept).unwrap();
    }
}
