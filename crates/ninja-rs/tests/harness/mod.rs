//! What the end-to-end tests share. Each test gets a directory of its own
//! (under Bazel's `TEST_TMPDIR` when set), removed when it passes and kept
//! when it fails; a panic fails its test only; arguments (or Bazel's
//! `--test_filter`) select tests by name, and Bazel's sharding splits them.
//! Tests decide from markers, counters and the order of events, with
//! timeouts far above what a healthy run takes, rather than from how long
//! things take: the machine may be busy.

#![allow(dead_code)]

use std::any::Any;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub type Test = (&'static str, fn() -> Result<(), String>);

/// What the running test made and said.
struct Current {
    dir: PathBuf,
    /// Its toolchain's binary, linked into its fixtures (see `link_self`).
    tools: PathBuf,
    not_tested: Option<String>,
}

static CURRENT: Mutex<Option<Current>> = Mutex::new(None);

/// How long tests wait for what a healthy run does at once, or in a few
/// seconds: only a broken one gets there.
pub const PATIENCE: Duration = Duration::from_secs(60);

/// Runs the tests whose name contains one of the arguments (or of
/// `TESTBRIDGE_TEST_ONLY`'s), all of them without any, that are this
/// shard's (`TEST_SHARD_INDEX`), and sums up; fails when one failed.
pub fn run(suite: &str, tests: Vec<Test>) -> ExitCode {
    ninja_exe();
    let mut filter: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with('-')).collect();
    if let Ok(only) = std::env::var("TESTBRIDGE_TEST_ONLY") {
        filter.extend(only.split(',').filter(|f| !f.is_empty()).map(str::to_string));
    }
    let shards = std::env::var("TEST_TOTAL_SHARDS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 1);
    let shard = std::env::var("TEST_SHARD_INDEX").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if let Some(status) = std::env::var_os("TEST_SHARD_STATUS_FILE") {
        let _ = std::fs::write(status, "");
    }
    let tests: Vec<Test> = tests
        .into_iter()
        .filter(|(name, _)| filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str())))
        .enumerate()
        .filter(|(i, _)| shards.is_none_or(|n| i % n == shard))
        .map(|(_, t)| t)
        .collect();
    let dir = test_dirs::TestDir::new(&format!("ninja-rs-{suite}"));
    let tools = dir.join("tools");
    let (mut failed, mut not_tested) = (Vec::new(), Vec::new());
    for (name, test) in &tests {
        let test_dir = dir.join(name);
        std::fs::create_dir_all(&test_dir).unwrap();
        *CURRENT.lock().unwrap() = Some(Current { dir: test_dir.clone(), tools: tools.clone(), not_tested: None });
        let result = std::panic::catch_unwind(test).unwrap_or_else(|panic| Err(format!("panicked: {}", panic_message(&*panic))));
        let current = CURRENT.lock().unwrap().take().expect("the current test");
        match (result, current.not_tested) {
            (Err(e), _) => {
                println!("test {name} ... FAILED: {e}\n  (its files: {})", test_dir.display());
                failed.push(*name);
            }
            (Ok(()), Some(why)) => {
                println!("test {name} ... NOT TESTED: {why}");
                not_tested.push(format!("{name} ({why})"));
                remove(&test_dir);
            }
            (Ok(()), None) => {
                println!("test {name} ... ok");
                remove(&test_dir);
            }
        }
    }
    println!("test result: {} passed; {} failed; {} not tested", tests.len() - failed.len() - not_tested.len(), failed.len(), not_tested.len());
    if !failed.is_empty() {
        println!("failed: {}", failed.join(", "));
    }
    if !not_tested.is_empty() {
        println!("not tested: {}", not_tested.join(", "));
    }
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        println!("kept: {}", dir.keep().display());
        ExitCode::FAILURE
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
        (Some(s), _) => s.to_string(),
        (_, Some(s)) => s.clone(),
        _ => "(no message)".into(),
    }
}

fn remove(dir: &Path) {
    if let Err(e) = test_dirs::remove_all(dir) {
        println!("warning: removing {}: {e}", dir.display());
    }
}

/// A new, empty directory for the running test.
pub fn test_dir(name: &str) -> PathBuf {
    let dir = CURRENT.lock().unwrap().as_ref().expect("a test running").dir.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Says the running test couldn't test what it's for: it passes, but the
/// summary lists it.
pub fn not_tested(why: &str) {
    if let Some(c) = CURRENT.lock().unwrap().as_mut() {
        c.not_tested = Some(why.to_string());
    }
}

/// Makes `dest` this test binary (which acts as tools under their names):
/// a hard link, not a copy of hundreds of MB. Where the binary can't be
/// linked to (another file system), a copy of it next to the tests' files
/// is, made once.
pub fn link_self(dest: &Path) {
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(dest);
    let exe = std::env::current_exe().unwrap();
    if std::fs::hard_link(&exe, dest).is_ok() {
        return;
    }
    let tools = CURRENT.lock().unwrap().as_ref().expect("a test running").tools.clone();
    let copy = tools.join(exe.file_name().unwrap());
    if !copy.exists() {
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::copy(&exe, &copy).unwrap();
    }
    if std::fs::hard_link(&copy, dest).is_err() {
        std::fs::copy(&copy, dest).unwrap();
    }
}

/// The ninja-rs binary (`CARGO_BIN_EXE_ninja-rs`, a runfile under Bazel,
/// relative to the directory the test starts in).
pub fn ninja_exe() -> PathBuf {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let path = env!("CARGO_BIN_EXE_ninja-rs");
        if Path::new(path).exists() {
            return std::path::absolute(path).unwrap();
        }
        // Bazel on Windows: no runfiles tree, a manifest of them.
        let manifest = std::env::var("RUNFILES_MANIFEST_FILE").expect("ninja-rs binary not found");
        let key = format!("_main/{path} ");
        let text = std::fs::read_to_string(manifest).unwrap();
        let line = text.lines().find(|l| l.starts_with(&key)).expect("ninja-rs binary not in the runfiles");
        PathBuf::from(&line[key.len()..])
    })
    .clone()
}

/// Waits for `done`, polling, up to `PATIENCE`.
pub fn wait_for(what: &str, mut done: impl FnMut() -> bool) -> Result<(), String> {
    let start = Instant::now();
    while !done() {
        if start.elapsed() > PATIENCE {
            return Err(format!("{what}: still not the case after {PATIENCE:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Whether process `pid` still runs (a zombie doesn't).
pub fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const WAIT_TIMEOUT: u32 = 0x102;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
            fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
            fn CloseHandle(handle: *mut c_void) -> i32;
        }
        // SAFETY: plain system calls; the handle is closed below.
        unsafe {
            let h = OpenProcess(SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return false;
            }
            let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
            CloseHandle(h);
            running
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return false;
        }
        // "pid (name) S ...": the state follows the name.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        stat.rsplit_once(')').is_none_or(|(_, rest)| !rest.trim_start().starts_with('Z'))
    }
}
