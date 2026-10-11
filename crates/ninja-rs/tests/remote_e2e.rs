//! End-to-end test of remote execution against the in-process fake REAPI
//! server, with a hand-written build.ninja following the RBE interface.
//!
//! The "compiler" is this test binary itself: when it runs under the name
//! `fake-clang` it behaves like a tiny C compiler (follows `#include "..."`,
//! writes a depfile, answers `-E -v` with an include search list). Under the
//! name `fake-cmake` it deletes and touches files (`-E rm -f`, `-E touch`),
//! a tool declared local. Under the name `fake-ar` it writes thin archives
//! (`qcT`), which `fake-clang --link` reads. This keeps the test hermetic on
//! every host.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fake_reapi::FakeServer;
use ninja_core::subprocess::Interrupt;
use ninja_rs::Service;
use ninja_rs::options::{ParseOutcome, parse_args};

// ---------------------------------------------------------------------------
// fake-clang
// ---------------------------------------------------------------------------

fn fake_clang(args: &[String]) -> i32 {
    let mut include_dirs: Vec<PathBuf> = Vec::new();
    let mut src = None;
    let mut out = None;
    let mut depfile = None;
    let mut link = false;
    let mut inputs = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        match a.as_str() {
            "-v" => {
                eprintln!("fake-clang version 1\n#include \"...\" search starts here:\n#include <...> search starts here:\nEnd of search list.");
                return 0;
            }
            "-E" | "-MD" | "-x" => {
                if a == "-x" {
                    i += 1;
                }
            }
            "-c" => {
                src = args.get(i).cloned();
                i += 1;
            }
            "-o" => {
                out = args.get(i).cloned();
                i += 1;
            }
            "-MF" => {
                depfile = args.get(i).cloned();
                i += 1;
            }
            "--link" => link = true,
            "-DFAIL" => {
                eprintln!("error: FAIL requested");
                return 1;
            }
            _ if a.starts_with("-I") => include_dirs.push(PathBuf::from(&a[2..])),
            _ => inputs.push(a.clone()),
        }
    }
    let Some(out) = out else {
        eprintln!("fake-clang: no -o");
        return 2;
    };
    if link {
        let mut data = String::from("LINKED\n");
        for i in &inputs {
            let bytes = std::fs::read(i).unwrap_or_default();
            match ninja_remote::archive::members(&bytes) {
                // A thin archive: its members, next to it.
                Ok(Some(members)) => {
                    for m in members {
                        let p = Path::new(i).parent().unwrap().join(&m);
                        match std::fs::read_to_string(&p) {
                            Ok(s) => data.push_str(&s),
                            Err(e) => {
                                eprintln!("fake-clang: thin archive {i}: member {m}: {e}");
                                return 1;
                            }
                        }
                    }
                }
                _ => data.push_str(&String::from_utf8_lossy(&bytes)),
            }
        }
        std::fs::write(&out, data).unwrap();
        return 0;
    }
    let src = src.expect("-c");
    // Racing tests: files in the working directory, which only local runs
    // see (remote ones run elsewhere), make them slow, or mark their outputs.
    if let Ok(ms) = std::fs::read_to_string("race-local-sleep-ms") {
        std::thread::sleep(std::time::Duration::from_millis(ms.trim().parse().unwrap_or(0)));
    }
    let mut deps = vec![src.clone()];
    let mut text = String::new();
    fn expand(file: &Path, dirs: &[PathBuf], deps: &mut Vec<String>, text: &mut String) -> Result<(), String> {
        let content = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        for line in content.lines() {
            if let Some(rest) = line.trim().strip_prefix("#include \"") {
                let name = rest.trim_end_matches('"');
                let mut cands = vec![file.parent().unwrap().join(name)];
                cands.extend(dirs.iter().map(|d| d.join(name)));
                let found = cands.into_iter().find(|c| c.is_file()).ok_or_else(|| format!("{}: '{name}' file not found", file.display()))?;
                let s = found.to_string_lossy().replace('\\', "/");
                if !deps.contains(&s) {
                    deps.push(s);
                }
                expand(&found, dirs, deps, text)?;
            } else {
                text.push_str(line);
                text.push('\n');
            }
        }
        Ok(())
    }
    if let Err(e) = expand(Path::new(&src), &include_dirs, &mut deps, &mut text) {
        eprintln!("fake-clang: error: {e}");
        return 1;
    }
    if let Some(d) = depfile {
        let body: Vec<String> = deps.iter().map(|d| d.replace(' ', "\\ ")).collect();
        std::fs::write(&d, format!("{out}: {}\n", body.join(" "))).unwrap();
    }
    if Path::new("race-local-mark").exists() {
        text.push_str("RACE-LOCAL\n");
    }
    std::fs::write(&out, text).unwrap();
    println!("fake-clang: compiled {src}");
    0
}

/// `fake-ar qcT <archive> <members...>`: a GNU thin archive, with member
/// paths relative to the archive's directory and a large symbol table (so
/// that reading the members skips most of the archive).
fn fake_ar(args: &[String]) -> i32 {
    let [op, out, members @ ..] = args else {
        eprintln!("fake-ar: usage: qcT <archive> <members...>");
        return 2;
    };
    if op != "qcT" {
        eprintln!("fake-ar: unsupported operation {op}");
        return 2;
    }
    let comps = |p: &str| -> Vec<String> { p.replace('\\', "/").split('/').filter(|c| !c.is_empty() && *c != ".").map(str::to_string).collect() };
    let mut dir = comps(out);
    dir.pop();
    let header = |name: &str, size: usize| format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644).into_bytes();
    let mut names = String::new();
    let mut refs = Vec::new();
    for m in members {
        let size = std::fs::metadata(m).map(|md| md.len() as usize).unwrap_or(0);
        let m = comps(m);
        let common = dir.iter().zip(&m).take_while(|(a, b)| a == b).count();
        let mut rel: Vec<String> = vec!["..".to_string(); dir.len() - common];
        rel.extend(m[common..].iter().cloned());
        refs.push((format!("/{}", names.len()), size));
        names.push_str(&format!("{}/\n", rel.join("/")));
    }
    let symbols: Vec<u8> = b"SYMBOLS!".iter().copied().cycle().take(100_000).collect();
    let mut data = b"!<thin>\n".to_vec();
    data.extend(header("/", symbols.len()));
    data.extend(&symbols);
    data.extend(header("//", names.len()));
    data.extend(names.as_bytes());
    if names.len() % 2 == 1 {
        data.push(b'\n');
    }
    for (name, size) in refs {
        data.extend(header(&name, size));
    }
    std::fs::write(out, data).unwrap();
    0
}

fn fake_cmake(args: &[String]) -> i32 {
    match args {
        [e, rm, f, files @ ..] if e == "-E" && rm == "rm" && f == "-f" => {
            for p in files {
                let _ = std::fs::remove_file(p);
            }
            0
        }
        [e, copy, from, to] if e == "-E" && copy == "copy" => match std::fs::copy(from, to) {
            Ok(_) => 0,
            Err(err) => {
                eprintln!("fake-cmake: copying {from}: {err}");
                1
            }
        },
        [e, touch, files @ ..] if e == "-E" && touch == "touch" => {
            for p in files {
                std::fs::write(p, "").unwrap();
            }
            0
        }
        _ => {
            eprintln!("fake-cmake: unsupported arguments {args:?}");
            2
        }
    }
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct Fixture {
    root: PathBuf,
    build: PathBuf,
    compiler: PathBuf,
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn fixture(name: &str, rbe: bool) -> Fixture {
    let base = std::env::var_os("TEST_TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let root = base.join(format!("ninja-rs-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let build = root.join("build");
    let exe = std::env::current_exe().unwrap();
    let compiler = root.join("tc/bin").join(if cfg!(windows) { "fake-clang.exe" } else { "fake-clang" });
    std::fs::create_dir_all(compiler.parent().unwrap()).unwrap();
    std::fs::copy(&exe, &compiler).unwrap();
    let ar = root.join("tc/bin").join(if cfg!(windows) { "fake-ar.exe" } else { "fake-ar" });
    std::fs::copy(&exe, &ar).unwrap();
    let cmake = root.join("cmake/bin").join(if cfg!(windows) { "fake-cmake.exe" } else { "fake-cmake" });
    std::fs::create_dir_all(cmake.parent().unwrap()).unwrap();
    std::fs::copy(&exe, &cmake).unwrap();
    write(&root.join("src/main.c"), "#include \"lib.h\"\nint main() { return lib(); }\n");
    write(&root.join("src/lib.c"), "#include \"lib.h\"\nint lib() { return DEEP; }\n");
    write(&root.join("include/lib.h"), "#include \"nested/deep.h\"\nint lib();\n");
    write(&root.join("include/nested/deep.h"), "#define DEEP 0\n");
    write(&root.join("src/big.c"), "int big;\n");
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let tc_json = format!(
        "{{\"version\": 1, \"toolchains\": [{{\"language\": \"C\", \"tool\": \"{}\", \"inputs\": [\"{}\"], \"platform\": {{\"OSFamily\": \"{os}\"}}}}, \
          {{\"tool\": \"{}\", \"inputs\": [\"{}\"], \"platform\": {{\"OSFamily\": \"{os}\"}}}}, {{\"tool\": \"{}\", \"local\": true}}]}}",
        fwd(&compiler),
        fwd(&root.join("tc")),
        fwd(&ar),
        fwd(&root.join("tc")),
        fwd(&cmake),
        os = if cfg!(windows) { "Windows" } else { "Linux" },
    );
    write(&build.join("CMakeFiles/rbe_toolchains.json"), &tc_json);
    write(
        &build.join("CMakeFiles/rbe_targets.json"),
        "{\"version\": 1, \"configurations\": [{\"name\": \"Release\", \"mnemonic\": \"x86_64-test-Release\", \"variables\": {\"CMAKE_BUILD_TYPE\": \"Release\"}}], \
          \"targets\": [{\"name\": \"app\", \"kind\": \"executable\", \"label\": \"//src:app\"}, {\"name\": \"lib\", \"kind\": \"static_library\", \"label\": \"//src:lib\"}]}",
    );
    let cc = compiler.to_string_lossy().to_string();
    let cm = cmake.to_string_lossy().to_string();
    let thin_ar = ar.to_string_lossy().to_string();
    let inc = root.join("include").to_string_lossy().to_string();
    let header = if rbe { "rbe_version = 1\nrbe_toolchains = CMakeFiles/rbe_toolchains.json\nrbe_targets = CMakeFiles/rbe_targets.json\n" } else { "" };
    // Chains need a shell: cmd.exe on Windows (ninja runs commands directly
    // there), /bin/sh elsewhere (ninja's default).
    let chain = |steps: &[String]| {
        if cfg!(windows) { format!("cmd.exe /C \"{}\"", steps.join(" && ")) } else { steps.join(" && ") }
    };
    let ar = chain(&[format!("{cm} -E rm -f $out pre.flag"), format!("{cc} --link $in -o $out"), format!("{cm} -E touch ar.stamp")]);
    let ar2 = chain(&[format!("{cc} --link $in -o lib2.a"), format!("{cc} --link lib2.a -o lib2.copy")]);
    // A local tool between two compiler steps: can't be sent remotely.
    let sandwich = chain(&[format!("{cc} -I{inc} -c $in -o $out"), format!("{cm} -E touch sandwich.flag"), format!("{cc} -I{inc} -c $in -o $out")]);
    let manifest = format!(
        "{header}\
rule CC\n  command = {cc} -I{inc} -MD -MF $DEP_FILE $FLAGS -c $in -o $out\n  depfile = $DEP_FILE\n  deps = gcc\n  description = CC $out\n\
rule LINK\n  command = {cc} --link $in -o $out\n  description = LINK $out\n\
rule AR\n  command = {ar}\n  description = AR $out\n\
rule TOUCH\n  command = {cm} -E touch $out\n\
rule COPY\n  command = {cm} -E copy $in $out\n\
rule AR2\n  command = {ar2}\n\
rule SANDWICH\n  command = {sandwich}\n\
rule THINAR\n  command = {thin_ar} qcT $out $in\n\
build sandwich.o: SANDWICH {lib}\n  rbe_mnemonic = CCompile\n\
build obj/main.o: CC {main}\n  DEP_FILE = obj/main.o.d\n  rbe_target = app\n  rbe_mnemonic = CCompile\n\
build obj/lib.o: CC {lib}\n  DEP_FILE = obj/lib.o.d\n  rbe_target = app\n  rbe_mnemonic = CCompile\n\
build obj/local.o: CC {lib}\n  DEP_FILE = obj/local.o.d\n  rbe_target = app\n  rbe_mnemonic = CCompile\n  rbe_local = 1\n\
build app.bin: LINK obj/main.o obj/lib.o obj/local.o\n  rbe_target = app\n  rbe_mnemonic = CLink\n  rbe_artifact = 1\n  rbe_deliverable = 1\n\
build obj/bad.o: CC {lib}\n  DEP_FILE = obj/bad.o.d\n  FLAGS = -DFAIL\n  rbe_mnemonic = CCompile\n\
build bad: phony obj/bad.o\n\
build lib.a: AR obj/main.o obj/lib.o\n  rbe_target = lib\n  rbe_mnemonic = CArchive\n  rbe_artifact = 1\n\
build helper.stamp: TOUCH\n  rbe_mnemonic = CArchive\n\
build copy.o: COPY obj/lib.o\n\
build lib2.a lib2.copy: AR2 obj/main.o\n  rbe_mnemonic = CArchive\n\
build obj/broken.o: CC {lib}\n  DEP_FILE = obj/broken.o.d\n  FLAGS = -DFAIL\n  rbe_target = broken\n  rbe_mnemonic = CCompile\n\
build user.bin: LINK obj/broken.o\n  rbe_target = user\n  rbe_mnemonic = CLink\n  rbe_deliverable = 1\n\
build obj/big.o: CC {big}\n  DEP_FILE = obj/big.o.d\n  rbe_mnemonic = CCompile\n\
build lib/libthin.a: THINAR obj/main.o obj/lib.o\n  rbe_mnemonic = CArchive\n\
build thin.bin: LINK lib/libthin.a\n  rbe_mnemonic = CLink\n  rbe_deliverable = 1\n\
default app.bin\n",
        main = ninja_escape_path(&root.join("src/main.c").to_string_lossy()),
        lib = ninja_escape_path(&root.join("src/lib.c").to_string_lossy()),
        big = ninja_escape_path(&root.join("src/big.c").to_string_lossy()),
    );
    write(&build.join("build.ninja"), &manifest);
    Fixture { root, build, compiler }
}

/// Escapes a path for a build statement.
fn ninja_escape_path(p: &str) -> String {
    p.replace('$', "$$").replace(' ', "$ ").replace(':', "$:")
}

fn run_ninja(f: &Fixture, endpoint: Option<&str>, extra: &[&str]) -> i32 {
    let mut args: Vec<String> = Vec::new();
    if let Some(e) = endpoint {
        args.push(format!("--remote_executor={e}"));
        args.push("--remote_header=x-test=1".into());
    }
    args.extend(extra.iter().map(|s| s.to_string()));
    let opts = match parse_args(&args).unwrap() {
        ParseOutcome::Run(o) => o,
        ParseOutcome::Exit(..) => panic!("unexpected exit"),
    };
    std::env::set_current_dir(&f.build).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let code = rt.block_on(async {
        let mut s = Service::new(Interrupt::new());
        s.run(&opts).await
    });
    rt.shutdown_background();
    code
}

fn deps_of(f: &Fixture, out: &str) -> Vec<String> {
    let mut log = ninja_core::deps_log::DepsLog::new();
    let path = f.build.join(ninja_core::deps_log::DEPS_LOG_FILE);
    log.load(&path.to_string_lossy()).unwrap();
    log.get(out).map(|d| d.paths().map(str::to_string).collect()).unwrap_or_default()
}

fn check(cond: bool, msg: &str) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg.to_string()) }
}

async fn start_server(f: &Fixture) -> FakeServer {
    FakeServer::start(&f.root.join("server")).await.unwrap()
}

fn with_server<T>(f: &Fixture, body: impl FnOnce(&FakeServer) -> T) -> T {
    // The server runs on its own runtime thread so that the builds (which
    // create their own runtimes) can block.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let server = rt.block_on(start_server(f));
    let r = body(&server);
    drop(server);
    rt.shutdown_background();
    r
}

fn test_remote_build_and_cache() -> Result<(), String> {
    let f = fixture("remote", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        // 1. Clean build: the two compiles run remotely, the local=1
        //    compile and the link locally.
        check(run_ninja(&f, Some(&ep), &[]) == 0, "first build failed")?;
        let st = server.stats();
        check(st.executions == 2, &format!("expected 2 remote executions, got {st:?}"))?;
        let main_o = std::fs::read_to_string(f.build.join("obj/main.o")).map_err(|e| e.to_string())?;
        check(main_o.contains("#define DEEP 0") && main_o.contains("int main()"), "main.o content")?;
        check(std::fs::read_to_string(f.build.join("app.bin")).unwrap().starts_with("LINKED"), "app.bin")?;
        // The remote command used relative paths only.
        for (argv, wd) in server.state.executed.lock().unwrap().iter() {
            check(wd == "build", &format!("working directory {wd}"))?;
            for a in argv {
                let root = f.root.to_string_lossy().replace('\\', "/");
                check(!a.replace('\\', "/").contains(&root), &format!("absolute path in remote argv: {a}"))?;
            }
        }
        // Depfile paths were mapped back to local absolute paths.
        let deps = deps_of(&f, "obj/main.o");
        // Windows paths compare case-insensitively (TEST_TMPDIR may differ in
        // case from the on-disk spelling, e.g. Bazel's lowercased output base).
        let same_path = |a: &str, b: &str| {
            let (a, b) = (a.replace('\\', "/"), b.replace('\\', "/"));
            if cfg!(windows) { a.eq_ignore_ascii_case(&b) } else { a == b }
        };
        let deep = f.root.join("include/nested/deep.h").to_string_lossy().into_owned();
        check(deps.iter().any(|d| same_path(d, &deep)), &format!("deps of obj/main.o: {deps:?}"))?;
        check(!f.build.join("obj/main.o.d").exists(), "depfile should be deleted after reading")?;

        // 2. No-op build.
        check(run_ninja(&f, Some(&ep), &[]) == 0, "no-op build failed")?;
        check(server.stats().executions == 2, "no-op build executed something")?;

        // 3. Changing a header rebuilds both remote compiles.
        write(&f.root.join("include/nested/deep.h"), "#define DEEP 1\n");
        check(run_ninja(&f, Some(&ep), &[]) == 0, "rebuild after header change failed")?;
        check(server.stats().executions == 4, &format!("expected 4 executions, got {:?}", server.stats()))?;
        check(std::fs::read_to_string(f.build.join("obj/lib.o")).unwrap().contains("#define DEEP 1"), "lib.o not updated")?;

        // 4. Clean and rebuild: remote cache hits, no new executions.
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        let hits_before = server.stats().action_cache_hits;
        check(run_ninja(&f, Some(&ep), &[]) == 0, "rebuild after clean failed")?;
        let st = server.stats();
        check(st.executions == 4, &format!("expected cache hits, got {st:?}"))?;
        check(st.action_cache_hits >= hits_before + 2, &format!("expected 2 more AC hits, got {st:?}"))?;
        check(f.build.join("obj/main.o").exists(), "main.o not downloaded from cache")?;

        // 5. A failing remote compile fails the build.
        check(run_ninja(&f, Some(&ep), &["bad"]) == 1, "failing compile should fail the build")?;
        check(!f.build.join("obj/bad.o").exists(), "bad.o should not exist")?;
        Ok(())
    })
}

fn test_local_tool_steps() -> Result<(), String> {
    let f = fixture("localsteps", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let archives = ["--remote_mnemonics=Compile,Archive", "lib.a", "helper.stamp"];
        write(&f.build.join("pre.flag"), "");
        check(run_ninja(&f, Some(&ep), &archives) == 0, "archive build failed")?;
        // The compiles and the archive step ran remotely; the local tool's
        // steps before and after it, and the edge running nothing else, ran
        // locally.
        let st = server.stats();
        check(st.executions == 3, &format!("expected 3 remote executions, got {st:?}"))?;
        check(std::fs::read_to_string(f.build.join("lib.a")).unwrap().starts_with("LINKED"), "lib.a")?;
        check(!f.build.join("pre.flag").exists(), "the local step before the archive did not run")?;
        check(f.build.join("ar.stamp").exists(), "the local step after the archive did not run")?;
        check(f.build.join("helper.stamp").exists(), "helper.stamp missing")?;
        for (argv, _) in server.state.executed.lock().unwrap().iter() {
            check(!argv.iter().any(|a| a.contains("fake-cmake")), &format!("local tool in remote argv: {argv:?}"))?;
        }
        // Two remote steps run through the shell on POSIX hosts, locally on
        // Windows hosts.
        check(run_ninja(&f, Some(&ep), &["--remote_mnemonics=Compile,Archive", "lib2.a"]) == 0, "lib2.a build failed")?;
        let expected = if cfg!(windows) { 3 } else { 4 };
        let st = server.stats();
        check(st.executions == expected, &format!("expected {expected} remote executions, got {st:?}"))?;
        check(std::fs::read_to_string(f.build.join("lib2.copy")).unwrap().starts_with("LINKED\nLINKED"), "lib2.copy")?;
        Ok(())
    })
}

fn test_no_rbe_version_runs_locally() -> Result<(), String> {
    let f = fixture("local", false);
    with_server(&f, |server| {
        check(run_ninja(&f, Some(&server.endpoint), &[]) == 0, "local build failed")?;
        check(server.stats().executions == 0, "nothing may run remotely without rbe_version")?;
        check(f.build.join("app.bin").exists(), "app.bin missing")?;
        Ok(())
    })
}

/// The action cache entries in a disk cache.
fn disk_cache_entries(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("ac")).map(|shards| shards.flatten().map(|s| std::fs::read_dir(s.path()).map_or(0, |e| e.count())).sum()).unwrap_or(0)
}

fn test_disk_cache_alone() -> Result<(), String> {
    let f = fixture("disk", true);
    let disk = f.root.join("disk-cache");
    let flag = format!("--disk_cache={}", disk.to_string_lossy());
    // 1. A miss runs locally and stores the two compiles (not the
    //    rbe_local one, nor the link, which isn't a cached mnemonic).
    check(run_ninja(&f, None, &[&flag]) == 0, "first build failed")?;
    check(disk_cache_entries(&disk) == 2, &format!("expected 2 cached actions, got {}", disk_cache_entries(&disk)))?;
    let main_o = std::fs::read(f.build.join("obj/main.o")).map_err(|e| e.to_string())?;

    // 2. After a clean, the compiles come from the cache. To tell, the
    //    cached object is altered (same size, which is all a read checks).
    let blob = rbe_client::digest_of(&main_o);
    let blob_path = disk.join("cas").join(&blob.hash[..2]).join(&blob.hash);
    let mut altered = main_o.clone();
    altered[0] ^= 0x20;
    std::fs::write(&blob_path, &altered).map_err(|e| e.to_string())?;
    check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
    check(run_ninja(&f, None, &[&flag]) == 0, "rebuild from the disk cache failed")?;
    check(std::fs::read(f.build.join("obj/main.o")).ok() == Some(altered), "obj/main.o was not restored from the disk cache")?;
    check(std::fs::read_to_string(f.build.join("app.bin")).is_ok_and(|s| s.starts_with("LINKED")), "app.bin")?;
    // The depfile was stored with relative paths and mapped back.
    let deep = f.root.join("include/nested/deep.h").to_string_lossy().replace('\\', "/");
    let deps = deps_of(&f, "obj/main.o");
    check(deps.iter().any(|d| d.replace('\\', "/").eq_ignore_ascii_case(&deep)), &format!("deps of obj/main.o: {deps:?}"))?;

    // 3. A hit whose output is already there (the header's line endings
    //    changed and changed back: the object is the same): the output is
    //    made newer than the header, or every later build would run it.
    let header = f.root.join("include/nested/deep.h");
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).unwrap();
    std::fs::write(&header, "#define DEEP 0\r\n").unwrap();
    check(run_ninja(&f, None, &[&flag]) == 0, "build after CRLF failed")?;
    std::fs::write(&header, "#define DEEP 0\n").unwrap();
    check(run_ninja(&f, None, &[&flag]) == 0, "build after LF failed")?;
    check(mtime(&f.build.join("obj/main.o")) >= mtime(&header), "obj/main.o is older than deep.h after a disk cache hit")?;
    Ok(())
}

/// `--remote_cache`: a remote cache without remote execution. Misses run
/// locally and store their results; another checkout gets them as hits.
fn test_remote_cache_only() -> Result<(), String> {
    let f = fixture("cacheonly", true);
    with_server(&f, |server| {
        let flag = format!("--remote_cache={}", server.endpoint);
        // 1. Misses run locally and store the two compiles (not the
        //    rbe_local one, nor the link, which isn't a cached mnemonic).
        check(run_ninja(&f, None, &[&flag]) == 0, "first build failed")?;
        let st = server.stats();
        check(st.executions == 0 && st.action_cache_updates == 2, &format!("expected 2 stored results and no executions, got {st:?}"))?;

        // 2. Another checkout of the same sources: the compiles are hits, and
        //    the depfiles map back to its paths.
        let g = fixture("cacheonly-other", true);
        check(run_ninja(&g, None, &[&flag]) == 0, "build in the other checkout failed")?;
        let st = server.stats();
        check(st.executions == 0 && st.action_cache_hits == 2 && st.action_cache_updates == 2, &format!("expected 2 remote cache hits, got {st:?}"))?;
        check(std::fs::read(g.build.join("obj/main.o")).ok() == std::fs::read(f.build.join("obj/main.o")).ok(), "obj/main.o differs")?;
        check(std::fs::read_to_string(g.build.join("app.bin")).is_ok_and(|s| s.starts_with("LINKED")), "app.bin")?;
        let deep = g.root.join("include/nested/deep.h").to_string_lossy().replace('\\', "/");
        let deps = deps_of(&g, "obj/main.o");
        check(deps.iter().any(|d| d.replace('\\', "/").eq_ignore_ascii_case(&deep)), &format!("deps of obj/main.o: {deps:?}"))?;

        // 3. Read only: a changed source misses, runs locally, and nothing
        //    is stored.
        write(
            &g.root.join("src/lib.c"),
            "#include \"lib.h\"
int lib() { return DEEP + 1; }
",
        );
        check(run_ninja(&g, None, &[&flag, "--noremote_upload_local_results"]) == 0, "read-only build failed")?;
        let st = server.stats();
        check(st.executions == 0 && st.action_cache_updates == 2, &format!("a read-only build stored results: {st:?}"))?;
        Ok(())
    })
}

/// An action that can't be sent runs locally, unless --remote_strict.
/// Remote failures run locally too, until --remote_max_fallbacks is
/// reached, and not at all with --remote_strict.
fn test_fallback_guards() -> Result<(), String> {
    let f = fixture("fallbacks", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let exists = |p: &str| f.build.join(p).exists();
        let clean = || check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed");
        check(run_ninja(&f, Some(&ep), &["sandwich.o"]) == 0, "sandwich.o failed")?;
        check(exists("sandwich.o") && exists("sandwich.flag"), "sandwich.o wasn't built locally")?;
        clean()?;
        check(run_ninja(&f, Some(&ep), &["--remote_strict", "sandwich.o"]) != 0, "--remote_strict ran an action that can't be sent")?;
        check(!exists("sandwich.o"), "sandwich.o built with --remote_strict")?;

        server.state.set_fail_executions(true);
        let objs = ["obj/main.o", "obj/lib.o"];
        check(run_ninja(&f, Some(&ep), &["-j1", objs[0], objs[1]]) == 0, "remote failures didn't run locally")?;
        check(objs.iter().all(|o| exists(o)), "objects missing after local runs")?;
        clean()?;
        // The first failure runs locally, the second fails.
        check(run_ninja(&f, Some(&ep), &["-j1", "-k", "0", "--remote_max_fallbacks=1", objs[0], objs[1]]) != 0, "the limit didn't apply")?;
        let built = objs.iter().filter(|o| exists(o)).count();
        check(built == 1, &format!("expected one object built locally, got {built}"))?;
        clean()?;
        check(run_ninja(&f, Some(&ep), &["--remote_strict", objs[0]]) != 0, "--remote_strict ran a remote failure locally")?;
        check(!exists(objs[0]), "obj/main.o built with --remote_strict")?;
        server.state.set_fail_executions(false);
        Ok(())
    })
}

/// --remote_racing: a slow remote execution loses to a local run when a
/// local slot is free; a slow local run is stopped when the remote result
/// arrives first, and the remote outputs are written.
fn test_racing() -> Result<(), String> {
    use std::time::{Duration, Instant};
    let f = fixture("racing", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let objs = ["obj/main.o", "obj/lib.o"];
        let local_made = |o: &str| std::fs::read_to_string(f.build.join(o)).is_ok_and(|s| s.contains("RACE-LOCAL"));
        let race = ["--remote_racing", "--remote_racing_delay=0", "-j4", objs[0], objs[1]];
        write(&f.build.join("race-local-mark"), "");

        // 1. The remote side is slow: the local runs win.
        server.state.set_execute_delay(Duration::from_secs(4));
        let start = Instant::now();
        check(run_ninja(&f, Some(&ep), &race) == 0, "racing build failed")?;
        check(start.elapsed() < Duration::from_secs(4), &format!("waited for the remote side: {:?}", start.elapsed()))?;
        check(objs.iter().all(|o| local_made(o)), "outputs not from the local runs")?;

        // 2. The local side is slow: the remote results win, the local
        //    commands are stopped and don't overwrite them.
        server.state.set_execute_delay(Duration::ZERO);
        server.state.clear_action_cache();
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        write(&f.build.join("race-local-sleep-ms"), "4000");
        let start = Instant::now();
        check(run_ninja(&f, Some(&ep), &race) == 0, "racing build failed")?;
        check(start.elapsed() < Duration::from_secs(4), &format!("waited for the local side: {:?}", start.elapsed()))?;
        std::thread::sleep(Duration::from_secs(5));
        check(objs.iter().all(|o| f.build.join(o).exists() && !local_made(o)), "outputs not from the remote side")?;
        let _ = std::fs::remove_file(f.build.join("race-local-sleep-ms"));
        let _ = std::fs::remove_file(f.build.join("race-local-mark"));
        Ok(())
    })
}

fn test_disk_cache_with_remote() -> Result<(), String> {
    let f = fixture("diskremote", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let disk = f.root.join("disk-cache");
        let flag = format!("--disk_cache={}", disk.to_string_lossy());
        // 1. Remote executions are kept in the disk cache too.
        check(run_ninja(&f, Some(&ep), &[&flag]) == 0, "remote build failed")?;
        check(server.stats().executions == 2, "expected 2 remote executions")?;
        check(disk_cache_entries(&disk) == 2, "remote results not stored in the disk cache")?;
        // 2. With the remote cache emptied, a rebuild after a clean still
        //    needs nothing remote: the disk cache has it all.
        server.state.clear_action_cache();
        server.state.clear_cas();
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        check(run_ninja(&f, Some(&ep), &[&flag]) == 0, "rebuild failed")?;
        check(server.stats().executions == 2, &format!("the rebuild executed remotely: {:?}", server.stats()))?;
        check(f.build.join("obj/main.o").exists() && f.build.join("app.bin").exists(), "outputs missing")?;
        let deep = f.root.join("include/nested/deep.h").to_string_lossy().replace('\\', "/");
        let deps = deps_of(&f, "obj/main.o");
        check(deps.iter().any(|d| d.replace('\\', "/").eq_ignore_ascii_case(&deep)), &format!("deps of obj/main.o: {deps:?}"))?;
        Ok(())
    })
}

fn test_build_events() -> Result<(), String> {
    use bep::build_event::Payload;
    use bep::build_event_id::Id;
    use rbe_bes::proto::build_event_stream as bep;
    fn id_of(e: &bep::BuildEvent) -> Option<Id> {
        e.id.as_ref().and_then(|i| i.id.clone())
    }
    let f = fixture("bes", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let inv = "11111111-2222-4333-8444-555555555555";
        let file = f.root.join("events.bin");
        let args = [
            format!("--bes_backend={ep}"),
            format!("--invocation_id={inv}"),
            format!("--build_event_binary_file={}", file.to_string_lossy()),
            "--bes_results_url=https://ui.example/invocation/".to_string(),
            "app.bin".to_string(),
            "lib.a".to_string(),
        ];
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        check(run_ninja(&f, Some(&ep), &args) == 0, "build with build events failed")?;

        // 1. The uploaded stream is valid, and the file holds the same events.
        let events = server.state.build_events.lock().unwrap().clone();
        rbe_bes::events::validate(&events)?;
        let from_file = ninja_rs::bes::read_binary_file(&file).map_err(|e| e.to_string())?;
        check(from_file == events, "the binary file differs from the uploaded events")?;
        check(server.state.lifecycle_events.lock().unwrap().len() == 4, "expected 4 lifecycle events")?;
        check(*server.state.bes_streams_finished.lock().unwrap() == 1, "the event stream wasn't finished")?;
        let started = events.iter().find_map(|e| match &e.payload {
            Some(Payload::Started(s)) => Some(s.clone()),
            _ => None,
        });
        check(started.is_some_and(|s| s.uuid == inv), "BuildStarted with the invocation id")?;
        // BuildStarted announces the requested targets (BES services show
        // them as the invocation's title) and the structured command lines.
        let pattern = |e: &bep::BuildEvent| {
            e.children.iter().find_map(|c| match &c.id {
                Some(Id::Pattern(p)) => Some(p.pattern.clone()),
                _ => None,
            })
        };
        check(pattern(&events[0]) == Some(vec!["app.bin".into(), "lib.a".into()]), &format!("announced pattern: {:?}", pattern(&events[0])))?;
        let canonical = events.iter().find_map(|e| match &e.payload {
            Some(Payload::StructuredCommandLine(c)) if c.command_line_label == "canonical" => Some(c.clone()),
            _ => None,
        });
        let canonical = canonical.ok_or("no canonical command line")?;
        use rbe_bes::proto::command_line::command_line_section::SectionType;
        let section = |label: &str| canonical.sections.iter().find(|s| s.section_label == label).and_then(|s| s.section_type.clone());
        check(
            matches!(section("residual"), Some(SectionType::ChunkList(c)) if c.chunk == ["app.bin", "lib.a"]),
            &format!("residual: {:?}", section("residual")),
        )?;
        check(matches!(section("command"), Some(SectionType::ChunkList(c)) if c.chunk == ["build"]), &format!("command: {:?}", section("command")))?;
        check(
            matches!(section("command options"), Some(SectionType::OptionList(o))
                if o.option.iter().any(|o| o.option_name == "invocation_id" && o.option_value == inv && o.combined_form == format!("--invocation_id={inv}"))),
            &format!("command options: {:?}", section("command options")),
        )?;

        // 2. The configuration, the target and its completion with its output.
        let config_id = events
            .iter()
            .find_map(|e| match (id_of(e), &e.payload) {
                (Some(Id::Configuration(c)), Some(Payload::Configuration(p))) if p.mnemonic == "x86_64-test-Release" => Some(c.id),
                _ => None,
            })
            .ok_or("no Configuration event")?;
        let kind = events.iter().find_map(|e| match (id_of(e), &e.payload) {
            (Some(Id::TargetConfigured(t)), Some(Payload::Configured(c))) if t.label == "//src:app" => Some(c.target_kind.clone()),
            _ => None,
        });
        check(kind.as_deref() == Some("executable rule"), &format!("TargetConfigured //src:app: {kind:?}"))?;
        let completed = events
            .iter()
            .find_map(|e| match (id_of(e), &e.payload) {
                (Some(Id::TargetCompleted(t)), Some(Payload::Completed(c))) if t.label == "//src:app" => {
                    Some((t.configuration.map(|c| c.id).unwrap_or_default(), c.clone()))
                }
                _ => None,
            })
            .ok_or("no TargetCompleted //src:app")?;
        check(completed.0 == config_id && completed.1.success, "//src:app completed successfully in its configuration")?;
        let set = completed.1.output_group.first().and_then(|g| g.file_sets.first()).map(|s| s.id.clone()).ok_or("no output set")?;
        let files = events
            .iter()
            .find_map(|e| match (id_of(e), &e.payload) {
                (Some(Id::NamedSet(n)), Some(Payload::NamedSetOfFiles(s))) if n.id == set => Some(s.files.clone()),
                _ => None,
            })
            .ok_or("no NamedSetOfFiles")?;
        let uri = match files.first().and_then(|f| f.file.clone()) {
            Some(bep::file::File::Uri(u)) => u,
            other => return Err(format!("app.bin entry: {other:?}")),
        };
        check(files[0].name == "app.bin" && uri.starts_with("file://") && files[0].digest.len() == 64, &format!("app.bin entry: {:?}", files[0]))?;
        // A static library's artifact is its output too, deliverable or not.
        let lib_set = events
            .iter()
            .find_map(|e| match (id_of(e), &e.payload) {
                (Some(Id::TargetCompleted(t)), Some(Payload::Completed(c))) if t.label == "//src:lib" && c.success => {
                    c.output_group.first().and_then(|g| g.file_sets.first()).map(|s| s.id.clone())
                }
                _ => None,
            })
            .ok_or("no TargetCompleted //src:lib with outputs")?;
        let lib_files: Vec<String> = events
            .iter()
            .find_map(|e| match (id_of(e), &e.payload) {
                (Some(Id::NamedSet(n)), Some(Payload::NamedSetOfFiles(s))) if n.id == lib_set => Some(s.files.iter().map(|f| f.name.clone()).collect()),
                _ => None,
            })
            .unwrap_or_default();
        check(lib_files == ["lib.a"], &format!("//src:lib outputs: {lib_files:?}"))?;
        let finished = events.iter().find_map(|e| match &e.payload {
            Some(Payload::Finished(f)) => f.exit_code.clone(),
            _ => None,
        });
        check(finished.is_some_and(|c| c.code == 0 && c.name == "SUCCESS"), "BuildFinished SUCCESS")?;

        // 3. Remote executions carry the same ids.
        let metas = server.state.request_metadata.lock().unwrap().clone();
        check(!metas.is_empty(), "no request metadata")?;
        for m in &metas {
            check(
                m.tool_invocation_id == inv && m.target_id == "//src:app" && m.configuration_id == config_id && m.action_mnemonic == "CCompile",
                &format!("request metadata: {m:?}"),
            )?;
        }

        // 4. A failing build (events to the file only): the failed action,
        //    its target failed, and the target depending on it skipped.
        let file2 = f.root.join("events2.bin");
        let flag = format!("--build_event_binary_file={}", file2.to_string_lossy());
        check(run_ninja(&f, Some(&ep), &[&flag, "user.bin"]) == 1, "the failing build should fail")?;
        let events = ninja_rs::bes::read_binary_file(&file2).map_err(|e| e.to_string())?;
        rbe_bes::events::validate(&events)?;
        let action = events.iter().find_map(|e| match &e.payload {
            Some(Payload::Action(a)) => Some(a.clone()),
            _ => None,
        });
        #[allow(deprecated)]
        let ok = action.is_some_and(|a| !a.success && a.exit_code == 1 && a.r#type == "CCompile" && a.label == "//:broken");
        check(ok, "ActionExecuted for the failed compile")?;
        let broken = events
            .iter()
            .any(|e| matches!((id_of(e), &e.payload), (Some(Id::TargetCompleted(t)), Some(Payload::Completed(c))) if t.label == "//:broken" && !c.success));
        check(broken, "//:broken completed as failed")?;
        let skipped = events.iter().any(|e| {
            matches!((id_of(e), &e.payload), (Some(Id::TargetCompleted(t)), Some(Payload::Aborted(a)))
                if t.label == "//:user" && a.reason == bep::aborted::AbortReason::Skipped as i32)
        });
        check(skipped, "//:user aborted as skipped")?;
        let finished = events.iter().find_map(|e| match &e.payload {
            Some(Payload::Finished(f)) => f.exit_code.clone(),
            _ => None,
        });
        check(finished.is_some_and(|c| c.code == 1), "BuildFinished BUILD_FAILURE")?;

        // 4. Without targets, the defaults are announced (app.bin here).
        let file3 = f.root.join("events3.bin");
        let flag = format!("--build_event_binary_file={}", file3.to_string_lossy());
        check(run_ninja(&f, Some(&ep), &[&flag, "-j", "2"]) == 0, "the default build failed")?;
        let events = ninja_rs::bes::read_binary_file(&file3).map_err(|e| e.to_string())?;
        rbe_bes::events::validate(&events)?;
        check(pattern(&events[0]) == Some(vec!["app.bin".into()]), &format!("default pattern: {:?}", pattern(&events[0])))?;
        let given = events.iter().find_map(|e| match &e.payload {
            Some(Payload::StructuredCommandLine(c)) if c.command_line_label == "original" => {
                c.sections.iter().find(|s| s.section_label == "command options").and_then(|s| s.section_type.clone())
            }
            _ => None,
        });
        check(
            matches!(&given, Some(SectionType::OptionList(o)) if o.option.iter().any(|o| o.option_name == "j" && o.option_value == "2" && o.combined_form == "-j 2")),
            &format!("short options: {given:?}"),
        )?;
        Ok(())
    })
}

/// Lines of incompressible C (xorshift noise), for large sources.
fn noise_lines(count: usize, mut state: u64) -> Vec<String> {
    (0..count)
        .map(|i| {
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            format!("static const char *s{i} = \"{:016x}{:016x}\";", next(), next())
        })
        .collect()
}

/// With a remote that splits and splices blobs (as RE API 2.`minor`), a
/// large input is uploaded as the chunks the server lacks, and a large
/// output downloaded as the chunks the disk cache lacks.
fn test_split_and_splice(minor: u32) -> Result<(), String> {
    let f = fixture(&format!("chunks{minor}"), true);
    with_server(&f, |server| {
        server.state.set_chunking(Some(minor));
        let ep = server.endpoint.clone();
        let flag = format!("--disk_cache={}", f.root.join("disk-cache").to_string_lossy());
        let mut lines = noise_lines(160_000, u64::from(minor));
        let source = |lines: &[String]| format!("#include \"lib.h\"\n{}\n", lines.join("\n"));
        write(&f.root.join("src/big.c"), &source(&lines));
        let size = source(&lines).len() as u64;

        // 1. Nothing is known yet: all the chunks travel.
        check(run_ninja(&f, Some(&ep), &[&flag, "obj/big.o"]) == 0, "first build failed")?;
        let st = server.stats();
        // The source is spliced, and so is the compiler if it's large (it's
        // this test's binary).
        check(st.executions == 1 && st.splices >= 1 && st.splits == 1, &format!("expected splices and one split, got {st:?}"))?;
        let big_o = std::fs::read_to_string(f.build.join("obj/big.o")).map_err(|e| e.to_string())?;
        check(big_o.contains("#define DEEP 0") && big_o.ends_with(&format!("{}\n", lines.last().unwrap())), "obj/big.o content")?;

        // 2. After an edit in the middle, only the chunks around it do.
        lines.insert(80_000, "int edited;".into());
        write(&f.root.join("src/big.c"), &source(&lines));
        check(run_ninja(&f, Some(&ep), &[&flag, "obj/big.o"]) == 0, "rebuild failed")?;
        let after = server.stats();
        check(after.executions == 2 && after.splices == st.splices + 1 && after.splits == 2, &format!("expected another splice and split, got {after:?}"))?;
        let (up, down) = (after.bytes_uploaded - st.bytes_uploaded, after.bytes_read - st.bytes_read);
        println!("after the edit: {up} bytes uploaded, {down} downloaded, of {size}");
        check(up < size / 4, &format!("uploaded {up} of {size} bytes"))?;
        check(down < size / 4, &format!("downloaded {down} of {size} bytes"))?;
        let big_o = std::fs::read_to_string(f.build.join("obj/big.o")).map_err(|e| e.to_string())?;
        check(big_o.contains("\nint edited;\n") && big_o.ends_with(&format!("{}\n", lines.last().unwrap())), "obj/big.o content after the edit")?;
        Ok(())
    })
}

/// The outputs recorded as remote-only.
fn remote_only(f: &Fixture) -> Vec<String> {
    let text = std::fs::read_to_string(f.build.join(".ninja_remote")).unwrap_or_default();
    text.lines().skip(1).filter_map(|l| l.splitn(5, ' ').nth(4)).map(str::to_string).collect()
}

/// `--remote_download_minimal`: objects stay remote, deliverables come down,
/// local commands and named targets get their files, and outputs gone
/// from the remote cache are built again.
fn test_minimal_downloads() -> Result<(), String> {
    let f = fixture("minimal", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flags = ["--remote_download_minimal", "--remote_mnemonics=Compile,Link"];
        // 1. The compiles and the link run remotely; only the executable
        //    (a deliverable) and the rbe_local object are on disk.
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        check(server.stats().executions == 3, &format!("expected 3 remote executions, got {:?}", server.stats()))?;
        check(!f.build.join("obj/main.o").exists() && !f.build.join("obj/lib.o").exists(), "objects were downloaded")?;
        check(remote_only(&f) == ["obj/lib.o", "obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        let app = std::fs::read_to_string(f.build.join("app.bin")).map_err(|e| e.to_string())?;
        check(app.starts_with("LINKED") && app.contains("int main()"), "app.bin content")?;
        // The deps of a remote-only object were still recorded.
        check(!deps_of(&f, "obj/main.o").is_empty(), "no deps for obj/main.o")?;

        // 2. Nothing to do: recorded outputs count as present.
        check(run_ninja(&f, Some(&ep), &flags) == 0, "no-op build failed")?;
        check(server.stats().executions == 3, "the no-op build executed something")?;

        // 3. Outputs gone from the remote cache are built again.
        server.state.clear_cas();
        server.state.clear_action_cache();
        check(run_ninja(&f, Some(&ep), &flags) == 0, "build after eviction failed")?;
        check(server.stats().executions == 6, &format!("expected the 3 actions again, got {:?}", server.stats()))?;
        check(remote_only(&f) == ["obj/lib.o", "obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;

        // 4. A local command gets its remote-only input downloaded first.
        check(run_ninja(&f, Some(&ep), &[flags[0], flags[1], "copy.o"]) == 0, "local copy failed")?;
        let lib_o = std::fs::read(f.build.join("obj/lib.o")).map_err(|e| format!("obj/lib.o: {e}"))?;
        check(std::fs::read(f.build.join("copy.o")).ok() == Some(lib_o), "copy.o content")?;
        check(remote_only(&f) == ["obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;

        // 5. A target named on the command line is downloaded, even up to
        //    date, without making what depends on it stale.
        check(run_ninja(&f, Some(&ep), &[flags[0], flags[1], "obj/main.o"]) == 0, "named target failed")?;
        check(f.build.join("obj/main.o").exists(), "obj/main.o was not downloaded")?;
        check(remote_only(&f).is_empty(), &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        check(run_ninja(&f, Some(&ep), &flags) == 0, "final no-op failed")?;
        check(server.stats().executions == 6, &format!("downloading made something stale: {:?}", server.stats()))?;

        // 6. Cleaning forgets the records.
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        check(!f.build.join(".ninja_remote").exists(), ".ninja_remote survived the clean")?;
        Ok(())
    })
}

/// Action rewinding: remote-only inputs evicted during the build are built
/// again by their own actions, then the action needing them runs, in the
/// same build.
fn test_action_rewinding() -> Result<(), String> {
    let f = fixture("rewind", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flags = ["--remote_download_minimal", "--remote_mnemonics=Compile,Link"];
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        check(server.stats().executions == 3, &format!("expected 3 remote executions, got {:?}", server.stats()))?;
        check(remote_only(&f) == ["obj/lib.o", "obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;

        // The link runs again; the CAS loses everything when it's sent, after
        // the build checked the objects were there.
        std::fs::remove_file(f.build.join("app.bin")).map_err(|e| e.to_string())?;
        server.state.clear_action_cache();
        server.state.evict_on_next_execute();
        check(run_ninja(&f, Some(&ep), &flags) == 0, "build with an eviction failed")?;
        // Both compiles and the link ran (the link's first attempt never ran:
        // its inputs were missing).
        check(server.stats().executions == 6, &format!("expected 3 more executions, got {:?}", server.stats()))?;
        let app = std::fs::read_to_string(f.build.join("app.bin")).map_err(|e| e.to_string())?;
        check(app.starts_with("LINKED") && app.contains("int main()"), "app.bin content")?;
        check(remote_only(&f) == ["obj/lib.o", "obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        // And it's all consistent: nothing left to do.
        check(run_ninja(&f, Some(&ep), &flags) == 0, "no-op build failed")?;
        check(server.stats().executions == 6, &format!("the no-op build executed something: {:?}", server.stats()))?;
        Ok(())
    })
}

/// Minimal downloads with a disk cache: results whose outputs stayed remote
/// are kept, and trusted without asking the remote. Outputs gone from the
/// remote CAS make the result stale once an action reading them reports
/// them lost: the rewound action runs again instead of finding it.
fn test_disk_cache_minimal() -> Result<(), String> {
    let f = fixture("diskminimal", true);
    let disk = f.root.join("disk-cache");
    let disk_flag = format!("--disk_cache={}", disk.to_string_lossy());
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flags = ["--remote_download_minimal", "--remote_mnemonics=Compile,Link", &disk_flag];
        // 1. Executed remotely; the compiles' results are kept although
        //    their objects stayed remote.
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        check(server.stats().executions == 3, &format!("expected 3 remote executions, got {:?}", server.stats()))?;
        check(remote_only(&f) == ["obj/lib.o", "obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        check(disk_cache_entries(&disk) == 3, &format!("expected 3 cached results, got {}", disk_cache_entries(&disk)))?;

        // 2. After a clean, all from the disk cache, without a remote lookup.
        let hits = server.stats().action_cache_hits;
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        check(run_ninja(&f, Some(&ep), &flags) == 0, "build from the disk cache failed")?;
        let st = server.stats();
        check(st.executions == 3 && st.action_cache_hits == hits, &format!("the remote was asked: {st:?}"))?;
        check(remote_only(&f) == ["obj/lib.o", "obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        check(std::fs::read_to_string(f.build.join("app.bin")).is_ok_and(|s| s.starts_with("LINKED")), "app.bin")?;

        // 3. The remote CAS loses everything, and a local link needs the
        //    objects: they're lost, the compiles rewound, their disk
        //    results stale. They run again rather than loop.
        server.state.clear_cas();
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        let local_link = ["--remote_download_minimal", "--remote_mnemonics=Compile", &disk_flag];
        check(run_ninja(&f, Some(&ep), &local_link) == 0, "build with lost objects failed")?;
        check(server.stats().executions == 5, &format!("expected the 2 compiles again, got {:?}", server.stats()))?;
        let app = std::fs::read_to_string(f.build.join("app.bin")).map_err(|e| e.to_string())?;
        check(app.starts_with("LINKED") && app.contains("int main()"), "app.bin content")?;

        // 4. The fresh results replaced the stale ones: a clean build is all
        //    disk cache hits again.
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        check(run_ninja(&f, Some(&ep), &flags) == 0, "last build failed")?;
        check(server.stats().executions == 5, &format!("expected no more executions, got {:?}", server.stats()))?;
        Ok(())
    })
}

/// zstd: with a remote that supports it, blobs travel compressed, batched
/// and through ByteStream, both ways; `--noremote_cache_compression` sends
/// them as they are.
fn test_compression() -> Result<(), String> {
    let text = |n: usize| {
        (0..n)
            .map(|i| {
                format!(
                    "int v{i} = {i}; /* a line of a large, repetitive source */
"
                )
            })
            .collect::<String>()
    };
    let mut stats = Vec::new();
    for compress in [true, false] {
        let f = fixture(&format!("zstd-{compress}"), true);
        // Past the fake server's 4 MiB batches (ByteStream), and within.
        write(&f.root.join("src/big.c"), &text(100_000));
        write(
            &f.root.join("src/main.c"),
            &format!(
                "#include \"lib.h\"
int main() {{ return lib(); }}
{}",
                text(15_000)
            ),
        );
        with_server(&f, |server| {
            server.state.set_compression(true);
            let ep = server.endpoint.clone();
            let mut args = vec!["obj/big.o", "obj/main.o"];
            if !compress {
                args.push("--noremote_cache_compression");
            }
            check(run_ninja(&f, Some(&ep), &args) == 0, "build failed")?;
            let st = server.stats();
            if compress {
                check(st.compressed_writes >= 2 && st.compressed_reads >= 2, &format!("expected compressed transfers both ways: {st:?}"))?;
            } else {
                check(st.compressed_writes == 0 && st.compressed_reads == 0, &format!("compressed although asked not to: {st:?}"))?;
            }
            stats.push(st);
            let big_o = std::fs::read_to_string(f.build.join("obj/big.o")).map_err(|e| e.to_string())?;
            check(
                big_o.ends_with(
                    "int v99999 = 99999; /* a line of a large, repetitive source */
",
                ),
                "obj/big.o content",
            )?;
            let main_o = std::fs::read_to_string(f.build.join("obj/main.o")).map_err(|e| e.to_string())?;
            check(main_o.contains("int main()") && main_o.contains("int v14999 = 14999;"), "obj/main.o content")?;
            Ok::<(), String>(())
        })?;
    }
    // The sources (7 MB) and objects went over in a fraction of their size.
    let (on, off) = (&stats[0], &stats[1]);
    check(on.bytes_uploaded + 5_000_000 < off.bytes_uploaded, &format!("uploads weren't smaller: {} against {}", on.bytes_uploaded, off.bytes_uploaded))?;
    check(on.bytes_read * 5 < off.bytes_read, &format!("downloads weren't smaller: {} against {}", on.bytes_read, off.bytes_read))?;
    Ok(())
}

/// The members of a thin archive recorded as remote-only.
fn recorded_members(f: &Fixture, archive: &str) -> Vec<String> {
    let text = std::fs::read_to_string(f.build.join(".ninja_remote")).unwrap_or_default();
    let mut lines = text.lines().skip_while(|l| l.splitn(5, ' ').nth(4) != Some(archive));
    lines.next();
    lines.map_while(|l| l.strip_prefix("+ ")).map(str::to_string).collect()
}

/// Thin archives: a remote link gets the members of the archives it reads.
/// Downloaded, the archive is read from disk; left remote, its members are
/// read with ranged reads past its symbol table, and recorded; a local link
/// gets the archive and its members downloaded.
fn test_thin_archives() -> Result<(), String> {
    let mnemonics = "--remote_mnemonics=Compile,Link,Archive";
    let linked = |f: &Fixture| std::fs::read_to_string(f.build.join("thin.bin")).unwrap_or_default();
    // All outputs downloaded.
    let f = fixture("thin", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &[mnemonics, "thin.bin"]) == 0, "build failed")?;
        check(server.stats().executions == 4, &format!("expected 4 remote executions, got {:?}", server.stats()))?;
        check(f.build.join("lib/libthin.a").exists(), "the archive wasn't downloaded")?;
        check(linked(&f).contains("int main()") && linked(&f).contains("int lib()"), &format!("thin.bin: {}", linked(&f)))?;
        Ok::<(), String>(())
    })?;

    // Minimal downloads.
    let f = fixture("thin-minimal", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flags = ["--remote_download_minimal", mnemonics, "thin.bin"];
        check(run_ninja(&f, Some(&ep), &flags) == 0, "build failed")?;
        let stats = server.stats();
        check(stats.executions == 4, &format!("expected 4 remote executions, got {stats:?}"))?;
        check(!f.build.join("lib/libthin.a").exists() && !f.build.join("obj/main.o").exists(), "the archive or an object was downloaded")?;
        check(recorded_members(&f, "lib/libthin.a") == ["../obj/main.o", "../obj/lib.o"], &format!("members: {:?}", recorded_members(&f, "lib/libthin.a")))?;
        // The members were read past the symbol table (100 kB).
        check(stats.ranged_reads == 2 && stats.bytes_read < 10_000, &format!("reads: {stats:?}"))?;
        check(linked(&f).contains("int main()") && linked(&f).contains("int lib()"), &format!("thin.bin: {}", linked(&f)))?;

        // A member changes: the link gets the new one.
        write(&f.root.join("src/lib.c"), "#include \"lib.h\"\nint lib() { return DEEP + 1; }\n");
        check(run_ninja(&f, Some(&ep), &flags) == 0, "rebuild failed")?;
        check(server.stats().executions == 7, &format!("expected the compile, archive and link again, got {:?}", server.stats()))?;
        check(linked(&f).contains("DEEP + 1"), &format!("thin.bin is stale: {}", linked(&f)))?;

        // A local link downloads the archive and its members.
        let remote_link = linked(&f);
        std::fs::remove_file(f.build.join("thin.bin")).map_err(|e| e.to_string())?;
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_mnemonics=Compile,Archive", "thin.bin"]) == 0, "local link failed")?;
        check(server.stats().executions == 7, &format!("something ran remotely: {:?}", server.stats()))?;
        check(linked(&f) == remote_link, &format!("local link: {}", linked(&f)))?;
        check(f.build.join("lib/libthin.a").exists() && f.build.join("obj/lib.o").exists(), "the archive and its members weren't downloaded")?;
        Ok(())
    })
}

/// The test certificates (crates/fake-reapi/testdata/tls).
fn tls_file(name: &str) -> String {
    let dir = match std::env::var_os("TEST_SRCDIR") {
        // Bazel: the data files under the runfiles tree.
        Some(r) => PathBuf::from(r).join(std::env::var("TEST_WORKSPACE").unwrap_or_else(|_| "_main".into())).join("crates/fake-reapi/testdata/tls"),
        // Cargo: the package directory, set when tests run.
        None => PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../fake-reapi/testdata/tls"),
    };
    dir.join(name).to_string_lossy().into_owned()
}

/// mTLS: the server requires a client certificate signed by its CA. With
/// the CA and a client certificate, remote execution and build events work
/// (also when the server's name differs from the endpoint's host, with
/// --tls_authority_override); without the CA, the client certificate, or
/// with one signed by another CA, connecting fails.
fn test_mtls() -> Result<(), String> {
    let f = fixture("mtls", true);
    let read = |n: &str| std::fs::read(tls_file(n)).map_err(|e| format!("{}: {e}", tls_file(n)));
    let tls = fake_reapi::ServerTls { cert_pem: read("server.pem")?, key_pem: read("server-key.pem")?, client_ca_pem: Some(read("ca.pem")?) };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let server = rt.block_on(fake_reapi::FakeServer::start_with(&f.root.join("server"), 0, Some(tls))).map_err(|e| e.to_string())?;
    let r = (|| {
        let ep = server.endpoint.clone();
        check(ep.starts_with("grpcs://localhost:"), &format!("endpoint {ep}"))?;
        let ca = format!("--tls_certificate={}", tls_file("ca.pem"));
        let cert = format!("--tls_client_certificate={}", tls_file("client.pem"));
        let key = format!("--tls_client_key={}", tls_file("client-key.pem"));
        let bes = format!("--bes_backend={ep}");
        check(run_ninja(&f, Some(&ep), &[&ca, &cert, &key, &bes, "--remote_strict"]) == 0, "build over mTLS failed")?;
        check(server.stats().executions == 2, &format!("expected 2 remote executions, got {:?}", server.stats()))?;
        check(!server.state.build_events.lock().unwrap().is_empty(), "no build events over mTLS")?;

        // The server's certificate names rbe.test too: connecting by address
        // with that name works.
        let port = ep.rsplit(':').next().unwrap();
        let by_ip = format!("grpcs://127.0.0.1:{port}");
        run_ninja(&f, None, &["-t", "clean"]);
        check(
            run_ninja(&f, Some(&by_ip), &[&ca, &cert, &key, "--tls_authority_override=rbe.test", "--remote_strict"]) == 0,
            "build with --tls_authority_override failed",
        )?;
        check(server.stats().action_cache_hits == 2, &format!("expected 2 cache hits, got {:?}", server.stats()))?;

        // Refused: no client certificate, one from another CA, or a server
        // the client doesn't trust. Every action needs the server, strictly.
        let other_cert = format!("--tls_client_certificate={}", tls_file("other-client.pem"));
        let other_key = format!("--tls_client_key={}", tls_file("other-client-key.pem"));
        for (what, flags) in [
            ("without a client certificate", vec![ca.as_str()]),
            ("with an untrusted client certificate", vec![&ca, &other_cert, &other_key]),
            ("without trusting the server's CA", vec![&cert, &key]),
        ] {
            run_ninja(&f, None, &["-t", "clean"]);
            let mut args = flags.clone();
            args.push("--remote_strict");
            check(run_ninja(&f, Some(&ep), &args) != 0, &format!("a build {what} succeeded"))?;
        }
        let stats = server.stats();
        check(stats.executions == 2 && stats.action_cache_hits == 2, &format!("refused clients got through: {stats:?}"))?;
        Ok(())
    })();
    drop(server);
    rt.shutdown_background();
    r
}

/// The include scanners other than the conservative one, with a compiler
/// none of them can use (the fake one): the conservative scanner takes over.
fn test_include_scanner_fallback() -> Result<(), String> {
    for scanner in ["preprocessor", "clang-scan-deps", "compiler"] {
        let f = fixture(&format!("scanner-{scanner}"), true);
        let flag = format!("--remote_include_scanner={scanner}");
        with_server(&f, |server| {
            let ep = server.endpoint.clone();
            check(run_ninja(&f, Some(&ep), &[&flag, "--remote_strict"]) == 0, &format!("build with {flag} failed"))?;
            check(server.stats().executions == 2, &format!("{flag}: expected 2 remote executions, got {:?}", server.stats()))?;
            Ok::<(), String>(())
        })?;
    }
    Ok(())
}

/// Builds through the daemon (started by the first, running this test
/// binary): an edited header, same size, must reach the remote actions
/// although the daemon vouched for it after the second build.
fn test_daemon() -> Result<(), String> {
    let f = fixture("daemon", true);
    // A repository: watched.
    std::fs::create_dir_all(f.root.join(".git")).unwrap();
    let dir = f.root.join("daemon");
    // SAFETY: the tests run one at a time; builds read these later.
    unsafe {
        std::env::set_var("NINJA_RS_DAEMON_DIR", &dir);
        std::env::set_var("NINJA_RS_DAEMON_IDLE", "120");
    }
    let r = with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let build = || run_ninja(&f, Some(&ep), &["--daemon", "--remote_include_scanner=preprocessor", "--remote_strict"]);
        check(build() == 0, "first build failed")?;
        check(server.stats().executions == 2, &format!("expected 2 remote executions, got {:?}", server.stats()))?;
        let deep = f.root.join("include/nested/deep.h");
        for (i, value) in ["1", "2"].iter().enumerate() {
            std::fs::write(
                &deep,
                format!(
                    "#define DEEP {value}
"
                ),
            )
            .unwrap();
            check(build() == 0, &format!("build {} failed", i + 2))?;
            let expected = 4 + 2 * i as u64;
            check(server.stats().executions == expected, &format!("after editing deep.h: expected {expected} remote executions, got {:?}", server.stats()))?;
        }
        let loc = ninja_daemon::Location::for_exe(&std::env::current_exe().unwrap()).unwrap();
        let status = ninja_daemon::Client::connect(&loc).and_then(|c| c.status()).map_err(|e| format!("daemon status: {e}"))?;
        check(status.builds == 3, &format!("expected 3 builds, got {status:?}"))?;
        check(status.digests > 0 && status.trees > 0, &format!("nothing kept: {status:?}"))?;
        let root = std::fs::canonicalize(&f.root).unwrap();
        let watched = status.roots.iter().any(|(p, s)| std::fs::canonicalize(p).is_ok_and(|p| p == root) && s == "watched");
        check(watched, &format!("the fixture isn't watched: {:?}", status.roots))?;
        Ok::<(), String>(())
    });
    ninja_rs::daemon::tool(&["stop".to_string()]);
    unsafe {
        std::env::remove_var("NINJA_RS_DAEMON_DIR");
        std::env::remove_var("NINJA_RS_DAEMON_IDLE");
    }
    r
}

fn test_without_executor_runs_locally() -> Result<(), String> {
    let f = fixture("noexec", true);
    check(run_ninja(&f, None, &[]) == 0, "local build failed")?;
    check(f.build.join("app.bin").exists(), "app.bin missing")?;
    let _ = &f.compiler;
    Ok(())
}

fn main() -> ExitCode {
    let exe = std::env::current_exe().unwrap();
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem.starts_with("fake-clang") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_clang(&args) as u8);
    }
    if stem.starts_with("fake-ar") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_ar(&args) as u8);
    }
    if stem.starts_with("fake-cmake") {
        let args: Vec<String> = std::env::args().skip(1).collect();
        return ExitCode::from(fake_cmake(&args) as u8);
    }
    // The daemon test_daemon's builds start.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() >= 3 && args[..3] == ["-t", "daemon", "run"] {
        return ExitCode::from(ninja_rs::daemon::tool(&args[2..]) as u8);
    }
    let tests: Vec<(&str, fn() -> Result<(), String>)> = vec![
        ("remote_build_and_cache", test_remote_build_and_cache),
        ("local_tool_steps", test_local_tool_steps),
        ("no_rbe_version_runs_locally", test_no_rbe_version_runs_locally),
        ("build_events", test_build_events),
        ("disk_cache_alone", test_disk_cache_alone),
        ("disk_cache_with_remote", test_disk_cache_with_remote),
        ("remote_cache_only", test_remote_cache_only),
        ("fallback_guards", test_fallback_guards),
        ("racing", test_racing),
        ("without_executor_runs_locally", test_without_executor_runs_locally),
        ("minimal_downloads", test_minimal_downloads),
        ("action_rewinding", test_action_rewinding),
        ("disk_cache_minimal", test_disk_cache_minimal),
        ("compression", test_compression),
        ("thin_archives", test_thin_archives),
        ("mtls", test_mtls),
        ("include_scanner_fallback", test_include_scanner_fallback),
        ("daemon", test_daemon),
        ("split_and_splice_v2_12", || test_split_and_splice(12)),
        ("split_and_splice_v2_13", || test_split_and_splice(13)),
    ];
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
