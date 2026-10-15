//! End-to-end tests of the command line, running the ninja-rs binary: tools,
//! `--status`, the jobserver and interruption. Expected outputs are ninja
//! 1.14's. Arguments select the tests whose names contain them.
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
    c.current_dir(dir).args(args).env_remove("MAKEFLAGS").env_remove("NINJA_STATUS").env_remove(ninja_rs::nested::ENV);
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

/// Build events report an up-to-date artifact with the digest kept from
/// earlier builds (`.ninja_rs_digests`), remote or not: a build with
/// nothing to do doesn't hash it again.
fn test_build_event_digests() -> Result<(), String> {
    let dir = test_dir("build_event_digests");
    let manifest = "rule gen\n  command = gen\n  generator = 1\nbuild app.bin: gen\n  rbe_target = app\n  rbe_artifact = 1\n";
    std::fs::write(dir.join("build.ninja"), manifest).unwrap();
    let artifact = dir.join("app.bin");
    std::fs::write(&artifact, "hello").unwrap();
    // Modified a while ago: a recent file's digest isn't kept.
    let old = std::time::SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options().write(true).open(&artifact).unwrap().set_modified(old).unwrap();
    let hello = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    let reported = |digest: &str| std::fs::read(dir.join("events.bep")).is_ok_and(|e| e.windows(digest.len()).any(|w| w == digest.as_bytes()));
    let r = ninja(&dir, &["--build_event_binary_file=events.bep"], &[]);
    check(r.code == 0, &format!("the build failed: {}", r.stderr))?;
    check(reported(hello), "app.bin's digest isn't reported")?;
    let kept = std::fs::read_to_string(dir.join(".ninja_rs_digests")).map_err(|e| format!(".ninja_rs_digests: {e}"))?;
    check(kept.contains(hello), &format!("app.bin's digest isn't kept: {kept}"))?;
    // A digest only the kept one gives.
    let other = "f".repeat(64);
    std::fs::write(dir.join(".ninja_rs_digests"), kept.replace(hello, &other)).unwrap();
    let r = ninja(&dir, &["--build_event_binary_file=events.bep"], &[]);
    check(r.code == 0, &format!("the second build failed: {}", r.stderr))?;
    check(reported(&other), "app.bin was hashed again")
}

// ---------------------------------------------------------------------------
// fake-job
// ---------------------------------------------------------------------------

/// `.ninjars` files: the workspace's found from the build directory up,
/// their options before the command line's, `--config`s expanded; and
/// `-t exec`, which takes them too.
fn test_ninjars() -> Result<(), String> {
    let dir = test_dir("ninjars");
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(
        dir.join(".ninjars"),
        "build --build_event_binary_file=%workspace%/a.bep\n\
         build:other --build_event_binary_file=%workspace%/b.bep\n\
         build:bad --nope\n\
         common --some_bazel_flag\n\
         test --test_output=errors\n",
    )
    .unwrap();
    std::fs::write(out.join("build.ninja"), "build all: phony\ndefault all\n").unwrap();
    let base = ["-C", "out", "--nosystem_rc", "--nohome_rc"];
    let with = |more: &[&str]| -> Vec<String> { base.iter().chain(more).map(|s| s.to_string()).collect() };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        ninja(&dir, &refs, &[])
    };
    let r = run(with(&[]));
    check(r.code == 0 && dir.join("a.bep").is_file(), &format!("the workspace's options: {}{}", r.stdout, r.stderr))?;
    let r = run(with(&["--config=other", "--announce_rc"]));
    check(r.code == 0 && dir.join("b.bep").is_file(), &format!("--config: {}{}", r.stdout, r.stderr))?;
    for want in ["reading rc options for 'build' from", "found applicable config definition build:other"] {
        check(r.stderr.contains(want), &format!("--announce_rc: no '{want}' in\n{}", r.stderr))?;
    }
    let r = run(with(&["--config=bad"]));
    check(r.code == 2 && r.stderr.contains(".ninjars:3: unrecognized option '--nope'"), &format!("an unknown option: {}", r.stderr))?;
    let r = run(with(&["--ignore_all_rc_files", "--config=other"]));
    check(r.code == 2 && r.stderr.contains("config value 'other' is not defined"), &format!("--ignore_all_rc_files: {}", r.stderr))?;
    // -t exec: the command's exit code.
    let exe = ninja_exe().to_string_lossy().into_owned();
    let r = run(with(&["-t", "exec", "--", &exe, "nothing"]));
    check(r.code == 1 && r.stderr.contains("unknown target 'nothing'"), &format!("-t exec: {}{}", r.stdout, r.stderr))?;
    let r = run(with(&["-t", "exec", "--", &exe]));
    check(r.code == 0, &format!("-t exec: {}{}", r.stdout, r.stderr))?;
    let r = run(with(&["-t", "exec", "no-such-program-here"]));
    check(r.code == 127, &format!("-t exec of a missing program: {} {}", r.code, r.stderr))?;
    Ok(())
}

/// `fake-cl gen OUT` writes OUT; `fake-cl cc OUT HEADER` writes OUT and
/// reports HEADER as `cl /showIncludes` does.
fn fake_cl(args: &[String]) -> i32 {
    if let [mode, out, rest @ ..] = args {
        if let Some(dir) = Path::new(out).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(out, "").unwrap();
        if mode == "cc" {
            for h in rest {
                println!("Note: including file: {h}");
            }
        }
        return 0;
    }
    2
}

/// `deps = msvc`: absolute `/showIncludes` paths are recorded relative to
/// the build directory, as ninja does, so that a generated header is the
/// node its edge produces (and not a source statted before it is made).
#[cfg(windows)]
fn test_msvc_deps_relative() -> Result<(), String> {
    let dir = test_dir("msvcdeps");
    let cl = dir.join("fake-cl.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &cl).unwrap();
    let header = dir.join("gen").join("x.h");
    let m = format!(
        "rule gen\n  command = {cl} gen $out\nrule cc\n  command = {cl} cc $out {header}\n  deps = msvc\n\
         build gen/x.h: gen\nbuild out.obj: cc || gen/x.h\ndefault out.obj\n",
        cl = cl.display(),
        header = header.display(),
    );
    std::fs::write(dir.join("build.ninja"), m).unwrap();
    let r = ninja(&dir, &[], &[]);
    check(r.code == 0, &format!("build failed: {}{}", r.stdout, r.stderr))?;
    let r = ninja(&dir, &["-t", "deps", "out.obj"], &[]);
    check(r.stdout.contains("    gen/x.h\n") && !r.stdout.contains(":\\") && !r.stdout.contains(":/"), &format!("deps of out.obj:\n{}", r.stdout))
}

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

/// `-t wincodepage` says build files are UTF-8 (CMake writes them in the
/// ANSI code page otherwise), and they are read so.
fn test_wincodepage() -> Result<(), String> {
    let dir = test_dir("wincodepage");
    check_eq(&ninja(&dir, &["-t", "wincodepage"], &[]).stdout, "Build file encoding: UTF-8\n", "-t wincodepage")?;
    check(ninja(&dir, &["-t", "wincodepage", "x"], &[]).code == 1, "-t wincodepage x prints the usage")?;
    std::fs::write(dir.join("build.ninja"), "build caf\u{e9}/\u{65e5}\u{672c}.o: phony\n").unwrap();
    check_eq(&ninja(&dir, &["-t", "targets", "all"], &[]).stdout, "caf\u{e9}/\u{65e5}\u{672c}.o: phony\n", "-t targets")
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

/// Starts a build of two commands whose grandchildren write a marker and
/// the output after 2 s, and interrupts it after 1 s with `interrupt`: it
/// must stop at once like ninja, end its build events, and leave nothing
/// behind (its secret flags file included) or written later. Skipped when
/// `interrupt` can't interrupt.
fn interrupted_build(name: &str, interrupt: impl FnOnce(&std::process::Child) -> Result<(), String>) -> Result<(), String> {
    use std::process::Stdio;
    let dir = test_dir(name);
    let command = if cfg!(windows) {
        "cmd /c cmd /c \"ping -n 3 127.0.0.1 >nul & echo late> $out.marker & echo done> $out\""
    } else {
        "sh -c 'sh -c \"sleep 2; echo late > $out.marker; echo done > $out\"; true'"
    };
    std::fs::write(dir.join("build.ninja"), format!("rule slow\n  command = {command}\nbuild a: slow\nbuild b: slow\n")).unwrap();
    // Where the secret flags file for nested builds goes.
    let private = dir.join("private");
    std::fs::create_dir(&private).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&private, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let mut c = Command::new(ninja_exe());
    c.current_dir(&dir).args(["-j2", "--remote_header=x=y", "--build_event_binary_file=events.bin"]);
    c.env_remove("MAKEFLAGS").env_remove("NINJA_STATUS").env_remove(ninja_rs::nested::ENV).env("NINJA_RS_DAEMON_DIR", &private);
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x200); // CREATE_NEW_PROCESS_GROUP
    let start = std::time::Instant::now();
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_secs(1));
    let flags_written = std::fs::read_dir(&private).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("flags-"));
    if let Err(e) = interrupt(&child) {
        let _ = child.kill();
        let _ = child.wait();
        println!("{name} not tested: {e}");
        return Ok(());
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let exited = start.elapsed();
    std::thread::sleep(Duration::from_secs(4).saturating_sub(start.elapsed()));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).replace("\r\n", "\n");
    check(out.status.code() == Some(130), &format!("{name}: {}\n{text}", out.status))?;
    check(text.contains("ninja: build stopped: interrupted by user.\n"), &format!("{name}:\n{text}"))?;
    check(exited < Duration::from_secs(2), &format!("{name}: exited after {exited:?}"))?;
    check(flags_written, &format!("{name}: no secret flags file while building"))?;
    let events = std::fs::read(dir.join("events.bin")).unwrap_or_default();
    check(events.windows(b"INTERRUPTED".len()).any(|w| w == b"INTERRUPTED"), &format!("{name}: no interrupted build in the build events"))?;
    let mut left: Vec<String> = ["a", "b", "a.marker", "b.marker", ".ninja_lock"].into_iter().filter(|f| dir.join(f).exists()).map(String::from).collect();
    left.extend(std::fs::read_dir(&private).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()));
    check(left.is_empty(), &format!("{name}: left behind or written after the build: {left:?}"))
}

/// SIGTERM and SIGHUP interrupt a build as SIGINT does, as in ninja: every
/// process the commands started gets the signal and is waited for before
/// the outputs are deleted, and ninja-rs exits with 130.
#[cfg(unix)]
fn test_interrupt_signals() -> Result<(), String> {
    for (name, signal) in [("sigterm", libc::SIGTERM), ("sighup", libc::SIGHUP), ("sigint", libc::SIGINT)] {
        interrupted_build(name, |child| {
            // SAFETY: plain system call.
            unsafe { libc::kill(child.id() as i32, signal) };
            Ok(())
        })?;
    }
    Ok(())
}

/// Ctrl-Break interrupts a build as Ctrl-C does, as in ninja; the commands'
/// jobs are terminated and waited for.
#[cfg(windows)]
fn test_interrupt_ctrl_break() -> Result<(), String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    }
    interrupted_build("ctrl-break", |child| {
        // SAFETY: plain system call; ninja-rs runs in a process group of
        // its own. It fails without a console.
        match unsafe { GenerateConsoleCtrlEvent(1, child.id()) } {
            0 => Err(format!("Ctrl-Break: {}", std::io::Error::last_os_error())),
            _ => Ok(()),
        }
    })
}

fn main() -> ExitCode {
    let exe = std::env::current_exe().unwrap();
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem.starts_with("fake-job") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_job(&args) as u8);
    }
    if stem.starts_with("fake-cl") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_cl(&args) as u8);
    }
    type Test = (&'static str, fn() -> Result<(), String>);
    #[allow(unused_mut)]
    let mut tests: Vec<Test> = vec![
        ("rules", test_rules),
        ("wincodepage", test_wincodepage),
        ("inputs", test_inputs),
        ("compdb", test_compdb),
        ("missingdeps", test_missingdeps),
        ("status_format", test_status_format),
        ("failure_summary", test_failure_summary),
        ("jobserver_pool", test_jobserver_pool),
        ("jobserver_pool_from_manifest", test_jobserver_pool_from_manifest),
        ("jobserver_client", test_jobserver_client),
        ("ninjars", test_ninjars),
        ("build_event_digests", test_build_event_digests),
    ];
    #[cfg(unix)]
    tests.push(("jobserver_pipe", test_jobserver_pipe));
    #[cfg(unix)]
    tests.push(("interrupt_signals", test_interrupt_signals));
    #[cfg(windows)]
    tests.push(("msvc_deps_relative", test_msvc_deps_relative));
    #[cfg(windows)]
    tests.push(("interrupt_ctrl_break", test_interrupt_ctrl_break));
    let filter: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with('-')).collect();
    tests.retain(|(name, _)| filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str())));
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
    if failed > 0 {
        return ExitCode::FAILURE;
    }
    // The fixtures (copies of this binary, hundreds of MB each), kept
    // after a failure to look into.
    let base = std::env::var_os("TEST_TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let prefix = format!("ninja-rs-cli-{}-", std::process::id());
    for e in std::fs::read_dir(&base).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    ExitCode::SUCCESS
}
