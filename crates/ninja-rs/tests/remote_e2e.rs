//! End-to-end test of remote execution against the in-process fake REAPI
//! server, with a hand-written build.ninja following the CMake interface.
//!
//! The "compiler" is this test binary itself: when it runs under the name
//! `fake-clang` it behaves like a tiny C compiler (follows `#include "..."`,
//! writes a depfile, answers `-E -v` with an include search list). This keeps
//! the test hermetic on every host.

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
            data.push_str(&std::fs::read_to_string(i).unwrap_or_default());
        }
        std::fs::write(&out, data).unwrap();
        return 0;
    }
    let src = src.expect("-c");
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
    std::fs::write(&out, text).unwrap();
    println!("fake-clang: compiled {src}");
    0
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
    write(&root.join("src/main.c"), "#include \"lib.h\"\nint main() { return lib(); }\n");
    write(&root.join("src/lib.c"), "#include \"lib.h\"\nint lib() { return DEEP; }\n");
    write(&root.join("include/lib.h"), "#include \"nested/deep.h\"\nint lib();\n");
    write(&root.join("include/nested/deep.h"), "#define DEEP 0\n");
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let tc_json = format!(
        "{{\"version\": 1, \"toolchains\": [{{\"language\": \"C\", \"compiler\": \"{}\", \"inputs\": [\"{}\"], \"platform\": {{\"OSFamily\": \"{}\"}}}}]}}",
        fwd(&compiler),
        fwd(&root.join("tc")),
        if cfg!(windows) { "Windows" } else { "Linux" }
    );
    write(&build.join("CMakeFiles/rbe_toolchains.json"), &tc_json);
    let cc = compiler.to_string_lossy().to_string();
    let inc = root.join("include").to_string_lossy().to_string();
    let header = if rbe { "cmake_rbe = 1\ncmake_rbe_toolchains = CMakeFiles/rbe_toolchains.json\n" } else { "" };
    let manifest = format!(
        "{header}\
rule CC\n  command = {cc} -I{inc} -MD -MF $DEP_FILE $FLAGS -c $in -o $out\n  depfile = $DEP_FILE\n  deps = gcc\n  description = CC $out\n\
rule LINK\n  command = {cc} --link $in -o $out\n  description = LINK $out\n\
build obj/main.o: CC {main}\n  DEP_FILE = obj/main.o.d\n  cmake_target = app\n  mnemonic = CCompile\n\
build obj/lib.o: CC {lib}\n  DEP_FILE = obj/lib.o.d\n  cmake_target = app\n  mnemonic = CCompile\n\
build obj/local.o: CC {lib}\n  DEP_FILE = obj/local.o.d\n  cmake_target = app\n  mnemonic = CCompile\n  local = 1\n\
build app.bin: LINK obj/main.o obj/lib.o obj/local.o\n  cmake_target = app\n  mnemonic = CLink\n  deliverable = 1\n\
build obj/bad.o: CC {lib}\n  DEP_FILE = obj/bad.o.d\n  FLAGS = -DFAIL\n  mnemonic = CCompile\n\
build bad: phony obj/bad.o\n\
default app.bin\n",
        main = ninja_escape_path(&root.join("src/main.c").to_string_lossy()),
        lib = ninja_escape_path(&root.join("src/lib.c").to_string_lossy()),
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

fn test_no_cmake_rbe_runs_locally() -> Result<(), String> {
    let f = fixture("local", false);
    with_server(&f, |server| {
        check(run_ninja(&f, Some(&server.endpoint), &[]) == 0, "local build failed")?;
        check(server.stats().executions == 0, "nothing may run remotely without cmake_rbe = 1")?;
        check(f.build.join("app.bin").exists(), "app.bin missing")?;
        Ok(())
    })
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
    let tests: Vec<(&str, fn() -> Result<(), String>)> = vec![
        ("remote_build_and_cache", test_remote_build_and_cache),
        ("no_cmake_rbe_runs_locally", test_no_cmake_rbe_runs_locally),
        ("without_executor_runs_locally", test_without_executor_runs_locally),
    ];
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
    println!("test result: {} passed; {failed} failed", 3 - failed);
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
