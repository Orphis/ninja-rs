//! End-to-end tests of the command line, running the ninja-rs binary: tools,
//! `--status`, the jobserver and interruption. Expected outputs are ninja
//! 1.14's. Arguments select the tests whose names contain them.
//!
//! Commands are this test binary itself: under the name `fake-job` it
//! records how many jobs run at once and the `MAKEFLAGS` it got.

mod harness;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use harness::{ninja_exe, test_dir};
use ninja_rs::jobserver;

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
    check(r.code == 1 && r.stderr.contains(".ninjars:3: unrecognized option '--nope'"), &format!("an unknown option: {}", r.stderr))?;
    let r = run(with(&["--ignore_all_rc_files", "--config=other"]));
    check(r.code == 1 && r.stderr.contains("config value 'other' is not defined"), &format!("--ignore_all_rc_files: {}", r.stderr))?;
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
    harness::link_self(&cl);
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

/// `fake-cc OUT HEADER`: runs for a while, then writes how many were
/// running halfway through (see `fake_job`) to OUT, and a depfile naming
/// HEADER.
fn fake_cc(args: &[String]) -> i32 {
    let [out, header] = args else { return 2 };
    let running = Path::new("running");
    let _ = std::fs::create_dir_all(running);
    let me = running.join(out);
    std::fs::write(&me, "").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let count = std::fs::read_dir(running).unwrap().count();
    std::fs::remove_file(&me).unwrap();
    std::fs::write(out, format!("{count}\n")).unwrap();
    std::fs::write(format!("{out}.d"), format!("{out}: {header}\n")).unwrap();
    0
}

/// Two builds at once in one build directory: one waits for the other (the
/// logs' lock), and each output gets its own deps. Without the lock, both
/// loaded the deps log before the other wrote, and gave their new headers
/// the same id: `b` got `a`'s header, and changing its own rebuilt nothing.
fn test_concurrent_builds() -> Result<(), String> {
    let dir = test_dir("concurrent");
    let cc = dir.join(if cfg!(windows) { "fake-cc.exe" } else { "fake-cc" });
    std::fs::copy(std::env::current_exe().unwrap(), &cc).unwrap();
    for h in ["old.h", "ha.h", "hb.h"] {
        std::fs::write(dir.join(h), "").unwrap();
    }
    let manifest = |a: &str, b: &str| {
        let m = format!(
            "rule cc\n  command = {} $out $header\n  depfile = $out.d\n  deps = gcc\nbuild a: cc\n  header = {a}\nbuild b: cc\n  header = {b}\n",
            cc.display()
        );
        std::fs::write(dir.join("build.ninja"), m).unwrap();
    };
    manifest("old.h", "old.h");
    let r = ninja(&dir, &[], &[]);
    check(r.code == 0, &format!("the first build failed: {}{}", r.stdout, r.stderr))?;
    manifest("ha.h", "hb.h");
    let start = |target: &str| {
        let mut c = Command::new(ninja_exe());
        c.current_dir(&dir).arg(target).env_remove("MAKEFLAGS").env_remove("NINJA_STATUS").env_remove(ninja_rs::nested::ENV);
        c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap()
    };
    let (a, b) = (start("a"), start("b"));
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    let text = |o: &std::process::Output| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    check(a.status.success() && b.status.success(), &format!("a build failed:\n{}\n{}", text(&a), text(&b)))?;
    for out in ["a", "b"] {
        let running = std::fs::read_to_string(dir.join(out)).unwrap_or_default();
        check(running.trim() == "1", &format!("{out} ran with another command at once: {running}"))?;
    }
    let deps = ninja(&dir, &["-t", "deps"], &[]).stdout;
    let of = |out: &str| deps.split("\n\n").find(|b| b.starts_with(&format!("{out}: "))).unwrap_or_default().to_string();
    check(of("a").contains("ha.h") && !of("a").contains("hb.h"), &format!("a's deps:\n{deps}"))?;
    check(of("b").contains("hb.h") && !of("b").contains("ha.h"), &format!("b's deps:\n{deps}"))?;
    let r = ninja(&dir, &["-n", "-d", "explain", "b"], &[]);
    check(r.stdout.contains("no work to do"), &format!("b isn't up to date: {}{}", r.stdout, r.stderr))
}

/// As ninja: a failed build exits with the code of the last command that
/// failed (1.13); an unknown option prints the usage and exits with 1; a
/// build file needing a newer ninja is fatal.
fn test_exit_codes() -> Result<(), String> {
    let dir = test_dir("exit_codes");
    let fail = if cfg!(windows) { "cmd /c exit 3" } else { "exit 3" };
    std::fs::write(dir.join("build.ninja"), format!("rule fail\n  command = {fail}\nbuild out: fail\n")).unwrap();
    let r = ninja(&dir, &[], &[]);
    check(r.code == 3 && r.stdout.contains("FAILED: [code=3] out"), &format!("a failed command: exit {}: {}{}", r.code, r.stdout, r.stderr))?;
    let r = ninja(&dir, &["--bogus"], &[]);
    check(r.code == 1 && r.stderr.starts_with("ninja: unrecognized option '--bogus'\nusage:"), &format!("--bogus: exit {}: {}", r.code, r.stderr))?;
    let r = ninja(&dir, &["-j", "x"], &[]);
    check(r.code == 1 && r.stderr.contains("ninja: fatal: invalid -j parameter"), &format!("-j x: exit {}: {}", r.code, r.stderr))?;
    std::fs::write(dir.join("build.ninja"), "ninja_required_version = 99.0\n").unwrap();
    let r = ninja(&dir, &[], &[]);
    check(
        r.code == 1
            && r.stderr.contains("ninja: fatal: ninja version (")
            && r.stderr.contains("incompatible with build file ninja_required_version version (99.0)."),
        &format!("ninja_required_version: exit {}: {}", r.code, r.stderr),
    )
}

/// Waits for `child` to exit, killing it after `limit`: None then.
#[cfg(unix)]
fn wait_at_most(child: &mut std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    while start.elapsed() < limit {
        if let Ok(Some(s)) = child.try_wait() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// An interrupt outside a build's loop (here while connecting to a remote
/// executor that never answers) ends the invocation at once, as ninja dies
/// of the signal there; it used to go on until the connection timed out.
#[cfg(unix)]
fn test_interrupt_outside_the_build() -> Result<(), String> {
    let dir = test_dir("interrupt_outside");
    std::fs::write(dir.join("build.ninja"), "rbe_version = 1\nrule r\n  command = touch $out\nbuild out: r\n").unwrap();
    let silent = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = silent.local_addr().unwrap().port();
    // Accepted, never answered.
    let (connected, connecting) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in silent.incoming().flatten() {
            held.push(s);
            let _ = connected.send(());
        }
    });
    let mut c = Command::new(ninja_exe());
    c.current_dir(&dir).arg(format!("--remote_executor=grpc://127.0.0.1:{port}")).env_remove(ninja_rs::nested::ENV);
    c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    if connecting.recv_timeout(Duration::from_secs(10)).is_err() {
        let _ = child.kill();
        return Err("ninja-rs didn't connect".into());
    }
    // SAFETY: plain system call.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    match wait_at_most(&mut child, Duration::from_secs(10)) {
        Some(s) => check(s.code() == Some(128 + libc::SIGTERM), &format!("exited with {s}")),
        None => Err("still running 10 s after SIGTERM".into()),
    }
}

/// `-t exec` passes SIGTERM on to its command (a terminal never sends it to
/// the command too), and exits with the command's code.
#[cfg(unix)]
fn test_exec_forwards_signals() -> Result<(), String> {
    let dir = test_dir("exec_signals");
    let script = "trap 'echo > got-term; exit 7' TERM; echo > ready; while :; do sleep 0.1; done";
    let mut c = Command::new(ninja_exe());
    c.current_dir(&dir).args(["-t", "exec", "sh", "-c", script]).env_remove(ninja_rs::nested::ENV);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let start = std::time::Instant::now();
    while !dir.join("ready").exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: plain system call.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    match wait_at_most(&mut child, Duration::from_secs(10)) {
        Some(s) => check(s.code() == Some(7) && dir.join("got-term").exists(), &format!("exited with {s}")),
        None => Err("still running 10 s after SIGTERM".into()),
    }
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
    harness::link_self(&job);
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

/// `fake-sleep OUT`: says it runs (`OUT.pid`), then sleeps for longer than
/// the tests wait, then writes `OUT.marker` and `OUT`, which an interrupted
/// build must stop it from doing.
fn fake_sleep(args: &[String]) -> i32 {
    let out = &args[0];
    std::fs::write(format!("{out}.pid.tmp"), std::process::id().to_string()).unwrap();
    std::fs::rename(format!("{out}.pid.tmp"), format!("{out}.pid")).unwrap();
    std::thread::sleep(2 * harness::PATIENCE);
    std::fs::write(format!("{out}.marker"), "late").unwrap();
    std::fs::write(out, "done").unwrap();
    0
}

/// Starts (with `launch`, given the test's directory) a build of two
/// commands whose grandchildren sleep (`fake-sleep`), and once they run,
/// interrupts it with `interrupt`: it must stop at once like ninja, without
/// waiting for its commands, end its build events, stop the grandchildren,
/// and leave nothing behind (its secret flags file included). An interrupt
/// that can't be sent makes the test not tested.
fn interrupted_build(
    name: &str,
    launch: impl FnOnce(&Path) -> Command,
    interrupt: impl FnOnce(&mut std::process::Child) -> Result<(), String>,
) -> Result<(), String> {
    let dir = test_dir(name);
    harness::link_self(&dir.join(if cfg!(windows) { "fake-sleep.exe" } else { "fake-sleep" }));
    let command = if cfg!(windows) { "cmd /c cmd /c \".\\fake-sleep $out\"" } else { "sh -c 'sh -c \"./fake-sleep $out\"; true'" };
    std::fs::write(dir.join("build.ninja"), format!("rule slow\n  command = {command}\nbuild a: slow\nbuild b: slow\n")).unwrap();
    // Where the secret flags file for nested builds goes.
    let private = dir.join("private");
    std::fs::create_dir(&private).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&private, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let mut c = launch(&dir);
    c.current_dir(&dir).args(["-j2", "--remote_header=x=y", "--build_event_binary_file=events.bin"]);
    c.env_remove("MAKEFLAGS").env_remove("NINJA_STATUS").env_remove(ninja_rs::nested::ENV).env("NINJA_RS_DAEMON_DIR", &private);
    // Files, not pipes: nothing blocks on them while the test waits.
    c.stdout(std::fs::File::create(dir.join("stdout.txt")).unwrap()).stderr(std::fs::File::create(dir.join("stderr.txt")).unwrap());
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let started = || dir.join("a.pid").exists() && dir.join("b.pid").exists();
    let running = harness::wait_for("the commands running", || started() || child.try_wait().is_ok_and(|s| s.is_some()))
        .and_then(|()| if started() { Ok(()) } else { Err("ninja-rs exited before its commands ran".into()) });
    let flags_written = std::fs::read_dir(&private).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("flags-"));
    let interrupted = running.and_then(|()| interrupt(&mut child));
    // It exits long before its commands would end.
    let exited = harness::wait_for("ninja-rs exiting", || child.try_wait().is_ok_and(|s| s.is_some()));
    if exited.is_err() || interrupted.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
    let text = format!("{}{}", read("stdout.txt"), read("stderr.txt")).replace("\r\n", "\n");
    interrupted.map_err(|e| format!("{name}: {e}\n{text}"))?;
    #[cfg(windows)]
    if status.code() == Some(NO_CTRL_BREAK) {
        harness::not_tested(&format!("{name}: {}", text.trim()));
        return Ok(());
    }
    exited.map_err(|e| format!("{name}: {e}\n{text}"))?;
    check(status.code() == Some(130), &format!("{name}: {status}\n{text}"))?;
    check(text.contains("ninja: build stopped: interrupted by user.\n"), &format!("{name}:\n{text}"))?;
    check(flags_written, &format!("{name}: no secret flags file while building"))?;
    let events = std::fs::read(dir.join("events.bin")).unwrap_or_default();
    check(events.windows(b"INTERRUPTED".len()).any(|w| w == b"INTERRUPTED"), &format!("{name}: no interrupted build in the build events"))?;
    // The grandchildren got it too: they're gone, before they wrote.
    for out in ["a", "b"] {
        let pid: u32 = read(&format!("{out}.pid")).trim().parse().map_err(|e| format!("{name}: {out}.pid: {e}"))?;
        harness::wait_for(&format!("{name}: {out}'s command stopped"), || !harness::alive(pid))?;
    }
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
        interrupted_build(
            name,
            |_| Command::new(ninja_exe()),
            |child| {
                // SAFETY: plain system call.
                unsafe { libc::kill(child.id() as i32, signal) };
                Ok(())
            },
        )?;
    }
    Ok(())
}

/// What `console_host` exits with when it can't send Ctrl-Break.
#[cfg(windows)]
const NO_CTRL_BREAK: i32 = 125;

/// `console-host PROGRAM ARGS...`: runs PROGRAM in a process group of its
/// own, on this process's console, and sends it Ctrl-Break when a line
/// arrives on stdin; exits with its exit code. Started with
/// CREATE_NO_WINDOW, it has a console (without a window), which the test
/// may not have (CI, Bazel) and Ctrl-Break needs.
#[cfg(windows)]
fn console_host(args: &[String]) -> i32 {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    const CTRL_BREAK_EVENT: u32 = 1;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    }
    let mut child = match Command::new(&args[0]).args(&args[1..]).stdin(std::process::Stdio::null()).creation_flags(CREATE_NEW_PROCESS_GROUP).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("console-host: {}: {e}", args[0]);
            return 1;
        }
    };
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        let _ = child.kill();
        let _ = child.wait();
        return 1;
    }
    // SAFETY: plain system call; the child leads a process group of its
    // own, on this console.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) } == 0 {
        eprintln!("console-host: can't send Ctrl-Break: {}", std::io::Error::last_os_error());
        let _ = child.kill();
        let _ = child.wait();
        return NO_CTRL_BREAK;
    }
    child.wait().map_or(1, |s| s.code().unwrap_or(1))
}

/// Ctrl-Break interrupts a build as Ctrl-C does, as in ninja; the commands'
/// jobs are terminated and waited for.
#[cfg(windows)]
fn test_interrupt_ctrl_break() -> Result<(), String> {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    interrupted_build(
        "ctrl-break",
        |dir| {
            let host = dir.join("console-host.exe");
            harness::link_self(&host);
            let mut c = Command::new(host);
            c.arg(ninja_exe()).stdin(std::process::Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
            c
        },
        |child| writeln!(child.stdin.take().expect("stdin"), "break").map_err(|e| format!("console-host: {e}")),
    )
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
    if stem.starts_with("fake-cc") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_cc(&args) as u8);
    }
    if stem.starts_with("fake-sleep") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_sleep(&args) as u8);
    }
    #[cfg(windows)]
    if stem.starts_with("console-host") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(console_host(&args) as u8);
    }
    use harness::Test;
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
        ("concurrent_builds", test_concurrent_builds),
        ("exit_codes", test_exit_codes),
    ];
    #[cfg(unix)]
    tests.push(("jobserver_pipe", test_jobserver_pipe));
    #[cfg(unix)]
    tests.push(("interrupt_signals", test_interrupt_signals));
    #[cfg(unix)]
    tests.push(("interrupt_outside_the_build", test_interrupt_outside_the_build));
    #[cfg(unix)]
    tests.push(("exec_forwards_signals", test_exec_forwards_signals));
    #[cfg(windows)]
    tests.push(("msvc_deps_relative", test_msvc_deps_relative));
    #[cfg(windows)]
    tests.push(("interrupt_ctrl_break", test_interrupt_ctrl_break));
    harness::run("cli", tests)
}
