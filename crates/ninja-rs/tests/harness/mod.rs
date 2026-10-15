//! What the end-to-end tests share. Each test gets a directory of its own
//! (under Bazel's `TEST_TMPDIR` when set), removed when it passes and kept
//! when it fails; a panic fails its test only, and so does a test running
//! longer than `TEST_TIMEOUT`. Arguments select tests as libtest's do (and
//! Bazel's `--test_filter`), Bazel's sharding splits them, and the results
//! go to Bazel's `XML_OUTPUT_FILE`, a test case each. The processes a
//! test starts through `spawn` (or `output`, `status`) don't outlive it
//! when it runs out of time.
//! Tests decide from markers, counters and the order of events, with
//! timeouts far above what a healthy run takes, rather than from how long
//! things take: the machine may be busy.

#![allow(dead_code)]

use std::any::Any;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Output, Stdio};
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

/// How long a test may run: a few times what the slowest takes on a busy
/// machine. One that hangs fails then, naming itself, rather than holding
/// up the others until Bazel's timeout (or forever).
pub const TEST_TIMEOUT: Duration = Duration::from_secs(300);

/// What a test came to.
enum Outcome {
    Passed,
    Failed(String),
    NotTested(String),
}

/// The tests done, and the suite's name, for the summary and
/// `XML_OUTPUT_FILE` (which the watchdog writes too).
struct Results {
    suite: String,
    done: Vec<(&'static str, Duration, Outcome)>,
}

static RESULTS: Mutex<Results> = Mutex::new(Results { suite: String::new(), done: Vec::new() });

/// The running test, and when the watchdog fails it.
static RUNNING: Mutex<Option<(&'static str, Instant, Instant)>> = Mutex::new(None);

/// A process the running test started (`spawn`).
struct Started {
    pid: u32,
    /// A handle of it, which keeps its id from naming another process.
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

/// What the running test started and hasn't waited for through this
/// module: the watchdog kills it, rather than leave it running once the
/// tests' binary exited.
static STARTED: Mutex<Vec<Started>> = Mutex::new(Vec::new());

/// What the arguments select, as libtest's: tests whose name contains one
/// of the filters (or is one, with `--exact`) and none of `--skip`'s.
#[derive(Default)]
struct Selection {
    filters: Vec<String>,
    skip: Vec<String>,
    exact: bool,
    list: bool,
    /// Only the ignored tests (`--ignored`): there are none.
    ignored: bool,
    /// `--list` without the count after the names (`--format terse`, `-q`).
    terse: bool,
}

impl Selection {
    fn parse(args: impl IntoIterator<Item = String>) -> Selection {
        let mut s = Selection::default();
        let mut args = args.into_iter();
        while let Some(a) = args.next() {
            let (flag, joined) = match a.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (a.clone(), None),
            };
            let mut value = || joined.clone().or_else(|| args.next()).unwrap_or_default();
            match flag.as_str() {
                "--list" => s.list = true,
                "--exact" => s.exact = true,
                "--ignored" => s.ignored = true,
                "-q" | "--quiet" => s.terse = true,
                "--skip" => s.skip.push(value()),
                "--format" => s.terse |= value() == "terse",
                // libtest's other options with a value: none selects tests.
                "--test-threads" | "--color" | "--logfile" | "--shuffle-seed" | "-Z" => {
                    value();
                }
                _ if a.starts_with('-') => {}
                _ => s.filters.push(a),
            }
        }
        // Bazel's `--test_filter`.
        if let Ok(only) = std::env::var("TESTBRIDGE_TEST_ONLY") {
            s.filters.extend(only.split(',').filter(|f| !f.is_empty()).map(str::to_string));
        }
        s
    }

    fn selects(&self, name: &str) -> bool {
        let matches = |f: &String| if self.exact { name == f } else { name.contains(f.as_str()) };
        !self.ignored && (self.filters.is_empty() || self.filters.iter().any(matches)) && !self.skip.iter().any(matches)
    }
}

/// Runs the tests the arguments select that are this shard's
/// (`TEST_SHARD_INDEX`), or lists them (`--list`), and sums up; fails when
/// one failed.
pub fn run(suite: &str, tests: Vec<Test>) -> ExitCode {
    let selection = Selection::parse(std::env::args().skip(1));
    let shards = std::env::var("TEST_TOTAL_SHARDS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 1);
    let shard = std::env::var("TEST_SHARD_INDEX").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if let Some(status) = std::env::var_os("TEST_SHARD_STATUS_FILE") {
        let _ = std::fs::write(status, "");
    }
    let tests: Vec<Test> = tests
        .into_iter()
        .filter(|(name, _)| selection.selects(name))
        .enumerate()
        .filter(|(i, _)| shards.is_none_or(|n| i % n == shard))
        .map(|(_, t)| t)
        .collect();
    if selection.list {
        for (name, _) in &tests {
            println!("{name}: test");
        }
        if !selection.terse {
            println!("\n{} tests, 0 benchmarks", tests.len());
        }
        return ExitCode::SUCCESS;
    }
    ninja_exe();
    RESULTS.lock().unwrap().suite = suite.to_string();
    let dir = test_dirs::TestDir::new(&format!("ninja-rs-{suite}"));
    let tools = dir.join("tools");
    watchdog(dir.to_path_buf());
    for &(name, test) in &tests {
        let test_dir = dir.join(name);
        std::fs::create_dir_all(&test_dir).unwrap();
        *CURRENT.lock().unwrap() = Some(Current { dir: test_dir.clone(), tools: tools.clone(), not_tested: None });
        let started = Instant::now();
        *RUNNING.lock().unwrap() = Some((name, started, started + TEST_TIMEOUT.min(time_left())));
        let result = std::panic::catch_unwind(test).unwrap_or_else(|panic| Err(format!("panicked: {}", panic_message(&*panic))));
        // Blocks while the watchdog fails the test (and exits).
        RUNNING.lock().unwrap().take();
        STARTED.lock().unwrap().clear();
        let current = CURRENT.lock().unwrap().take().expect("the current test");
        let outcome = match (result, current.not_tested) {
            (Err(e), _) => {
                println!("test {name} ... FAILED: {e}\n  (its files: {})", test_dir.display());
                Outcome::Failed(e)
            }
            (Ok(()), Some(why)) => {
                println!("test {name} ... NOT TESTED: {why}");
                remove(&test_dir);
                Outcome::NotTested(why)
            }
            (Ok(()), None) => {
                println!("test {name} ... ok");
                remove(&test_dir);
                Outcome::Passed
            }
        };
        RESULTS.lock().unwrap().done.push((name, started.elapsed(), outcome));
    }
    if sum_up() {
        ExitCode::SUCCESS
    } else {
        println!("kept: {}", dir.keep().display());
        ExitCode::FAILURE
    }
}

/// Prints the summary and writes `XML_OUTPUT_FILE`; whether every test
/// passed or wasn't tested.
fn sum_up() -> bool {
    let results = RESULTS.lock().unwrap();
    let (mut failed, mut not_tested) = (Vec::new(), Vec::new());
    for (name, _, outcome) in &results.done {
        match outcome {
            Outcome::Failed(_) => failed.push(name.to_string()),
            Outcome::NotTested(why) => not_tested.push(format!("{name} ({why})")),
            Outcome::Passed => {}
        }
    }
    let passed = results.done.len() - failed.len() - not_tested.len();
    println!("test result: {passed} passed; {} failed; {} not tested", failed.len(), not_tested.len());
    if !failed.is_empty() {
        println!("failed: {}", failed.join(", "));
    }
    if !not_tested.is_empty() {
        println!("not tested: {}", not_tested.join(", "));
    }
    if let Some(path) = std::env::var_os("XML_OUTPUT_FILE").filter(|p| !p.is_empty())
        && let Err(e) = std::fs::write(&path, junit_xml(&results))
    {
        println!("warning: writing {}: {e}", Path::new(&path).display());
    }
    failed.is_empty()
}

/// The results as JUnit XML, the format of Bazel's `test.xml`: a test case
/// each (by default Bazel makes one for the whole binary, which hides the
/// tests not tested).
fn junit_xml(results: &Results) -> String {
    let count = |f: fn(&Outcome) -> bool| results.done.iter().filter(|(_, _, o)| f(o)).count();
    let total: Duration = results.done.iter().map(|(_, t, _)| *t).sum();
    let suite = xml_escape(&results.suite);
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n<testsuite name=\"{suite}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" errors=\"0\" time=\"{:.3}\">\n",
        results.done.len(),
        count(|o| matches!(o, Outcome::Failed(_))),
        count(|o| matches!(o, Outcome::NotTested(_))),
        total.as_secs_f64(),
    );
    for (name, time, outcome) in &results.done {
        let head = format!("<testcase name=\"{}\" classname=\"{suite}\" time=\"{:.3}\"", xml_escape(name), time.as_secs_f64());
        match outcome {
            Outcome::Passed => xml.push_str(&format!("{head}/>\n")),
            Outcome::NotTested(why) => xml.push_str(&format!("{head}><skipped message=\"{}\"/></testcase>\n", xml_escape(why))),
            Outcome::Failed(e) => {
                let first = e.lines().next().unwrap_or_default();
                xml.push_str(&format!("{head}><failure message=\"{}\">{}</failure></testcase>\n", xml_escape(first), xml_escape(e)));
            }
        }
    }
    xml.push_str("</testsuite>\n</testsuites>\n");
    xml
}

/// Text for XML content and attributes; characters XML 1.0 can't have
/// (terminal escapes from build output) as `?`.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("&#10;"),
            '\t' | '\r' => out.push(c),
            c if c < ' ' => out.push('?'),
            c => out.push(c),
        }
    }
    out
}

/// What is left of Bazel's timeout for the whole binary (its `TEST_TIMEOUT`
/// variable, in seconds), less a margin: a test still running then fails,
/// naming itself, before Bazel stops the binary. Unbounded without Bazel.
fn time_left() -> Duration {
    static END: OnceLock<Option<Instant>> = OnceLock::new();
    let end = END.get_or_init(|| {
        let timeout = Duration::from_secs(std::env::var("TEST_TIMEOUT").ok()?.parse().ok()?);
        Some(Instant::now() + timeout - (timeout / 20).max(Duration::from_secs(5)).min(timeout / 2))
    });
    end.map_or(Duration::MAX, |end| end.saturating_duration_since(Instant::now()))
}

/// Fails the running test once it's past its time: says so, sums up (the
/// tests not run yet aren't in the results), kills what the test started,
/// and exits, leaving the tests' files.
fn watchdog(dir: PathBuf) {
    time_left();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let running = RUNNING.lock().unwrap();
            let Some((name, started, deadline)) = *running else { continue };
            if Instant::now() < deadline {
                continue;
            }
            let e = if started.elapsed() < TEST_TIMEOUT {
                format!("still running after {:?}, at the end of Bazel's timeout", started.elapsed())
            } else {
                format!("timed out after {:?}", started.elapsed())
            };
            println!("test {name} ... FAILED: {e}\n  (its files: {})", dir.join(name).display());
            RESULTS.lock().unwrap().done.push((name, started.elapsed(), Outcome::Failed(e)));
            sum_up();
            println!("kept: {}", dir.display());
            kill_started();
            // Holding `RUNNING`: the test can't finish meanwhile.
            std::process::exit(1);
        }
    });
}

/// Starts `c` for the running test, as `Command::spawn`: a process the
/// watchdog kills if the test runs out of time.
pub fn spawn(c: &mut Command) -> io::Result<Child> {
    let child = c.spawn()?;
    #[cfg(windows)]
    let Ok(handle) = std::os::windows::io::AsHandle::as_handle(&child).try_clone_to_owned() else { return Ok(child) };
    STARTED.lock().unwrap().push(Started {
        pid: child.id(),
        #[cfg(windows)]
        handle,
    });
    Ok(child)
}

/// Runs `c` through `spawn`, as `Command::status`.
pub fn status(c: &mut Command) -> io::Result<ExitStatus> {
    let mut child = spawn(c)?;
    let status = child.wait();
    forget(child.id());
    status
}

/// Runs `c` through `spawn`, as `Command::output` with its default
/// streams: none in, the output and errors piped.
pub fn output(c: &mut Command) -> io::Result<Output> {
    let child = spawn(c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()))?;
    let pid = child.id();
    let output = child.wait_with_output();
    forget(pid);
    output
}

/// Stops tracking process `pid`, waited for.
fn forget(pid: u32) {
    STARTED.lock().unwrap().retain(|s| s.pid != pid);
}

/// Kills what the running test started that still runs.
fn kill_started() {
    for s in STARTED.lock().unwrap().drain(..) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the handle is open (a process that exited stays so).
            unsafe { windows_sys::Win32::System::Threading::TerminateProcess(s.handle.as_raw_handle(), 1) };
        }
        #[cfg(unix)]
        {
            let pid = s.pid as libc::pid_t;
            // SAFETY: plain system calls. Only a child still running: one
            // waited for may have given its id to another process.
            unsafe {
                if libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) == 0 {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
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
/// summary lists it, and Bazel's results have it skipped.
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
        runfile(path).expect("ninja-rs binary not found")
    })
    .clone()
}

/// A data file of the test's Bazel target, `path` from the workspace's
/// top: in the runfiles tree, or (Bazel on Windows, which makes none)
/// where the manifest of them says. None without Bazel.
pub fn runfile(path: &str) -> Option<PathBuf> {
    let key = format!("{}/{path}", std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into()));
    let tree = PathBuf::from(std::env::var_os("TEST_SRCDIR")?).join(&key);
    if tree.exists() {
        return Some(tree);
    }
    let manifest = std::fs::read_to_string(std::env::var_os("RUNFILES_MANIFEST_FILE")?).ok()?;
    manifest.lines().find_map(|l| l.strip_prefix(&key)?.strip_prefix(' ')).map(PathBuf::from)
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
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
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
