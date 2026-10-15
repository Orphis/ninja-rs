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

/// Leaves out of `c`'s environment what would change how ninja-rs runs
/// (`MAKEFLAGS`, `NINJA_STATUS`, an outer invocation's, the daemon's).
fn hermetic(c: &mut Command) -> &mut Command {
    c.env_remove("MAKEFLAGS").env_remove("NINJA_STATUS");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("NINJA_RS_") {
            c.env_remove(name);
        }
    }
    c
}

/// ninja-rs in `dir`, hermetic, without the settings files of the machine
/// and the user (`/etc/ninjars`, `~/.ninjars`).
fn command(dir: &Path) -> Command {
    let mut c = Command::new(ninja_exe());
    hermetic(&mut c).current_dir(dir).arg("--ignore_all_rc_files");
    c
}

fn ninja(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut c = command(dir);
    c.args(args);
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
    let base = ["-C", "out", "--noignore_all_rc_files", "--nosystem_rc", "--nohome_rc"];
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
    harness::link_self(&cc);
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
        let mut c = command(&dir);
        c.arg(target).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap()
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

// ---------------------------------------------------------------------------
// The logs and their lock
// ---------------------------------------------------------------------------

/// This test binary as the tool `name` in `dir` (see `main`), as a manifest
/// names it.
fn tool(dir: &Path, name: &str) -> String {
    let path = dir.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() });
    harness::link_self(&path);
    format!("\"{}\"", path.display())
}

/// ninja-rs as a manifest's command names it.
fn ninja_command() -> String {
    format!("\"{}\"", ninja_exe().display())
}

/// Starts ninja-rs in `dir`, its output to `name.txt`, as no build runs
/// it.
fn start_ninja(dir: &Path, name: &str, args: &[&str]) -> std::process::Child {
    let mut c = command(dir);
    c.args(args);
    // Files, not pipes: nothing blocks on them while the test waits.
    let out = std::fs::File::create(dir.join(format!("{name}.txt"))).unwrap();
    c.stdout(out.try_clone().unwrap()).stderr(out);
    c.spawn().unwrap()
}

/// Waits for `child` to exit, killing it after `harness::PATIENCE`.
fn wait_exit(child: &mut std::process::Child, what: &str) -> Result<std::process::ExitStatus, String> {
    let waited = harness::wait_for(what, || child.try_wait().is_ok_and(|s| s.is_some()));
    if let Err(e) = waited {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    child.wait().map_err(|e| e.to_string())
}

/// A build log of `records` (output, mtime).
fn build_log(records: impl IntoIterator<Item = (String, u64)>) -> String {
    let mut log = String::from("# ninja-rs log v2\n");
    for (output, mtime) in records {
        log += &format!("1\t2\t{mtime}\t{output}\tabcdef\n");
    }
    log
}

/// The build log in `dir`.
fn logged(dir: &Path) -> Result<ninja_core::build_log::BuildLog, String> {
    let mut log = ninja_core::build_log::BuildLog::new();
    log.load(&dir.join(".ninja_rs_log").to_string_lossy())?;
    Ok(log)
}

/// A command of a Debug build builds a Release tool in the same directory
/// (CMake's Ninja Multi-Config, a custom command running `cmake --build .
/// --config Release`), under the outer build's lock: it appends to the
/// logs, and never rewrites them. Its recompaction lost every record the
/// outer one had (and those the outer one wrote after it, to the file it
/// had open): the next build rebuilt everything.
fn test_nested_build_appends_only() -> Result<(), String> {
    let dir = test_dir("nested_appends_only");
    let cl = tool(&dir, "fake-cl");
    let objects: Vec<String> = (1..=10).map(|i| format!("Debug/o{i}")).collect();
    let mut debug =
        format!("rule cc\n  command = {cl} cc $out Debug.h\n  deps = msvc\nrule nest\n  command = {} -f build-Release.ninja tool\n", ninja_command());
    for o in &objects {
        debug += &format!("build {o}: cc\n");
    }
    debug += &format!("build Debug/gen: nest | {}\ndefault Debug/gen\n", objects.join(" "));
    std::fs::write(dir.join("build-Debug.ninja"), debug).unwrap();
    std::fs::write(dir.join("build-Release.ninja"), format!("rule cc\n  command = {cl} cc $out Release.h\n  deps = msvc\nbuild tool: cc\n")).unwrap();
    for header in ["Debug.h", "Release.h"] {
        std::fs::write(dir.join(header), "").unwrap();
    }
    // Records of 190 other outputs and of the Debug objects, each three
    // times: one more record, and a build loading the log recompacts it.
    let others: Vec<String> = (0..190).map(|i| format!("Other/output{i}.o")).collect();
    let records = others.iter().chain(&objects).flat_map(|o| [(o.clone(), 1), (o.clone(), 2), (o.clone(), 3)]);
    std::fs::write(dir.join(".ninja_rs_log"), build_log(records)).unwrap();
    let r = ninja(&dir, &["-f", "build-Debug.ninja"], &[]);
    check(r.code == 0, &format!("the build failed: {}{}", r.stdout, r.stderr))?;
    let log = logged(&dir)?;
    let missing: Vec<&String> = others.iter().chain(&objects).filter(|o| log.lookup_by_output(o).is_none()).collect();
    check(missing.is_empty(), &format!("{} records lost, {:?}...", missing.len(), missing.iter().take(3).collect::<Vec<_>>()))?;
    check(log.lookup_by_output("tool").is_some() && log.lookup_by_output("Debug/gen").is_some(), "the builds' own records are lost")?;
    let deps = ninja(&dir, &["-f", "build-Debug.ninja", "-t", "deps", "Debug/o1"], &[]).stdout;
    check(deps.contains("    Debug.h\n"), &format!("Debug/o1's deps: {deps}"))?;
    let deps = ninja(&dir, &["-f", "build-Release.ninja", "-t", "deps", "tool"], &[]).stdout;
    check(deps.contains("    Release.h\n"), &format!("tool's deps: {deps}"))?;
    let r = ninja(&dir, &["-f", "build-Debug.ninja", "-n", "-d", "explain", "Debug/o1"], &[]);
    check(r.stdout.contains("no work to do"), &format!("Debug/o1 isn't up to date: {}{}", r.stdout, r.stderr))
}

/// X's command builds Y, whose command builds X again (another target):
/// each nested build knows every lock the builds it runs for hold. It
/// waited forever for X's lock, which only Y's was known to be lent.
fn test_cross_nested_builds() -> Result<(), String> {
    let dir = test_dir("cross_nested");
    let cl = tool(&dir, "fake-cl");
    let (x, y) = (dir.join("x"), dir.join("y"));
    for d in [&x, &y] {
        std::fs::create_dir_all(d).unwrap();
    }
    let nest = |other: &Path, target: &str| format!("rule nest\n  command = {} -C \"{}\" {target}\n", ninja_command(), other.display());
    std::fs::write(x.join("build.ninja"), format!("{}rule gen\n  command = {cl} gen $out\nbuild x2: gen\nbuild x1: nest\ndefault x1\n", nest(&y, "y1")))
        .unwrap();
    std::fs::write(y.join("build.ninja"), format!("{}build y1: nest\n", nest(&x, "x2"))).unwrap();
    let mut child = start_ninja(&x, "out", &[]);
    let status = wait_exit(&mut child, "the build of X")?;
    let out = std::fs::read_to_string(x.join("out.txt")).unwrap_or_default();
    check(status.success() && x.join("x2").exists(), &format!("{status}: {out}"))?;
    check(!out.contains("waiting for"), &format!("a nested build waited: {out}"))
}

/// `fake-gen IN OUT NINJA`: regenerates OUT from IN, then runs what CMake
/// runs at the end of a configure: `NINJA -t recompact`, `NINJA -t restat
/// OUT`.
fn fake_gen(args: &[String]) -> i32 {
    let [input, out, ninja] = args else { return 2 };
    std::fs::copy(input, out).unwrap();
    for tool in [vec!["-t", "recompact"], vec!["-t", "restat", out]] {
        match Command::new(ninja).args(tool).status() {
            Ok(s) if s.success() => {}
            Ok(s) => return s.code().unwrap_or(1),
            Err(_) => return 1,
        }
    }
    0
}

/// A generator regenerating the manifest runs `-t recompact` and `-t
/// restat` (CMake), while the build running it holds the lock: they run
/// at once, for that build, and leave what it appends to alone (the
/// recompaction is skipped, the restat appends). The build then goes on
/// with the new manifest, and the next one has nothing to do.
fn test_generator_under_the_lock() -> Result<(), String> {
    let dir = test_dir("generator_lock");
    let (generator, cl) = (tool(&dir, "fake-gen"), tool(&dir, "fake-cl"));
    let manifest = format!(
        "rule gen\n  command = {generator} build.ninja.in build.ninja {}\n  generator = 1\nbuild build.ninja: gen build.ninja.in\n\
         rule touch\n  command = {cl} gen $out\nbuild out: touch\n",
        ninja_command()
    );
    std::fs::write(dir.join("build.ninja.in"), &manifest).unwrap();
    std::fs::write(dir.join("build.ninja"), &manifest).unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options().write(true).open(dir.join("build.ninja")).unwrap().set_modified(old).unwrap();
    // A record no recompaction would keep.
    std::fs::write(dir.join(".ninja_rs_log"), build_log([("gone.o".to_string(), 1)])).unwrap();
    let mut child = start_ninja(&dir, "out", &[]);
    let status = wait_exit(&mut child, "the build regenerating its manifest")?;
    let out = std::fs::read_to_string(dir.join("out.txt")).unwrap_or_default();
    check(status.success() && dir.join("out").exists(), &format!("{status}: {out}"))?;
    check(!out.contains("waiting for"), &format!("the generator's tools waited: {out}"))?;
    check(logged(&dir)?.lookup_by_output("gone.o").is_some(), "the log was recompacted under the build")?;
    let r = ninja(&dir, &["-d", "explain"], &[]);
    check(r.code == 0 && r.stdout.contains("no work to do"), &format!("the next build: {}{}", r.stdout, r.stderr))
}

/// `fake-wait OUT`: says it runs (`OUT.started`), waits for a `release`
/// file, then writes OUT.
fn fake_wait(args: &[String]) -> i32 {
    let out = &args[0];
    std::fs::write(format!("{out}.started"), "").unwrap();
    if harness::wait_for("release", || Path::new("release").exists()).is_err() {
        return 1;
    }
    std::fs::write(out, "").unwrap();
    0
}

/// A build waiting for the one running in its directory says which one
/// (the lock file holds its pid), and an interrupt ends it at once; the
/// running build goes on.
fn test_interrupt_while_waiting_for_the_lock() -> Result<(), String> {
    let dir = test_dir("interrupt_waiting");
    let wait = tool(&dir, "fake-wait");
    std::fs::write(dir.join("build.ninja"), format!("rule wait\n  command = {wait} $out\nbuild held: wait\nbuild other: wait\n")).unwrap();
    let mut holder = start_ninja(&dir, "holder", &["held"]);
    let result = (|| {
        harness::wait_for("the first build's command", || dir.join("held.started").exists())?;
        #[cfg(unix)]
        let mut waiter = start_ninja(&dir, "waiter", &["other"]);
        #[cfg(windows)]
        let mut waiter = {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            harness::link_self(&dir.join("console-host.exe"));
            let mut c = Command::new(dir.join("console-host.exe"));
            hermetic(&mut c)
                .arg(ninja_exe())
                .args(["--ignore_all_rc_files", "other"])
                .current_dir(&dir)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(std::process::Stdio::piped());
            let out = std::fs::File::create(dir.join("waiter.txt")).unwrap();
            c.stdout(out.try_clone().unwrap()).stderr(out);
            c.spawn().map_err(|e| e.to_string())?
        };
        let said = || std::fs::read_to_string(dir.join("waiter.txt")).unwrap_or_default().replace("\r\n", "\n");
        let waiting = format!("waiting for the build running in this directory (pid {}, started ", holder.id());
        let waited = harness::wait_for("the second build waiting", || said().contains(&waiting) || waiter.try_wait().is_ok_and(|s| s.is_some()));
        let interrupted =
            waited.and_then(|()| check(said().contains(&waiting), &format!("no waiting message naming the first build:\n{}", said()))).and_then(|()| {
                #[cfg(unix)]
                // SAFETY: plain system call.
                unsafe {
                    libc::kill(waiter.id() as i32, libc::SIGINT)
                };
                #[cfg(windows)]
                {
                    use std::io::Write;
                    writeln!(waiter.stdin.take().expect("stdin"), "break").map_err(|e| format!("console-host: {e}"))?;
                }
                Ok(())
            });
        let status = wait_exit(&mut waiter, "the waiting build exiting");
        interrupted?;
        let status = status?;
        #[cfg(windows)]
        if status.code() == Some(NO_CTRL_BREAK) {
            harness::not_tested(&said());
            return Ok(());
        }
        check(status.code() == Some(130), &format!("the waiting build: {status}:\n{}", said()))?;
        check(!said().contains("panicked"), &format!("a panic:\n{}", said()))?;
        check(!dir.join("other.started").exists(), "the waiting build ran its command")
    })();
    std::fs::write(dir.join("release"), "").unwrap();
    let status = wait_exit(&mut holder, "the first build")?;
    result?;
    check(status.success() && dir.join("held").exists(), &format!("the first build: {status}"))
}

/// `-t clean` of some targets forgets the records of outputs of remote
/// actions it removed, and only those.
fn test_clean_keeps_other_remote_records() -> Result<(), String> {
    let dir = test_dir("clean_remote_records");
    std::fs::write(dir.join("build.ninja"), "rule r\n  command = r\nbuild a: r\nbuild b: r\n").unwrap();
    let record = |path: &str| format!("1 {} 5 - {path}\n", "a".repeat(64));
    std::fs::write(dir.join(".ninja_remote"), format!("# ninja-rs remote outputs v2\n{}{}", record("a"), record("b"))).unwrap();
    let r = ninja(&dir, &["-t", "clean", "a"], &[]);
    check(r.code == 0, &format!("-t clean a: {}{}", r.stdout, r.stderr))?;
    let records = std::fs::read_to_string(dir.join(".ninja_remote")).unwrap_or_default();
    check(records.contains(&record("b")) && !records.contains(&record("a")), &format!("the records left:\n{records}"))
}

/// Two manifests share one build directory's logs (CMake's Ninja
/// Multi-Config): one recompacting them keeps the records of the other's
/// outputs, which exist, as ninja does.
fn test_recompaction_keeps_other_manifests_records() -> Result<(), String> {
    let dir = test_dir("multi_config");
    let cl = tool(&dir, "fake-cl");
    for config in ["Debug", "Release"] {
        let mut m = format!("rule gen\n  command = {cl} gen $out\n");
        for i in 1..=3 {
            m += &format!("build {config}/o{i}: gen\n");
        }
        std::fs::write(dir.join(format!("build-{config}.ninja")), m).unwrap();
        let r = ninja(&dir, &["-f", &format!("build-{config}.ninja")], &[]);
        check(r.code == 0, &format!("the {config} build failed: {}{}", r.stdout, r.stderr))?;
    }
    let r = ninja(&dir, &["-f", "build-Debug.ninja", "-t", "recompact"], &[]);
    check(r.code == 0, &format!("-t recompact: {}{}", r.stdout, r.stderr))?;
    let r = ninja(&dir, &["-f", "build-Release.ninja", "-n", "-d", "explain"], &[]);
    check(r.stdout.contains("no work to do"), &format!("the Release build after the Debug recompaction: {}{}", r.stdout, r.stderr))
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

/// Output to a pipe its reader closed: a tool dies of SIGPIPE, as ninja's
/// (a shell says 141), or on Windows goes on without it, as ninja's; a
/// build goes on without it. None panics.
fn test_write_errors() -> Result<(), String> {
    let dir = test_dir("write_errors");
    // More output than a pipe holds.
    let mut manifest = String::from("rule cc\n  command = cc $in\n");
    for i in 0..20000 {
        manifest.push_str(&format!("build o{i}.o: cc s{i}.c\n"));
    }
    std::fs::write(dir.join("build.ninja"), manifest).unwrap();
    let closed = |dir: &Path, args: &[&str]| -> Result<(std::process::ExitStatus, String), String> {
        let mut c = command(dir);
        c.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
        let mut child = c.spawn().map_err(|e| e.to_string())?;
        // Closed before anything is read.
        drop(child.stdout.take());
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        Ok((out.status, String::from_utf8_lossy(&out.stderr).into_owned()))
    };
    let (status, stderr) = closed(&dir, &["-t", "targets", "all"])?;
    check(!stderr.contains("panicked"), &format!("-t targets: {stderr}"))?;
    #[cfg(unix)]
    check(std::os::unix::process::ExitStatusExt::signal(&status) == Some(libc::SIGPIPE), &format!("-t targets: {status}"))?;
    #[cfg(windows)]
    check(status.code() == Some(0), &format!("-t targets: {status}"))?;
    // A build, saying it failed.
    let build = dir.join("build");
    std::fs::create_dir_all(&build).unwrap();
    let fail = if cfg!(windows) { "cmd /c exit 3" } else { "exit 3" };
    std::fs::write(build.join("build.ninja"), format!("rule fail\n  command = {fail}\nbuild out: fail\n")).unwrap();
    let (status, stderr) = closed(&build, &[])?;
    check(status.code() == Some(3) && !stderr.contains("panicked"), &format!("a build: {status}: {stderr}"))
}

/// `--version` and `-t list` are answered from the command line alone, as
/// ninja does: settings files neither change them (`build --verbose`) nor
/// make them fail (CMake runs `--version` to find the generator).
fn test_answers_without_settings() -> Result<(), String> {
    let dir = test_dir("answers");
    let run = |args: &[&str]| {
        let mut c = Command::new(ninja_exe());
        hermetic(&mut c).current_dir(&dir).args(["--nosystem_rc", "--nohome_rc"]).args(args);
        let out = c.output().unwrap();
        Run { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).into(), stderr: String::from_utf8_lossy(&out.stderr).into() }
    };
    std::fs::write(dir.join(".ninjars"), "build --verbose\n").unwrap();
    let r = run(&["--version"]);
    check(r.code == 0 && r.stdout.lines().count() == 1, &format!("--version: {}{}", r.stdout, r.stderr))?;
    std::fs::write(dir.join(".ninjars"), "build --nope\n").unwrap();
    for args in [&["--version"][..], &["-t", "list"]] {
        let r = run(args);
        check(r.code == 0 && r.stderr.is_empty(), &format!("{args:?}: exit {}: {}", r.code, r.stderr))?;
    }
    let r = run(&["-t", "cleen"]);
    check_eq(&r.stderr.replace("\r\n", "\n"), "ninja: fatal: unknown tool 'cleen', did you mean 'clean'?\n", "-t cleen")?;
    check(r.code == 1, &format!("-t cleen: exit {}", r.code))
}

/// `-t targets depth` without a number keeps the default depth (1), as
/// ninja's; one that isn't a number is 0: the whole tree.
fn test_targets_depth() -> Result<(), String> {
    let dir = tools_fixture("targets_depth");
    let default = ninja(&dir, &["-t", "targets"], &[]).stdout;
    check_eq(&default, "app: link\n", "-t targets")?;
    check_eq(&ninja(&dir, &["-t", "targets", "depth"], &[]).stdout, &default, "-t targets depth")?;
    let whole = ninja(&dir, &["-t", "targets", "depth", "0"], &[]).stdout;
    check(whole.lines().count() > 1, &format!("-t targets depth 0:\n{whole}"))?;
    check_eq(&ninja(&dir, &["-t", "targets", "depth", "x"], &[]).stdout, &whole, "-t targets depth x")
}

/// A failed command exiting with 130 by itself doesn't make the build
/// events say the user interrupted the build.
fn test_command_exiting_130() -> Result<(), String> {
    let dir = test_dir("exit_130");
    let fail = if cfg!(windows) { "cmd /c exit 130" } else { "exit 130" };
    std::fs::write(dir.join("build.ninja"), format!("rule fail\n  command = {fail}\nbuild out: fail\n")).unwrap();
    let r = ninja(&dir, &["--build_event_binary_file=events.bin"], &[]);
    check(r.code == 130, &format!("exit {}: {}{}", r.code, r.stdout, r.stderr))?;
    let events = std::fs::read(dir.join("events.bin")).unwrap_or_default();
    let has = |what: &[u8]| events.windows(what.len()).any(|w| w == what);
    check(has(b"BUILD_FAILURE") && !has(b"INTERRUPTED"), "the build events say the build was interrupted")
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

/// A directory only the user can enter in `dir`: the daemon's, where the
/// secret flags file for nested builds goes, and their events' relay.
fn private_dir(dir: &Path) -> PathBuf {
    let private = dir.join("private");
    std::fs::create_dir(&private).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&private, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    private
}

/// ninja-rs in `dir` (see `command`), started so that `interrupt` reaches
/// it: on Windows, on a console of its own (see `console_host`).
fn interruptible(dir: &Path) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let host = dir.join("console-host.exe");
        harness::link_self(&host);
        let mut c = Command::new(host);
        hermetic(&mut c).current_dir(dir).arg(ninja_exe()).arg("--ignore_all_rc_files");
        c.stdin(std::process::Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
        c
    }
    #[cfg(unix)]
    command(dir)
}

/// Interrupts ninja-rs started by `interruptible`: SIGTERM, or Ctrl-Break.
fn interrupt(child: &mut std::process::Child) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::io::Write;
        writeln!(child.stdin.take().expect("stdin"), "break").map_err(|e| format!("console-host: {e}"))
    }
    #[cfg(unix)]
    {
        // SAFETY: plain system call.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        Ok(())
    }
}

/// An interrupt outside a build's loop (here while connecting to a remote
/// executor that never answers) ends the invocation at once, as ninja dies
/// of the signal there (it used to go on until the connection timed out),
/// once its build events end (they were left empty); without a panic, and
/// leaving nothing behind (its secret flags file, its relay for nested
/// builds' events).
fn test_interrupt_outside_the_build() -> Result<(), String> {
    let dir = test_dir("interrupt_outside");
    std::fs::write(dir.join("build.ninja"), "rbe_version = 1\nrule r\n  command = touch $out\nbuild out: r\n").unwrap();
    let private = private_dir(&dir);
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
    let mut c = interruptible(&dir);
    c.args([format!("--remote_executor=grpc://127.0.0.1:{port}").as_str(), "--remote_header=x=y", "--build_event_binary_file=events.bin"]);
    c.env("NINJA_RS_DAEMON_DIR", &private);
    c.stdout(std::fs::File::create(dir.join("stdout.txt")).unwrap()).stderr(std::fs::File::create(dir.join("stderr.txt")).unwrap());
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    if connecting.recv_timeout(harness::PATIENCE).is_err() {
        let _ = child.kill();
        return Err("ninja-rs didn't connect".into());
    }
    let interrupted = interrupt(&mut child);
    let exited = harness::wait_for("ninja-rs exiting", || child.try_wait().is_ok_and(|s| s.is_some()));
    if exited.is_err() || interrupted.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
    let text = format!("{}{}", read("stdout.txt"), read("stderr.txt"));
    interrupted.map_err(|e| format!("{e}\n{text}"))?;
    #[cfg(windows)]
    if status.code() == Some(NO_CTRL_BREAK) {
        harness::not_tested(text.trim());
        return Ok(());
    }
    exited.map_err(|e| format!("{e}\n{text}"))?;
    // 128 + SIGTERM, or SIGINT (Ctrl-Break, as Ctrl-C).
    let code = if cfg!(windows) { 130 } else { 143 };
    check(status.code() == Some(code), &format!("exited with {status}\n{text}"))?;
    check(!text.contains("panicked"), &format!("a panic:\n{text}"))?;
    let events = std::fs::read(dir.join("events.bin")).unwrap_or_default();
    check(events.windows(b"INTERRUPTED".len()).any(|w| w == b"INTERRUPTED"), &format!("the build events didn't end:\n{text}"))?;
    let left: Vec<String> = std::fs::read_dir(&private).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    check(left.is_empty(), &format!("left behind: {left:?}"))
}

/// `-t exec` passes SIGTERM on to its command (a terminal never sends it to
/// the command too), and exits with the command's code.
#[cfg(unix)]
fn test_exec_forwards_signals() -> Result<(), String> {
    let dir = test_dir("exec_signals");
    let script = "trap 'echo > got-term; exit 7' TERM; echo > ready; while :; do sleep 0.1; done";
    let mut c = command(&dir);
    c.args(["-t", "exec", "sh", "-c", script]);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    harness::wait_for("the command running", || dir.join("ready").exists() || child.try_wait().is_ok_and(|s| s.is_some()))?;
    // SAFETY: plain system call.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    match wait_at_most(&mut child, harness::PATIENCE) {
        Some(s) => check(s.code() == Some(7) && dir.join("got-term").exists(), &format!("exited with {s}")),
        None => Err("still running long after SIGTERM".into()),
    }
}

/// `-t exec` passes on SIGINT when a process sent it to ninja-rs alone
/// (kill(2)), even in the terminal's foreground: the terminal's own reaches
/// the command too (and isn't passed on), a process's doesn't.
#[cfg(target_os = "linux")]
fn test_exec_forwards_sigint_from_processes() -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let dir = test_dir("exec_sigint");
    let (_master, terminal) = pty(80)?;
    let script = "trap 'echo > got-int; exit 7' INT; echo > ready; while :; do sleep 0.1; done";
    let mut c = command(&dir);
    c.args(["-t", "exec", "sh", "-c", script]).stdin(std::process::Stdio::null());
    let tty = terminal.as_raw_fd();
    // In the foreground of a terminal of its own, as its command.
    // SAFETY: async-signal-safe calls only, between fork and exec.
    unsafe {
        c.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(tty, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    harness::wait_for("the command running", || dir.join("ready").exists() || child.try_wait().is_ok_and(|s| s.is_some()))?;
    // SAFETY: plain system call.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    match wait_at_most(&mut child, harness::PATIENCE) {
        Some(s) => check(s.code() == Some(7) && dir.join("got-int").exists(), &format!("exited with {s}")),
        None => Err("the command didn't get SIGINT".into()),
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

/// A build with the daemon, its output read through pipes (an IDE, `| tee`,
/// CI): they end with the build, not with the daemon it started, which
/// must not hold them (on Windows it inherited them, and their reader
/// waited until it exited, hours later).
fn test_daemon_pipes() -> Result<(), String> {
    use std::io::Read;
    let dir = test_dir("daemon_pipes");
    std::fs::write(dir.join("build.ninja"), "rbe_version = 1\nbuild all: phony\n").unwrap();
    let daemon_dir = dir.join("daemon");
    let env = [("NINJA_RS_DAEMON_DIR", daemon_dir.to_str().unwrap()), ("NINJA_RS_DAEMON_IDLE", "600")];
    let mut c = Command::new(ninja_exe());
    c.current_dir(&dir).args(["--ignore_all_rc_files", "--daemon", "--disk_cache=cache"]).envs(env);
    c.env_remove("MAKEFLAGS").env_remove("NINJA_STATUS").env_remove(ninja_rs::nested::ENV);
    let mut child = c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let (tx, rx) = std::sync::mpsc::channel();
    let pipes: [Box<dyn Read + Send>; 2] = [Box::new(child.stdout.take().unwrap()), Box::new(child.stderr.take().unwrap())];
    for mut pipe in pipes {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = pipe.read_to_end(&mut text);
            let _ = tx.send(String::from_utf8_lossy(&text).into_owned());
        });
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let ended: Result<Vec<String>, _> = (0..2).map(|_| rx.recv_timeout(harness::PATIENCE)).collect();
    let running = ninja(&dir, &["--ignore_all_rc_files", "-t", "daemon", "status"], &env);
    ninja(&dir, &["--ignore_all_rc_files", "-t", "daemon", "stop"], &env);
    let output = ended.map_err(|_| format!("the build's pipes were still open {:?} after it exited: the daemon holds them", harness::PATIENCE))?;
    check(status.success(), &format!("the build failed: {}", output.concat()))?;
    check(running.stdout.contains("pid"), &format!("the build started no daemon: {}{}", running.stdout, running.stderr))
}

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

/// Failed commands are named again before "build stopped" when other output
/// followed them (ninja issue 2378), not when the only one is still last.
/// Their `FAILED:` blocks aren't repeated: IDEs would count the errors twice.
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
    let summary = "ninja: 2 commands failed:\n  a.o\n  c.o\n";
    check(
        r.stdout.matches("FAILED: ").count() == 2 && r.stdout.lines().filter(|l| *l == "a.o: error").count() == 1,
        &format!("errors repeated:\n{}", r.stdout),
    )?;
    let at = r.stdout.find(summary).ok_or_else(|| {
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

/// `NINJA_STATUS` as in ninja 1.13: `%P` predicts from the time the finished
/// commands took and the build log's times of the others, and an unknown
/// placeholder is fatal.
fn test_ninja_status() -> Result<(), String> {
    let dir = jobs_fixture("ninja_status", 2, "");
    let env = [("NINJA_STATUS", "[%P] ")];
    let predicted = |out: &str| -> Vec<String> { out.lines().filter_map(|l| Some(l.strip_prefix('[')?.split_once(']')?.0.to_string())).collect() };
    // Without a build log, the second command takes as long as the first.
    let r = ninja(&dir, &["-j1"], &env);
    check(r.code == 0 && predicted(&r.stdout) == [" 50%", "100%"], &format!("without a build log:\n{}", r.stdout))?;
    // The build log says they take 1000 s.
    let log = std::fs::read_to_string(dir.join(".ninja_rs_log")).unwrap();
    let log: String = log
        .lines()
        .map(|line| match line.split('\t').collect::<Vec<_>>().as_slice() {
            [start, _, rest @ ..] if rest.len() == 3 => format!("{start}\t{}\t{}\n", start.parse::<i64>().unwrap() + 1_000_000, rest.join("\t")),
            _ => format!("{line}\n"),
        })
        .collect();
    std::fs::write(dir.join(".ninja_rs_log"), log).unwrap();
    for out in ["j1", "j2"] {
        std::fs::remove_file(dir.join(out)).unwrap();
    }
    let r = ninja(&dir, &["-j1"], &env);
    check(r.code == 0 && predicted(&r.stdout) == ["  0%", "100%"], &format!("with a build log:\n{}", r.stdout))?;
    let r = ninja(&dir, &["-n"], &[("NINJA_STATUS", "[%f/%x] ")]);
    check(r.code == 1 && r.stderr.contains("ninja: fatal: unknown placeholder '%x' in $NINJA_STATUS"), &format!("an unknown placeholder: {}", r.stderr))
}

/// A new pseudo-terminal `columns` wide: its master, and the terminal.
#[cfg(unix)]
fn pty(columns: u16) -> Result<(std::fs::File, std::fs::File), String> {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // SAFETY: a new pseudo-terminal, its name copied at once. Its master
    // isn't inherited: the terminal hangs up once the test closes it.
    let (master, name) = unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if fd < 0 || libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) != 0 || libc::grantpt(fd) != 0 || libc::unlockpt(fd) != 0 {
            return Err(format!("no pseudo-terminal: {}", std::io::Error::last_os_error()));
        }
        let size = libc::winsize { ws_row: 24, ws_col: columns, ws_xpixel: 0, ws_ypixel: 0 };
        libc::ioctl(fd, libc::TIOCSWINSZ, &size);
        (std::fs::File::from_raw_fd(fd), std::ffi::CStr::from_ptr(libc::ptsname(fd)).to_string_lossy().into_owned())
    };
    let terminal = std::fs::File::options().read(true).write(true).custom_flags(libc::O_NOCTTY).open(name).map_err(|e| e.to_string())?;
    Ok((master, terminal))
}

/// On a terminal, as ninja: the status line as wide as the terminal says
/// (not `COLUMNS`), the rest of the line erased; without `TERM`, plain lines.
#[cfg(unix)]
fn test_terminal_status() -> Result<(), String> {
    use std::io::Read;
    let dir = test_dir("terminal_status");
    std::fs::write(dir.join("build.ninja"), format!("rule r\n  command = true\n  description = {}\nbuild a: r\n", "x".repeat(100))).unwrap();
    // What ninja-rs writes to a terminal 40 columns wide.
    let run = |term: Option<&str>| -> Result<String, String> {
        let (mut master, terminal) = pty(40)?;
        let mut c = command(&dir);
        c.stdout(terminal).env("COLUMNS", "100").env_remove("NO_COLOR");
        match term {
            Some(t) => c.env("TERM", t),
            None => c.env_remove("TERM"),
        };
        let status = c.status().map_err(|e| e.to_string())?;
        // The terminal closed (the end of the output, then EIO).
        drop(c);
        let mut out = Vec::new();
        let _ = master.read_to_end(&mut out);
        check(status.success(), &format!("the build failed: {}", String::from_utf8_lossy(&out)))?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    };
    let line = |finished: usize| format!("\r[{finished}/1] {}...{}\x1b[K", "x".repeat(12), "x".repeat(19));
    check_eq(&run(Some("xterm"))?, &format!("{}{}\r\n", line(0), line(1)), "TERM=xterm")?;
    check_eq(&run(None)?, &format!("[1/1] {}\r\n", "x".repeat(100)), "without TERM")
}

/// `console-screen NINJA...` runs ninja-rs on its console, escape sequences
/// off as a new console starts, then prints whether they are on, the
/// console's width, and what its screen shows.
#[cfg(windows)]
fn console_screen(args: &[String]) -> i32 {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Info {
        size: Coord,
        cursor: Coord,
        attributes: u16,
        window: [i16; 4],
        maximum_window_size: Coord,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
        fn GetConsoleScreenBufferInfo(console: *mut c_void, info: *mut Info) -> i32;
        fn ReadConsoleOutputCharacterW(console: *mut c_void, chars: *mut u16, len: u32, at: Coord, read: *mut u32) -> i32;
    }
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;
    let console = match std::fs::File::options().read(true).write(true).open("CONOUT$") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("console-screen: {e}");
            return 1;
        }
    };
    let handle = console.as_raw_handle();
    let mut mode = 0;
    // SAFETY: plain system calls on the console.
    unsafe {
        GetConsoleMode(handle, &mut mode);
        SetConsoleMode(handle, mode & !ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
    let (Ok(stdout), Ok(stderr)) = (console.try_clone(), console.try_clone()) else { return 1 };
    let code = Command::new(&args[0]).args(&args[1..]).stdout(stdout).stderr(stderr).status().map_or(1, |s| s.code().unwrap_or(1));
    let mut info = Info::default();
    // SAFETY: as above.
    unsafe {
        GetConsoleMode(handle, &mut mode);
        GetConsoleScreenBufferInfo(handle, &mut info);
    }
    println!("escapes {} width {}", mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0, info.size.x);
    for y in 0..=info.cursor.y {
        let mut row = vec![0u16; info.size.x.max(0) as usize];
        let mut read = 0;
        // SAFETY: reads a row at most, into `row`.
        unsafe { ReadConsoleOutputCharacterW(handle, row.as_mut_ptr(), row.len() as u32, Coord { x: 0, y }, &mut read) };
        println!("{}", String::from_utf16_lossy(&row[..read as usize]).trim_end());
    }
    code
}

/// On a Windows console, as ninja: escape sequences turned on, so colors
/// show as such, and the status line as wide as the console.
#[cfg(windows)]
fn test_console_status() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let dir = test_dir("console_status");
    let host = dir.join("console-screen.exe");
    harness::link_self(&host);
    std::fs::write(dir.join("build.ninja"), format!("rule fail\n  command = cmd /c exit 1\n  description = {}\nbuild a: fail\n", "x".repeat(300))).unwrap();
    let mut c = Command::new(host);
    hermetic(&mut c).arg(ninja_exe()).arg("--ignore_all_rc_files").current_dir(&dir).creation_flags(CREATE_NO_WINDOW);
    for var in ["TERM", "NO_COLOR", "CLICOLOR_FORCE", "FORCE_COLOR"] {
        c.env_remove(var);
    }
    let out = c.output().map_err(|e| e.to_string())?;
    let screen = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let width = screen.lines().next().and_then(|l| l.strip_prefix("escapes true width ")?.parse::<usize>().ok());
    let width = width.ok_or_else(|| format!("escape sequences not turned on:\n{screen}"))?;
    check(!screen.contains('\x1b') && screen.contains("\nFAILED: a\ncmd /c exit 1\n"), &format!("escape sequences shown:\n{screen}"))?;
    // Filling the last column leaves the cursor there, and erasing the rest
    // of the line erases it (with ninja too).
    let status = screen.lines().find(|l| l.starts_with("[1/1] x")).unwrap_or_default();
    check(status.contains("...") && (width - 1..=width).contains(&status.chars().count()), &format!("the status line isn't {width} wide:\n{screen}"))
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
/// and leave nothing behind (its secret flags file included), without a
/// panic. Its output goes to `stdout` (a terminal) when given. An interrupt
/// that can't be sent makes the test not tested.
fn interrupted_build(
    name: &str,
    launch: impl FnOnce(&Path) -> Command,
    stdout: Option<std::fs::File>,
    interrupt: impl FnOnce(&mut std::process::Child) -> Result<(), String>,
) -> Result<(), String> {
    let dir = test_dir(name);
    harness::link_self(&dir.join(if cfg!(windows) { "fake-sleep.exe" } else { "fake-sleep" }));
    let command = if cfg!(windows) { "cmd /c cmd /c \".\\fake-sleep $out\"" } else { "sh -c 'sh -c \"./fake-sleep $out\"; true'" };
    std::fs::write(dir.join("build.ninja"), format!("rule slow\n  command = {command}\nbuild a: slow\nbuild b: slow\n")).unwrap();
    let private = private_dir(&dir);
    let mut c = launch(&dir);
    c.current_dir(&dir).args(["-j2", "--remote_header=x=y", "--build_event_binary_file=events.bin"]);
    c.env("NINJA_RS_DAEMON_DIR", &private);
    // Files, not pipes: nothing blocks on them while the test waits.
    let captured = stdout.is_none();
    c.stdout(stdout.unwrap_or_else(|| std::fs::File::create(dir.join("stdout.txt")).unwrap()));
    c.stderr(std::fs::File::create(dir.join("stderr.txt")).unwrap());
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
    check(!captured || text.contains("ninja: build stopped: interrupted by user.\n"), &format!("{name}:\n{text}"))?;
    check(!text.contains("panicked"), &format!("{name}: a panic:\n{text}"))?;
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
        interrupted_build(name, command, None, |child| {
            // SAFETY: plain system call.
            unsafe { libc::kill(child.id() as i32, signal) };
            Ok(())
        })?;
    }
    Ok(())
}

/// A terminal hanging up during a build: SIGHUP interrupts it, as in
/// ninja, and the output it can't write anymore (EIO) doesn't end it
/// otherwise (a panic, which left its build events unfinished).
#[cfg(unix)]
fn test_hangup_during_build() -> Result<(), String> {
    let (master, terminal) = pty(80)?;
    let master = std::cell::RefCell::new(Some(master));
    interrupted_build("hangup", command, Some(terminal), |child| {
        // The terminal goes: writing to it fails from now on.
        drop(master.borrow_mut().take());
        // SAFETY: plain system call.
        unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };
        Ok(())
    })
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
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
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
    interrupted_build("ctrl-break", interruptible, None, interrupt)
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
    if stem.starts_with("fake-gen") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_gen(&args) as u8);
    }
    if stem.starts_with("fake-wait") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_wait(&args) as u8);
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
    #[cfg(windows)]
    if stem.starts_with("console-screen") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(console_screen(&args) as u8);
    }
    use harness::Test;
    #[allow(unused_mut)]
    let mut tests: Vec<Test> = vec![
        ("rules", test_rules),
        ("daemon_pipes", test_daemon_pipes),
        ("wincodepage", test_wincodepage),
        ("inputs", test_inputs),
        ("compdb", test_compdb),
        ("missingdeps", test_missingdeps),
        ("status_format", test_status_format),
        ("failure_summary", test_failure_summary),
        ("ninja_status", test_ninja_status),
        ("jobserver_pool", test_jobserver_pool),
        ("jobserver_pool_from_manifest", test_jobserver_pool_from_manifest),
        ("jobserver_client", test_jobserver_client),
        ("ninjars", test_ninjars),
        ("build_event_digests", test_build_event_digests),
        ("concurrent_builds", test_concurrent_builds),
        ("nested_build_appends_only", test_nested_build_appends_only),
        ("cross_nested_builds", test_cross_nested_builds),
        ("generator_under_the_lock", test_generator_under_the_lock),
        ("interrupt_while_waiting_for_the_lock", test_interrupt_while_waiting_for_the_lock),
        ("clean_keeps_other_remote_records", test_clean_keeps_other_remote_records),
        ("recompaction_keeps_other_manifests_records", test_recompaction_keeps_other_manifests_records),
        ("exit_codes", test_exit_codes),
        ("write_errors", test_write_errors),
        ("answers_without_settings", test_answers_without_settings),
        ("targets_depth", test_targets_depth),
        ("command_exiting_130", test_command_exiting_130),
        ("interrupt_outside_the_build", test_interrupt_outside_the_build),
    ];
    #[cfg(unix)]
    tests.push(("jobserver_pipe", test_jobserver_pipe));
    #[cfg(unix)]
    tests.push(("terminal_status", test_terminal_status));
    #[cfg(unix)]
    tests.push(("interrupt_signals", test_interrupt_signals));
    #[cfg(unix)]
    tests.push(("hangup_during_build", test_hangup_during_build));
    #[cfg(unix)]
    tests.push(("exec_forwards_signals", test_exec_forwards_signals));
    #[cfg(target_os = "linux")]
    tests.push(("exec_forwards_sigint_from_processes", test_exec_forwards_sigint_from_processes));
    #[cfg(windows)]
    tests.push(("msvc_deps_relative", test_msvc_deps_relative));
    #[cfg(windows)]
    tests.push(("interrupt_ctrl_break", test_interrupt_ctrl_break));
    #[cfg(windows)]
    tests.push(("console_status", test_console_status));
    harness::run("cli", tests)
}
