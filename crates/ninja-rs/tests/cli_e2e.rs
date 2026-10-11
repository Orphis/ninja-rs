//! End-to-end tests of the command line, running the ninja-rs binary: tools,
//! `--status` and the jobserver. Expected outputs are ninja 1.14's.
//!
//! Commands are this test binary itself: under the name `fake-job` it
//! records how many jobs run at once and the `MAKEFLAGS` it got.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use ninja_rs::jobserver;

fn ninja_exe() -> PathBuf {
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
}

fn test_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("TEST_TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("ninja-rs-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn ninja(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut c = Command::new(ninja_exe());
    c.current_dir(dir).args(args).env_remove("MAKEFLAGS").env_remove("NINJA_STATUS");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    let text = |b: &[u8]| String::from_utf8_lossy(b).replace("\r\n", "\n");
    Run { code: out.status.code().unwrap_or(-1), stdout: text(&out.stdout), stderr: text(&out.stderr) }
}

fn check(cond: bool, msg: &str) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg.to_string()) }
}

fn check_eq(got: &str, want: &str, what: &str) -> Result<(), String> {
    check(got == want, &format!("{what}: got\n{got}\nwant\n{want}"))
}

// ---------------------------------------------------------------------------
// fake-job
// ---------------------------------------------------------------------------

/// `fake-job OUT`: runs for a while, then writes how many jobs were running
/// halfway through and its `MAKEFLAGS` to OUT.
fn fake_job(args: &[String]) -> i32 {
    let out = &args[0];
    let running = Path::new("running");
    let _ = std::fs::create_dir_all(running);
    let me = running.join(out);
    std::fs::write(&me, "").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let count = std::fs::read_dir(running).unwrap().count();
    std::fs::remove_file(&me).unwrap();
    let makeflags = std::env::var("MAKEFLAGS").unwrap_or_default();
    std::fs::write(out, format!("{count}\n{makeflags}\n")).unwrap();
    0
}

/// A manifest of `jobs` independent fake jobs.
fn jobs_fixture(name: &str, jobs: usize, header: &str) -> PathBuf {
    let dir = test_dir(name);
    let job = dir.join(if cfg!(windows) { "fake-job.exe" } else { "fake-job" });
    std::fs::copy(std::env::current_exe().unwrap(), &job).unwrap();
    let mut m = format!("{header}rule job\n  command = {} $out\n", job.display());
    let outs: Vec<String> = (1..=jobs).map(|i| format!("j{i}")).collect();
    for o in &outs {
        m += &format!("build {o}: job\n");
    }
    m += &format!("build all: phony {}\ndefault all\n", outs.join(" "));
    std::fs::write(dir.join("build.ninja"), m).unwrap();
    dir
}

/// Each job's (concurrency, MAKEFLAGS).
fn job_results(dir: &Path, jobs: usize) -> Vec<(usize, String)> {
    (1..=jobs)
        .map(|i| {
            let s = std::fs::read_to_string(dir.join(format!("j{i}"))).unwrap();
            let (n, flags) = s.split_once('\n').unwrap();
            (n.parse().unwrap(), flags.trim_end().to_string())
        })
        .collect()
}

fn max_running(results: &[(usize, String)]) -> usize {
    results.iter().map(|r| r.0).max().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tools and status
// ---------------------------------------------------------------------------

const TOOLS_MANIFEST: &str = "rule cc
  command = cc -c $in -o $out
  description = CC $out
  depfile = $out.d
rule link
  command = link $in -o $out
rule gen
  command = gen $out
build gen.h: gen
build a.o: cc a.c
build b.o: cc b.c | gen.h
build app: link a.o b.o
default app
";

fn tools_fixture(name: &str) -> PathBuf {
    let dir = test_dir(name);
    std::fs::write(dir.join("build.ninja"), TOOLS_MANIFEST).unwrap();
    dir
}

fn test_rules() -> Result<(), String> {
    let dir = tools_fixture("rules");
    check_eq(&ninja(&dir, &["-t", "rules"], &[]).stdout, "cc\ngen\nlink\nphony\n", "-t rules")?;
    check_eq(&ninja(&dir, &["-t", "rules", "-d"], &[]).stdout, "cc: CC ${out}\ngen\nlink\nphony\n", "-t rules -d")
}

fn test_inputs() -> Result<(), String> {
    let dir = tools_fixture("inputs");
    check_eq(&ninja(&dir, &["-t", "inputs", "app"], &[]).stdout, "a.c\na.o\nb.c\nb.o\ngen.h\n", "-t inputs")?;
    check_eq(&ninja(&dir, &["-t", "inputs", "-d", "app"], &[]).stdout, "a.c\na.o\nb.c\ngen.h\nb.o\n", "-t inputs -d")?;
    check_eq(&ninja(&dir, &["-t", "inputs", "-0", "b.o"], &[]).stdout, "b.c\0gen.h\0", "-t inputs -0")?;
    let r = ninja(&dir, &["-t", "inputs", "-x"], &[]);
    check(r.code == 1 && r.stdout.starts_with("Usage '-t inputs"), "-t inputs -x prints the usage")?;
    // Shell escaping, unless -E.
    std::fs::write(dir.join("build.ninja"), "build out: phony a$ b.c\n").unwrap();
    let escaped = if cfg!(windows) { "\"a b.c\"\n" } else { "'a b.c'\n" };
    check_eq(&ninja(&dir, &["-t", "inputs", "out"], &[]).stdout, escaped, "-t inputs escapes")?;
    check_eq(&ninja(&dir, &["-t", "inputs", "-E", "out"], &[]).stdout, "a b.c\n", "-t inputs -E")
}

fn test_compdb() -> Result<(), String> {
    let dir = tools_fixture("compdb");
    let r = ninja(&dir, &["-t", "compdb", "cc"], &[]);
    check(r.code == 0, "-t compdb failed")?;
    // One entry per input, the order-only and implicit ones included.
    for (source, file, output) in [("a.c", "a.c", "a.o"), ("b.c", "b.c", "b.o"), ("b.c", "gen.h", "b.o")] {
        let entry = format!("    \"command\": \"cc -c {source} -o {output}\",\n    \"file\": \"{file}\",\n    \"output\": \"{output}\"\n");
        check(r.stdout.contains(&entry), &format!("-t compdb: no entry for {file}:\n{}", r.stdout))?;
    }
    let r = ninja(&dir, &["-t", "compdb-targets", "a.o"], &[]);
    check(r.stdout.matches("\"file\"").count() == 1 && r.stdout.contains("\"output\": \"a.o\""), &format!("-t compdb-targets:\n{}", r.stdout))?;
    let r = ninja(&dir, &["-t", "compdb-targets"], &[]);
    check(r.code == 1, "-t compdb-targets without targets must fail")
}

fn test_missingdeps() -> Result<(), String> {
    let dir = tools_fixture("missingdeps");
    std::fs::write(dir.join("a.o.d"), "a.o: a.c gen.h\n").unwrap();
    let r = ninja(&dir, &["-t", "missingdeps"], &[]);
    check(r.code == 3, &format!("-t missingdeps exit code {}", r.code))?;
    check_eq(
        &r.stdout,
        "Missing dep: a.o uses gen.h (generated by gen)\n\
Processed 4 nodes.\n\
Error: There are 1 missing dependency paths.\n\
1 targets had depfile dependencies on 1 distinct generated inputs (from 1 rules)  without a non-depfile dep path to the generator.\n\
There might be build flakiness if any of the targets listed above are built alone, or not late enough, in a clean output directory.\n",
        "-t missingdeps",
    )
}

fn test_status_format() -> Result<(), String> {
    let dir = tools_fixture("status");
    for source in ["a.c", "b.c"] {
        std::fs::write(dir.join(source), "").unwrap();
    }
    let r = ninja(&dir, &["-n", "--status", "[$started/$finished/$total|$running] $description"], &[]);
    check_eq(&r.stdout, "[2/1/4|2] gen gen.h\n[3/2/4|2] CC a.o\n[3/3/4|1] CC b.o\n[4/4/4|1] link a.o b.o -o app\n", "--status")?;
    let r = ninja(&dir, &["-n", "--status", "$nope"], &[]);
    check(r.code == 1 && r.stderr.contains("unknown variable 'nope' in --status format"), &format!("an unknown --status variable must fail:\n{}", r.stderr))
}

/// Failed commands are shown again before "build stopped" when other output
/// followed them (ninja issue 2378), not when the only one is still last.
fn test_failure_summary() -> Result<(), String> {
    let dir = test_dir("summary");
    let (fail, noisy) =
        if cfg!(windows) { ("cmd /c \"echo $out: error& exit 1\"", "cmd /c \"echo $out: output\"") } else { ("echo $out: error; exit 1", "echo $out: output") };
    let manifest = format!(
        "rule fail
  command = {fail}
rule noisy
  command = {noisy}
build a.o: fail
build b.o: noisy
build c.o: fail
build d.o: noisy
build all: phony a.o b.o c.o d.o
default all
"
    );
    std::fs::write(dir.join("build.ninja"), manifest).unwrap();
    let r = ninja(&dir, &["-k", "0", "-j1"], &[]);
    let summary = format!(
        "ninja: 2 commands failed:
FAILED: a.o
{}
a.o: error
FAILED: c.o
",
        fail.replace("$out", "a.o")
    );
    let at = r.stdout.find(&summary).ok_or_else(|| {
        format!(
            "no summary:
{}",
            r.stdout
        )
    })?;
    check(
        r.code == 1 && r.stdout[at..].contains("ninja: build stopped"),
        &format!(
            "summary not before build stopped:
{}",
            r.stdout
        ),
    )?;
    let r = ninja(&dir, &["-j1", "a.o"], &[]);
    check(
        r.code == 1 && !r.stdout.contains("failed:"),
        &format!(
            "summary of a failure still last:
{}",
            r.stdout
        ),
    )
}

// ---------------------------------------------------------------------------
// Jobserver
// ---------------------------------------------------------------------------

const JOBS: usize = 6;

fn test_jobserver_pool() -> Result<(), String> {
    let dir = jobs_fixture("pool", JOBS, "");
    let r = ninja(&dir, &["-j2", "--jobserver-pool"], &[]);
    check(r.code == 0, &format!("build failed:\n{}", r.stdout))?;
    check(r.stdout.contains("ninja: Jobserver mode detected:  -j2 --jobserver-auth="), &format!("no jobserver message:\n{}", r.stdout))?;
    let results = job_results(&dir, JOBS);
    check(max_running(&results) <= 2, &format!("more than 2 jobs at once: {results:?}"))?;
    check(results.iter().all(|(_, f)| f.starts_with(" -j2 --jobserver-auth=")), &format!("commands didn't get the pool: {results:?}"))
}

fn test_jobserver_pool_from_manifest() -> Result<(), String> {
    let dir = jobs_fixture("poolvar", 2, "enable_jobserver_pool = 1\n");
    check(ninja(&dir, &["-j3"], &[]).code == 0, "build failed")?;
    let results = job_results(&dir, 2);
    check(results.iter().all(|(_, f)| f.starts_with(" -j3 --jobserver-auth=")), &format!("commands didn't get the pool: {results:?}"))?;
    // Without it, no pool.
    let dir = jobs_fixture("nopool", 2, "");
    check(ninja(&dir, &["-j3"], &[]).code == 0, "build failed")?;
    let results = job_results(&dir, 2);
    check(results.iter().all(|(_, f)| f.is_empty()), &format!("unexpected MAKEFLAGS: {results:?}"))
}

fn test_jobserver_client() -> Result<(), String> {
    // A pool of 3 slots, one of them held elsewhere: ninja-rs gets its
    // implicit slot and the one token left.
    let pool = jobserver::Pool::new(3)?;
    let flags = pool.makeflags();
    let other = jobserver::Client::new(&jobserver::parse_native_makeflags(&flags)?)?;
    let _implicit = other.try_acquire().ok_or("no implicit slot")?;
    let _token = other.try_acquire().ok_or("no token")?;
    let dir = jobs_fixture("client", JOBS, "");
    let r = ninja(&dir, &[], &[("MAKEFLAGS", &flags)]);
    check(r.code == 0, &format!("build failed:\n{}", r.stdout))?;
    check(r.stdout.contains("ninja: Jobserver mode detected:"), &format!("no jobserver message:\n{}", r.stdout))?;
    let results = job_results(&dir, JOBS);
    check(max_running(&results) <= 2, &format!("more than 2 jobs at once: {results:?}"))?;
    // Every token went back.
    drop(_token);
    let held: Vec<_> = std::iter::from_fn(|| other.try_acquire()).collect();
    check(held.len() == 2, &format!("{} tokens left in the pool, expected 2", held.len()))?;
    drop(held);
    // -j wins over the pool.
    let dir = jobs_fixture("explicit", 2, "");
    let r = ninja(&dir, &["-j4"], &[("MAKEFLAGS", &flags)]);
    check(r.code == 0 && !r.stdout.contains("Jobserver mode detected"), &format!("the pool wasn't ignored:\n{}", r.stdout))?;
    // `make -n`: no jobserver.
    let r = ninja(&dir, &[], &[("MAKEFLAGS", &format!("n{flags}"))]);
    check(!r.stdout.contains("Jobserver mode detected"), &format!("make -n:\n{}", r.stdout))
}

#[cfg(unix)]
fn test_jobserver_pipe() -> Result<(), String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    // GNU make 4.3: an inherited pipe with one token.
    let (r, mut w) = std::io::pipe().map_err(|e| e.to_string())?;
    w.write_all(b"+").map_err(|e| e.to_string())?;
    for fd in [r.as_raw_fd(), w.as_raw_fd()] {
        // SAFETY: plain fcntl calls on open descriptors.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    }
    let flags = format!(" -j2 --jobserver-auth={},{}", r.as_raw_fd(), w.as_raw_fd());
    let dir = jobs_fixture("pipe", 4, "");
    let out = ninja(&dir, &[], &[("MAKEFLAGS", &flags)]);
    check(out.code == 0, &format!("build failed:\n{}", out.stdout))?;
    check(out.stderr.contains("ninja: warning: Jobserver 'pipe' mode detected"), &format!("no pipe warning:\n{}", out.stderr))?;
    let results = job_results(&dir, 4);
    check(max_running(&results) <= 2, &format!("more than 2 jobs at once: {results:?}"))?;
    // The token is back.
    let client = jobserver::Client::new(&jobserver::Mode::Pipe { read: r.as_raw_fd(), write: w.as_raw_fd() })?;
    let _implicit = client.try_acquire();
    check(client.try_acquire().is_some(), "the token didn't come back")
}

fn main() -> ExitCode {
    let exe = std::env::current_exe().unwrap();
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem.starts_with("fake-job") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_job(&args) as u8);
    }
    type Test = (&'static str, fn() -> Result<(), String>);
    #[allow(unused_mut)]
    let mut tests: Vec<Test> = vec![
        ("rules", test_rules),
        ("inputs", test_inputs),
        ("compdb", test_compdb),
        ("missingdeps", test_missingdeps),
        ("status_format", test_status_format),
        ("failure_summary", test_failure_summary),
        ("jobserver_pool", test_jobserver_pool),
        ("jobserver_pool_from_manifest", test_jobserver_pool_from_manifest),
        ("jobserver_client", test_jobserver_client),
    ];
    #[cfg(unix)]
    tests.push(("jobserver_pipe", test_jobserver_pipe));
    let count = tests.len();
    let mut failed = 0;
    for (name, t) in tests {
        match t() {
            Ok(()) => println!("test {name} ... ok"),
            Err(e) => {
                println!("test {name} ... FAILED: {e}");
                failed += 1;
            }
        }
    }
    println!("test result: {} passed; {failed} failed", count - failed);
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
