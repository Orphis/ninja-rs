//! End-to-end test of remote execution against the in-process fake REAPI
//! server, with a hand-written build.ninja following the RBE interface.
//!
//! The "compiler" is this test binary itself: when it runs under the name
//! `fake-clang` it behaves like a tiny C compiler (follows `#include "..."`,
//! writes a depfile, answers `-E -v` with an include search list, and `-###`
//! with the job of a linker next to it, `ld.lld`, which never runs). Under
//! the name `fake-cmake` it deletes, touches and appends to files (`-E rm
//! -f`, `-E touch`, `-E append`), a tool declared local. Under the name
//! `fake-ar` it writes thin archives (`qcT`), which `fake-clang --link`
//! reads. This keeps the test hermetic on every host.
//!
//! Builds run the ninja-rs binary in the fixture (`-C`), with the
//! environment the test gives it: the tests change neither this process's
//! directory nor its environment, which the fake server's threads share.

mod harness;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fake_reapi::FakeServer;
use harness::ninja_exe;
use ninja_core::subprocess::Interrupt;
use ninja_rs::Service;
use ninja_rs::options::{ParseOutcome, parse_args};

/// Counts the bytes allocated, for the tests bounding a build's memory.
struct Counting;

static ALLOCATED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Counting {
    fn grew(by: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        PEAK.fetch_max(ALLOCATED.fetch_add(by, Relaxed) + by, Relaxed);
    }
}

// SAFETY: the system allocator does the work; this only counts.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller's layout, as `GlobalAlloc::alloc` requires.
        let p = unsafe { std::alloc::System.alloc(layout) };
        if !p.is_null() {
            Self::grew(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: `p` came from `alloc` or `realloc` with this layout.
        unsafe { std::alloc::System.dealloc(p, layout) };
        ALLOCATED.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `dealloc`, with the caller's new size.
        let q = unsafe { std::alloc::System.realloc(p, layout, new_size) };
        if !q.is_null() {
            if new_size > layout.size() {
                Self::grew(new_size - layout.size());
            } else {
                ALLOCATED.fetch_sub(layout.size() - new_size, std::sync::atomic::Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// What `f` returns, and the most bytes allocated at once while it ran
/// beyond those allocated before.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let base = ALLOCATED.load(Relaxed);
    PEAK.store(base, Relaxed);
    let r = f();
    (r, PEAK.load(Relaxed).saturating_sub(base))
}

// ---------------------------------------------------------------------------
// fake-clang
// ---------------------------------------------------------------------------

fn fake_clang(args: &[String]) -> i32 {
    // Response files (`@file`, whitespace-separated arguments).
    let args: Vec<String> = args
        .iter()
        .flat_map(|a| match a.strip_prefix('@') {
            Some(f) => std::fs::read_to_string(f).unwrap_or_default().split_whitespace().map(str::to_string).collect(),
            None => vec![a.clone()],
        })
        .collect();
    let args = &args;
    let mut include_dirs: Vec<PathBuf> = Vec::new();
    let mut src = None;
    let mut out = None;
    let mut depfile = None;
    let mut link = false;
    let mut dry_run = false;
    let mut split_dwarf = false;
    let mut repeat = 1;
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
            // The driver's dry run: what it would run, nothing written.
            "-###" => dry_run = true,
            "-gsplit-dwarf" => split_dwarf = true,
            // A large output from a small source.
            _ if a.starts_with("-DREPEAT=") => repeat = a["-DREPEAT=".len()..].parse().unwrap_or(1),
            // A precompiled header (CMake's with Clang) must be there.
            "-Xclang" if args.get(i).is_some_and(|n| n == "-include-pch") && args.get(i + 1).is_some_and(|n| n == "-Xclang") => {
                if let Some(pch) = args.get(i + 2)
                    && !Path::new(pch).exists()
                {
                    eprintln!("fake-clang: error: precompiled header {pch} not found");
                    return 1;
                }
                i += 3;
            }
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
    if dry_run {
        // The link job, as Clang prints it: quoted, backslashes escaped.
        let linker = std::env::current_exe().unwrap().with_file_name(if cfg!(windows) { "ld.lld.exe" } else { "ld.lld" });
        let quoted = |a: &str| format!(" \"{}\"", a.replace('\\', "\\\\").replace('"', "\\\""));
        let job: String = [linker.to_string_lossy().as_ref(), "-o", &out].into_iter().chain(inputs.iter().map(String::as_str)).map(quoted).collect();
        eprintln!("fake-clang version 1\n{job}");
        return 0;
    }
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
    // see (remote ones run elsewhere), make them slow (saying who's
    // sleeping), or mark their outputs.
    if let Ok(ms) = std::fs::read_to_string("race-local-sleep-ms") {
        let _ = std::fs::create_dir_all("race-local-pids");
        std::fs::write(Path::new("race-local-pids").join(std::process::id().to_string()), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(ms.trim().parse().unwrap_or(0)));
    }
    // A source edited after the build hashed it, before the compile reads
    // it: the file names the source, then its new contents.
    if let Ok(edit) = std::fs::read_to_string("local-edit-source")
        && let Some((path, contents)) = edit.split_once('\n')
    {
        std::fs::write(path, contents).unwrap();
    }
    // The same, edited back once the compile read it.
    let edited_back = std::fs::read_to_string("local-edit-source-back").ok().and_then(|edit| {
        let (path, contents) = edit.split_once('\n')?;
        let original = std::fs::read(path).unwrap();
        std::fs::write(path, contents).unwrap();
        Some((path.to_string(), original))
    });
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
    if let Some((path, original)) = edited_back {
        std::fs::write(path, original).unwrap();
    }
    // A file only local runs read, which no include names.
    if let Ok(extra) = std::fs::read_to_string("local-extra-dep") {
        text.push_str(&std::fs::read_to_string(&extra).unwrap_or_default());
        deps.push(extra.replace('\\', "/"));
    }
    if let Some(d) = depfile {
        let body: Vec<String> = deps.iter().map(|d| d.replace(' ', "\\ ")).collect();
        std::fs::write(&d, format!("{out}: {}\n", body.join(" "))).unwrap();
    }
    if Path::new("race-local-mark").exists() {
        text.push_str("RACE-LOCAL\n");
    }
    std::fs::write(&out, text.repeat(repeat)).unwrap();
    if split_dwarf {
        std::fs::write(Path::new(&out).with_extension("dwo"), "DWO\n").unwrap();
    }
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
        [e, append, file] if e == "-E" && append == "append" => {
            use std::io::Write as _;
            std::fs::File::options().create(true).append(true).open(file).and_then(|mut f| f.write_all(b"ran\n")).unwrap();
            0
        }
        [e, touch, files @ ..] if e == "-E" && touch == "touch" => {
            for p in files {
                std::fs::write(p, "").unwrap();
            }
            0
        }
        // What a build command gets from the invocation running it.
        [e, env, file] if e == "-E" && env == "environment" => {
            std::fs::write(file, std::env::var(ninja_rs::nested::ENV).unwrap_or_default()).unwrap();
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

/// A checkout and its build directory, in the running test's directory.
fn fixture(name: &str, rbe: bool) -> Fixture {
    let root = harness::test_dir(name);
    let build = root.join("build");
    let compiler = root.join("tc/bin").join(if cfg!(windows) { "fake-clang.exe" } else { "fake-clang" });
    harness::link_self(&compiler);
    let ar = root.join("tc/bin").join(if cfg!(windows) { "fake-ar.exe" } else { "fake-ar" });
    harness::link_self(&ar);
    // The linker `fake-clang -###` names (in the toolchain, never run).
    harness::link_self(&root.join("tc/bin").join(if cfg!(windows) { "ld.lld.exe" } else { "ld.lld" }));
    let cmake = root.join("cmake/bin").join(if cfg!(windows) { "fake-cmake.exe" } else { "fake-cmake" });
    harness::link_self(&cmake);
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

/// The options of a build of the fixture: the executor's, then `extra`.
fn build_args(endpoint: Option<&str>, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(e) = endpoint {
        args.push(format!("--remote_executor={e}"));
        args.push("--remote_header=x-test=1".into());
    }
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

/// Builds in the fixture (ninja-rs, `-C`, without `.ninjars` files).
fn run_ninja(f: &Fixture, endpoint: Option<&str>, extra: &[&str]) -> i32 {
    run_ninja_with(f, endpoint, extra, &[])
}

/// Like `run_ninja`, with variables set in the build's environment.
fn run_ninja_with(f: &Fixture, endpoint: Option<&str>, extra: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> i32 {
    let mut c = ninja_command(f, endpoint, extra);
    c.envs(env.iter().copied());
    c.status().map_or(-1, |s| s.code().unwrap_or(-1))
}

/// Like `run_ninja`, and the most memory the ninja-rs process held at once,
/// its commands' not counted: its peak commit charge on Windows; on Linux
/// its peak resident set (VmHWM), read until it exits (it only grows, and a
/// build's peak comes before its last step ends). None elsewhere.
fn run_ninja_peak(f: &Fixture, endpoint: Option<&str>, extra: &[&str]) -> (i32, Option<usize>) {
    // glibc gives threads arenas of their own, each keeping what it frees
    // for itself: with one, the resident peak is what the build held.
    let one_arena = [("MALLOC_ARENA_MAX", std::ffi::OsStr::new("1"))];
    let mut c = ninja_command(f, endpoint, extra);
    c.envs(one_arena.iter().copied());
    let mut child = c.spawn().expect("starting ninja-rs");
    #[cfg(target_os = "linux")]
    let (status, peak) = {
        let mut peak = None;
        loop {
            let status = std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap_or_default();
            let kib = status.lines().find_map(|l| l.strip_prefix("VmHWM:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<usize>().ok());
            // Gone once it has exited.
            peak = kib.map(|k| k << 10).or(peak);
            if let Some(s) = child.try_wait().unwrap() {
                break (s, peak);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    #[cfg(not(target_os = "linux"))]
    let status = child.wait().unwrap();
    #[cfg(windows)]
    let peak = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        // SAFETY: plain data, all zeros before the call fills it.
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: the process's handle, open while `child` lives; the size
        // of the counters it fills.
        let ok = unsafe { GetProcessMemoryInfo(child.as_raw_handle(), &mut counters, size) } != 0;
        ok.then_some(counters.PeakPagefileUsage)
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let peak = None;
    (status.code().unwrap_or(-1), peak)
}

/// Like `run_ninja`: the exit code, and what the build printed.
fn run_ninja_output(f: &Fixture, endpoint: Option<&str>, extra: &[&str]) -> (i32, String) {
    match ninja_command(f, endpoint, extra).output() {
        Ok(o) => (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
        Err(e) => (-1, e.to_string()),
    }
}

/// A build of the fixture, without what the environment of the test may
/// hold for invocations nested in a build.
fn ninja_command(f: &Fixture, endpoint: Option<&str>, extra: &[&str]) -> std::process::Command {
    let mut c = std::process::Command::new(ninja_exe());
    c.arg("-C").arg(&f.build).arg("--ignore_all_rc_files").args(build_args(endpoint, extra));
    c.env_remove(ninja_rs::nested::ENV)
        .env_remove(ninja_rs::nested::UNREACHABLE_ENV)
        .env_remove("MAKEFLAGS")
        .env_remove("NINJA_STATUS")
        .env_remove("NINJA_RS_DAEMON");
    c
}

/// Builds in the fixture in this process, for the tests measuring what a
/// build allocates; back to the directory it was in after.
fn run_ninja_in_process(f: &Fixture, endpoint: Option<&str>, extra: &[&str]) -> i32 {
    let opts = match parse_args(&build_args(endpoint, extra)).unwrap() {
        ParseOutcome::Run(o) => o,
        ParseOutcome::Exit(..) => panic!("unexpected exit"),
    };
    let before = std::env::current_dir().unwrap();
    std::env::set_current_dir(&f.build).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let code = rt.block_on(async {
        let mut s = Service::new(Interrupt::new());
        s.run(&opts).await
    });
    rt.shutdown_background();
    std::env::set_current_dir(before).unwrap();
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

    // 2. After a clean, the compiles come from the cache. To tell, local
    //    compiles mark their outputs (`race-local-mark`).
    let mark = f.build.join("race-local-mark");
    write(&mark, "");
    check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
    check(run_ninja(&f, None, &[&flag]) == 0, "rebuild from the disk cache failed")?;
    check(std::fs::read(f.build.join("obj/main.o")).ok().as_ref() == Some(&main_o), "obj/main.o was not restored from the disk cache")?;
    // A cached blob altered (same size) isn't restored: it's a miss, and
    // goes.
    let blob = rbe_client::digest_of(&main_o);
    let blob_path = disk.join("cas").join(&blob.hash[..2]).join(&blob.hash);
    let mut altered = main_o.clone();
    altered[0] ^= 0x20;
    std::fs::write(&blob_path, &altered).map_err(|e| e.to_string())?;
    check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
    check(run_ninja(&f, None, &[&flag]) == 0, "rebuild after a corrupt blob failed")?;
    let rebuilt = std::fs::read_to_string(f.build.join("obj/main.o")).unwrap_or_default();
    check(rebuilt.ends_with("RACE-LOCAL\n"), "obj/main.o should have been compiled again")?;
    check(!blob_path.exists(), "the corrupt blob should be gone")?;
    std::fs::remove_file(&mark).map_err(|e| e.to_string())?;
    check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
    check(run_ninja(&f, None, &[&flag]) == 0, "rebuild failed")?;
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

/// Files a compile writes besides its object (split DWARF here) come back
/// from remote executions and from cache hits.
fn test_compile_side_outputs() -> Result<(), String> {
    let f = fixture("sideoutputs", true);
    let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    let changed = manifest.replacen("DEP_FILE = obj/main.o.d\n", "DEP_FILE = obj/main.o.d\n  FLAGS = -gsplit-dwarf\n", 1);
    check(changed != manifest, "the manifest didn't change")?;
    write(&f.build.join("build.ninja"), &changed);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let dwo = f.build.join("obj/main.dwo");
        check(run_ninja(&f, Some(&ep), &["obj/main.o"]) == 0, "remote build failed")?;
        check(server.stats().executions == 1, &format!("expected a remote execution, got {:?}", server.stats()))?;
        check(std::fs::read_to_string(&dwo).is_ok_and(|s| s == "DWO\n"), "obj/main.dwo wasn't returned")?;
        std::fs::remove_file(&dwo).map_err(|e| e.to_string())?;
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        check(run_ninja(&f, Some(&ep), &["obj/main.o"]) == 0, "rebuild failed")?;
        let st = server.stats();
        check(st.executions == 1 && st.action_cache_hits == 1, &format!("expected a cache hit, got {st:?}"))?;
        check(dwo.exists(), "obj/main.dwo wasn't restored from the cache")?;
        Ok(())
    })
}

/// Local results the action's key doesn't describe aren't cached: when
/// the command read a file the action's inputs lack, ran with a variable
/// set that the compiler reads and the action can't carry, or ran steps
/// on the host.
fn test_capture_guards() -> Result<(), String> {
    let f = fixture("captureguards", true);
    let disk = f.root.join("disk-cache");
    let flag = format!("--disk_cache={}", disk.to_string_lossy());
    let objs = ["obj/main.o", "obj/lib.o"];
    let clean = || check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed");
    let entries = || disk_cache_entries(&disk);
    // 1. A file the scan didn't find.
    let secret = f.root.join("secret.h");
    write(&secret, "SECRET\n");
    write(&f.build.join("local-extra-dep"), &secret.to_string_lossy());
    check(run_ninja(&f, None, &[&flag, objs[0], objs[1]]) == 0, "build reading an undeclared file failed")?;
    check(entries() == 0, &format!("results reading an undeclared file were cached: {}", entries()))?;
    std::fs::remove_file(f.build.join("local-extra-dep")).map_err(|e| e.to_string())?;
    clean()?;
    // 2. A variable the compiler reads that the action can't carry (it
    //    writes a file the action doesn't return).
    let deps = std::ffi::OsStr::new("deps.txt");
    check(run_ninja_with(&f, None, &[&flag, objs[0], objs[1]], &[("DEPENDENCIES_OUTPUT", deps)]) == 0, "build with DEPENDENCIES_OUTPUT set failed")?;
    check(entries() == 0, &format!("results with DEPENDENCIES_OUTPUT set were cached: {}", entries()))?;
    clean()?;
    // 3. Steps on the host (removing the archive before, a stamp after):
    //    only the compiles are cached.
    check(run_ninja(&f, None, &[&flag, "--remote_mnemonics=Compile,Archive", "lib.a"]) == 0, "lib.a failed")?;
    check(f.build.join("ar.stamp").exists(), "the archive's host step didn't run")?;
    check(entries() == 2, &format!("expected the 2 compiles cached, got {}", entries()))?;
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
        //    the depfiles map back to its paths. Local compiles would mark
        //    their objects there: none ran.
        let g = fixture("cacheonly-other", true);
        write(&g.build.join("race-local-mark"), "");
        check(run_ninja(&g, None, &[&flag]) == 0, "build in the other checkout failed")?;
        let st = server.stats();
        check(st.executions == 0 && st.action_cache_hits == 2 && st.action_cache_updates == 2, &format!("expected 2 remote cache hits, got {st:?}"))?;
        for o in ["obj/main.o", "obj/lib.o"] {
            let theirs = std::fs::read_to_string(g.build.join(o)).map_err(|e| format!("{o}: {e}"))?;
            check(!theirs.contains("RACE-LOCAL"), &format!("{o} was compiled locally, not taken from the remote cache"))?;
        }
        std::fs::remove_file(g.build.join("race-local-mark")).map_err(|e| e.to_string())?;
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
/// arrives first, and the remote outputs are written. "Slow" is a side held
/// back until the other has won, or for far longer than a build takes:
/// what happened is read from the server's counters and the outputs.
fn test_racing() -> Result<(), String> {
    use std::time::Instant;
    let f = fixture("racing", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let objs = ["obj/main.o", "obj/lib.o"];
        let local_made = |o: &str| std::fs::read_to_string(f.build.join(o)).is_ok_and(|s| s.contains("RACE-LOCAL"));
        let race = ["--remote_racing", "--remote_racing_delay=0", "-j4", objs[0], objs[1]];
        write(&f.build.join("race-local-mark"), "");

        // 1. The remote side doesn't answer before the test gives up: the
        //    local runs win, and the build doesn't wait for it.
        let deadline = Instant::now() + harness::PATIENCE;
        server.state.set_execute_gate(Some(std::sync::Arc::new(move || Instant::now() > deadline)));
        check(run_ninja(&f, Some(&ep), &race) == 0, "racing build failed")?;
        check(server.stats().executions == 0, &format!("the build waited for the remote side: {:?}", server.stats()))?;
        check(objs.iter().all(|o| local_made(o)), "outputs not from the local runs")?;

        // 2. The local side takes longer than the test would wait: the
        //    remote results, held back until both local runs started, win;
        //    the local commands are stopped and don't overwrite them.
        server.state.clear_action_cache();
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        let pids = f.build.join("race-local-pids");
        let _ = std::fs::remove_dir_all(&pids);
        write(&f.build.join("race-local-sleep-ms"), &(10 * harness::PATIENCE.as_millis()).to_string());
        let (started, deadline) = (pids.clone(), Instant::now() + harness::PATIENCE);
        server
            .state
            .set_execute_gate(Some(std::sync::Arc::new(move || std::fs::read_dir(&started).is_ok_and(|d| d.count() == 2) || Instant::now() > deadline)));
        check(run_ninja(&f, Some(&ep), &race) == 0, "racing build failed")?;
        let st = server.stats();
        check(st.executions == 2, &format!("expected the 2 remote executions to run: {st:?}"))?;
        let pids: Vec<u32> = std::fs::read_dir(&pids).map_err(|e| e.to_string())?.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
        check(pids.len() == 2, &format!("expected 2 local runs, got {pids:?}"))?;
        harness::wait_for("the local runs stopped", || !pids.iter().any(|&p| harness::alive(p)))?;
        check(objs.iter().all(|o| f.build.join(o).exists() && !local_made(o)), "outputs not from the remote side")?;
        server.state.set_execute_gate(None);
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

        // 5. Up to date: every target the requested ones need still
        //    completes, with its outputs, as Bazel reports them.
        let target_outcomes = |file: &std::path::Path| -> Result<Vec<(String, bool, usize)>, String> {
            let events = ninja_rs::bes::read_binary_file(file).map_err(|e| e.to_string())?;
            rbe_bes::events::validate(&events)?;
            Ok(events
                .iter()
                .filter_map(|e| match (id_of(e), &e.payload) {
                    (Some(Id::TargetCompleted(t)), Some(Payload::Completed(c))) => {
                        Some((t.label, c.success, c.output_group.iter().map(|g| g.file_sets.len()).sum()))
                    }
                    (Some(Id::TargetCompleted(t)), _) => Some((t.label, false, 0)),
                    _ => None,
                })
                .collect())
        };
        let file4 = f.root.join("events4.bin");
        let flag = format!("--build_event_binary_file={}", file4.to_string_lossy());
        check(run_ninja(&f, Some(&ep), &[&flag, "app.bin", "lib.a"]) == 0, "the no-op build failed")?;
        let outcomes = target_outcomes(&file4)?;
        for label in ["//src:app", "//src:lib"] {
            check(outcomes.contains(&(label.to_string(), true, 1)), &format!("{label} up to date: {outcomes:?}"))?;
        }

        // 6. A restat leaving its output unchanged takes the edges after it
        //    out of the plan: their target completes, it isn't aborted.
        let cmake = f.root.join("cmake/bin").join(if cfg!(windows) { "fake-cmake.exe" } else { "fake-cmake" });
        write(&f.build.join("src.txt"), "source");
        write(&f.build.join("gen.txt"), "generated");
        let mut manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
        manifest.push_str(&format!(
            "rule KEEP\n  command = {} -E touch keep.flag\n  restat = 1\nbuild gen.txt: KEEP src.txt\n\
             build gen.bin: LINK gen.txt\n  rbe_target = gen\n  rbe_mnemonic = CLink\n  rbe_artifact = 1\n",
            cmake.display()
        ));
        write(&f.build.join("build.ninja"), &manifest);
        check(run_ninja(&f, Some(&ep), &["gen.bin"]) == 0, "building gen.bin failed")?;
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(f.build.join("src.txt")).unwrap().set_modified(later).unwrap();
        let file5 = f.root.join("events5.bin");
        let flag = format!("--build_event_binary_file={}", file5.to_string_lossy());
        check(run_ninja(&f, Some(&ep), &[&flag, "gen.bin"]) == 0, "the restat build failed")?;
        let outcomes = target_outcomes(&file5)?;
        check(outcomes == [("//:gen".to_string(), true, 1)], &format!("//:gen after the restat: {outcomes:?}"))?;
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

/// Spliced uploads with zstd: a server compressing ByteStream writes but
/// not batches (EngFlow's) gets each chunk through compressed ByteStream,
/// as Bazel sends them; one compressing batches gets compressed batches.
fn test_splice_compressed() -> Result<(), String> {
    for batches in [false, true] {
        let f = fixture(&format!("splicezstd-{batches}"), true);
        with_server(&f, |server| {
            server.state.set_chunking(Some(12));
            server.state.set_compression(true);
            server.state.set_batch_compression(batches);
            let ep = server.endpoint.clone();
            // About 5 MB: spliced, as ten chunks or so.
            let text: String = (0..160_000)
                .map(|i| {
                    format!(
                        "int v{i} = {i}; /* repetitive */
"
                    )
                })
                .collect();
            write(&f.root.join("src/big.c"), &text);
            check(run_ninja(&f, Some(&ep), &["obj/big.o"]) == 0, "build failed")?;
            let st = server.stats();
            check(st.splices >= 1 && st.compressed_writes >= 8, &format!("expected compressed chunks: {st:?}"))?;
            if batches {
                check(st.bytestream_writes == 0, &format!("expected batches only: {st:?}"))?;
            } else {
                check(st.bytestream_writes >= 8, &format!("expected chunks through ByteStream: {st:?}"))?;
            }
            let big_o = std::fs::read_to_string(f.build.join("obj/big.o")).map_err(|e| e.to_string())?;
            check(
                big_o.ends_with(
                    "int v159999 = 159999; /* repetitive */
",
                ),
                "obj/big.o content",
            )?;
            Ok::<(), String>(())
        })?;
    }
    Ok(())
}

/// The outputs recorded as remote-only.
fn remote_only(f: &Fixture) -> Vec<String> {
    let text = std::fs::read_to_string(f.build.join(".ninja_remote")).unwrap_or_default();
    text.lines().skip(1).filter_map(|l| l.splitn(5, ' ').nth(4)).map(str::to_string).collect()
}

/// A local run of an action whose outputs were left remote (here a local
/// mnemonic after a command change) drops their records: its outputs
/// removed are missing, not the old remote ones again. A build without the
/// remote service drops every record.
fn test_local_run_drops_records() -> Result<(), String> {
    let f = fixture("localrecords", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let minimal = "--remote_download_minimal";
        check(run_ninja(&f, Some(&ep), &[minimal, "--remote_mnemonics=Compile,Link"]) == 0, "first build failed")?;
        check(remote_only(&f).contains(&"obj/main.o".to_string()), &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        // Another command for obj/main.o, run locally (only links go
        // remotely now); local runs mark their outputs.
        let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
        let changed = manifest.replacen("DEP_FILE = obj/main.o.d\n", "DEP_FILE = obj/main.o.d\n  FLAGS = -DCHANGED\n", 1);
        check(changed != manifest, "the manifest wasn't changed")?;
        write(&f.build.join("build.ninja"), &changed);
        write(&f.build.join("race-local-mark"), "");
        let local = [minimal, "--remote_mnemonics=Link", "obj/main.o"];
        check(run_ninja(&f, Some(&ep), &local) == 0, "the local compile failed")?;
        check(!remote_only(&f).contains(&"obj/main.o".to_string()), &format!("obj/main.o still recorded: {:?}", remote_only(&f)))?;
        std::fs::remove_file(f.build.join("obj/main.o")).map_err(|e| e.to_string())?;
        check(run_ninja(&f, Some(&ep), &local) == 0, "the rebuild failed")?;
        let main_o = std::fs::read_to_string(f.build.join("obj/main.o")).map_err(|e| e.to_string())?;
        check(main_o.contains("RACE-LOCAL"), "obj/main.o came back from the old remote record")?;
        let _ = std::fs::remove_file(f.build.join("race-local-mark"));
        check(run_ninja(&f, None, &["obj/main.o"]) == 0, "the local build failed")?;
        check(!f.build.join(".ninja_remote").exists(), "records survived a build without the remote service")
    })
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

/// Local commands needing different remote-only inputs download them at
/// once, not one after the other.
fn test_parallel_materialization() -> Result<(), String> {
    let f = fixture("materialize", true);
    let mut manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    manifest.push_str("build copy_main.o: COPY obj/main.o\n");
    write(&f.build.join("build.ninja"), &manifest);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flags = ["--remote_download_minimal", "--remote_mnemonics=Compile,Link"];
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        check(remote_only(&f) == ["obj/lib.o", "obj/main.o"], &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        server.state.set_read_delay(std::time::Duration::from_millis(500));
        check(run_ninja(&f, Some(&ep), &[flags[0], flags[1], "-j4", "copy.o", "copy_main.o"]) == 0, "the copies failed")?;
        check(server.stats().max_concurrent_reads >= 2, &format!("the inputs were downloaded one after the other: {:?}", server.stats()))?;
        check(remote_only(&f).is_empty(), &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        let main_o = std::fs::read(f.build.join("obj/main.o")).map_err(|e| format!("obj/main.o: {e}"))?;
        check(std::fs::read(f.build.join("copy_main.o")).ok() == Some(main_o), "copy_main.o content")
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

/// A file the include scan finds that only exists remotely (a precompiled
/// header in `-Xclang -include-pch`, as CMake writes with Clang, left
/// remote by minimal downloads) goes in by digest, as declared inputs do.
fn test_remote_only_scanned_input() -> Result<(), String> {
    let f = fixture("pch", true);
    let mut manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    manifest.push_str(&format!(
        "build obj/pch.pch: CC {lib}
  DEP_FILE = obj/pch.pch.d
  rbe_mnemonic = CCompile
build obj/user.o: CC {main} | obj/pch.pch
  DEP_FILE = obj/user.o.d
  FLAGS = -Xclang -include-pch -Xclang obj/pch.pch
  rbe_mnemonic = CCompile
",
        lib = ninja_escape_path(&f.root.join("src/lib.c").to_string_lossy()),
        main = ninja_escape_path(&f.root.join("src/main.c").to_string_lossy()),
    ));
    write(&f.build.join("build.ninja"), &manifest);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_strict", "obj/user.o"]) == 0, "build failed")?;
        check(server.stats().executions == 2, &format!("expected 2 remote executions, got {:?}", server.stats()))?;
        check(!f.build.join("obj/pch.pch").exists(), "the precompiled header was downloaded")?;
        Ok(())
    })
}

/// A build command running ninja-rs again (LLVM's NATIVE tools,
/// ExternalProject): the nested build runs remotely with the outer one's
/// settings, under its invocation id, its events join the outer stream
/// and its actions the outer execution log.
fn test_nested_invocation() -> Result<(), String> {
    use rbe_bes::proto::build_event_stream::build_event::Payload;
    use rbe_bes::proto::build_event_stream::build_event_id::Id;
    let f = fixture("nested", true);
    let inner = f.build.join("inner");
    for file in ["CMakeFiles/rbe_toolchains.json", "CMakeFiles/rbe_targets.json"] {
        write(&inner.join(file), &std::fs::read_to_string(f.build.join(file)).unwrap());
    }
    let (cc, inc) = (f.compiler.to_string_lossy(), f.root.join("include").to_string_lossy().to_string());
    write(
        &inner.join("build.ninja"),
        &format!(
            "rbe_version = 1
rbe_toolchains = CMakeFiles/rbe_toolchains.json
rbe_targets = CMakeFiles/rbe_targets.json
rule CC
  command = {cc} -I{inc} -MD -MF $out.d -c $in -o $out
  depfile = $out.d
  deps = gcc
build obj/lib.o: CC {lib}
  rbe_target = lib
  rbe_mnemonic = CCompile
  rbe_artifact = 1
build obj/main.o: CC {main}
  rbe_target = app
  rbe_mnemonic = CCompile
  rbe_artifact = 1
",
            lib = ninja_escape_path(&f.root.join("src/lib.c").to_string_lossy()),
            main = ninja_escape_path(&f.root.join("src/main.c").to_string_lossy())
        ),
    );
    let exe = ninja_exe();
    write(
        &f.build.join("build.ninja"),
        &format!(
            "rule NESTED
  command = \"{}\" -C inner $target
rule ENV
  command = \"{}\" -E environment $out

build inner/obj/lib.o: NESTED
  target = obj/lib.o
build inner/obj/main.o: NESTED || inner/obj/lib.o
  target = obj/main.o
build env.txt: ENV
",
            exe.display(),
            f.root.join("cmake/bin").join(if cfg!(windows) { "fake-cmake.exe" } else { "fake-cmake" }).display()
        ),
    );
    let id = "4a1b2c3d-0000-4000-8000-000000000001";
    with_server(&f, |server| {
        let (bep, log) = (f.root.join("events.bep"), f.root.join("exec.jsonl"));
        let out = std::process::Command::new(&exe)
            .current_dir(&f.build)
            .env_remove(ninja_rs::nested::ENV)
            .args([
                format!("--remote_executor={}", server.endpoint),
                format!("--invocation_id={id}"),
                format!("--build_event_binary_file={}", bep.display()),
                format!("--execution_log_file={}", log.display()),
                "--remote_header=x-secret=TOKEN-123".to_string(),
            ])
            .output()
            .map_err(|e| e.to_string())?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        check(
            out.status.success(),
            &format!(
                "the build failed:
{text}"
            ),
        )?;
        // The nested build sends the header, which the outer one didn't put
        // in its commands' environment (nor in its build events, below).
        let headers = server.state.headers.lock().unwrap().clone();
        check(headers.contains(&("x-secret".into(), "TOKEN-123".into())), &format!("the nested build's headers: {headers:?}"))?;
        let env = std::fs::read_to_string(f.build.join("env.txt")).map_err(|e| e.to_string())?;
        check(env.contains("secret_flags") && !env.contains("TOKEN-123"), &format!("NINJA_RS_PARENT: {env}"))?;
        check(
            server.stats().executions == 2,
            &format!(
                "expected the nested compiles remotely: {:?}
{text}",
                server.stats()
            ),
        )?;
        let ids: Vec<String> = server.state.request_metadata.lock().unwrap().iter().map(|m| m.tool_invocation_id.clone()).collect();
        check(!ids.is_empty() && ids.iter().all(|i| i == id), &format!("executions not under the outer invocation: {ids:?}"))?;
        let events = ninja_rs::bes::read_binary_file(&bep).map_err(|e| e.to_string())?;
        rbe_bes::events::validate(&events)?;
        check(!format!("{events:?}").contains("TOKEN-123"), "a header value is in the build events")?;
        let nested = events.iter().any(|e| match (e.id.as_ref().and_then(|i| i.id.as_ref()), &e.payload) {
            (Some(Id::TargetCompleted(t)), Some(Payload::Completed(c))) => {
                t.label == "//src:lib" && t.configuration.as_ref().is_some_and(|c| c.id.ends_with("-inner")) && c.success
            }
            _ => false,
        });
        check(nested, "the nested build's target isn't in the outer stream")?;
        // Two nested builds in one directory each number their file sets
        // from 0: each target still gets its own files.
        let mut sets: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        let mut completed: Vec<(String, Vec<String>)> = Vec::new();
        for e in &events {
            match (e.id.as_ref().and_then(|i| i.id.as_ref()), &e.payload) {
                (Some(Id::NamedSet(s)), Some(Payload::NamedSetOfFiles(files))) => {
                    sets.insert(s.id.clone(), files.files.iter().map(|f| format!("{}/{}", f.path_prefix.join("/"), f.name)).collect());
                }
                (Some(Id::TargetCompleted(t)), Some(Payload::Completed(c))) => {
                    completed.push((t.label.clone(), c.output_group.iter().flat_map(|g| g.file_sets.iter().map(|s| s.id.clone())).collect()));
                }
                _ => {}
            }
        }
        for (label, file) in [("//src:lib", "inner/obj/lib.o"), ("//src:app", "inner/obj/main.o")] {
            let files: Vec<&String> = completed.iter().filter(|(l, _)| l == label).flat_map(|(_, s)| s.iter()).filter_map(|s| sets.get(s)).flatten().collect();
            check(files.len() == 1 && files[0].ends_with(file), &format!("{label}'s files: {files:?} (sets {sets:?}, completed {completed:?})"))?;
        }
        let lines = std::fs::read_to_string(&log).map_err(|e| e.to_string())?.lines().count();
        check(lines == 2, &format!("expected the nested actions in the execution log, got {lines} lines"))?;
        Ok(())
    })
}

/// `-t exec`: the builds a command runs (a configure building external
/// projects) are one invocation, with the options of the `.ninjars` file.
fn test_exec() -> Result<(), String> {
    use rbe_bes::proto::build_event_stream::build_event::Payload;
    use rbe_bes::proto::build_event_stream::build_event_id::Id;
    let f = fixture("exec", true);
    let inner = f.build.join("inner");
    for file in ["CMakeFiles/rbe_toolchains.json", "CMakeFiles/rbe_targets.json"] {
        write(&inner.join(file), &std::fs::read_to_string(f.build.join(file)).unwrap());
    }
    let (cc, inc) = (f.compiler.to_string_lossy(), f.root.join("include").to_string_lossy().to_string());
    write(
        &inner.join("build.ninja"),
        &format!(
            "rbe_version = 1
rbe_toolchains = CMakeFiles/rbe_toolchains.json
rbe_targets = CMakeFiles/rbe_targets.json
rule CC
  command = {cc} -I{inc} -MD -MF $out.d -c $in -o $out
  depfile = $out.d
  deps = gcc
build obj/lib.o: CC {lib}
  rbe_target = lib
  rbe_mnemonic = CCompile
",
            lib = ninja_escape_path(&f.root.join("src/lib.c").to_string_lossy())
        ),
    );
    let exe = ninja_exe();
    let id = "4a1b2c3d-0000-4000-8000-000000000002";
    with_server(&f, |server| {
        let (bep, log) = (f.root.join("events.bep"), f.root.join("exec.jsonl"));
        // The remote settings from the workspace's file, as a project keeps them.
        write(&f.build.join(".ninjars"), &format!("build --remote_executor={}\nexec --invocation_id={id}\n", server.endpoint));
        let out = std::process::Command::new(&exe)
            .current_dir(&f.build)
            .env_remove(ninja_rs::nested::ENV)
            .args(["--nosystem_rc", "--nohome_rc"])
            .arg(format!("--build_event_binary_file={}", bep.display()))
            .arg(format!("--execution_log_file={}", log.display()))
            .args(["-t", "exec", "--"])
            .arg(&exe)
            .args(["-C", "inner", "obj/lib.o"])
            .output()
            .map_err(|e| e.to_string())?;
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        check(out.status.success(), &format!("the command failed:\n{text}"))?;
        check(server.stats().executions == 1, &format!("expected the build's compile remotely: {:?}\n{text}", server.stats()))?;
        let ids: Vec<String> = server.state.request_metadata.lock().unwrap().iter().map(|m| m.tool_invocation_id.clone()).collect();
        check(!ids.is_empty() && ids.iter().all(|i| i == id), &format!("executions not under the exec invocation: {ids:?}"))?;
        let events = ninja_rs::bes::read_binary_file(&bep).map_err(|e| e.to_string())?;
        rbe_bes::events::validate(&events)?;
        let mut started = false;
        let mut target = false;
        let mut lines = Vec::new();
        for e in &events {
            match (e.id.as_ref().and_then(|i| i.id.as_ref()), &e.payload) {
                (_, Some(Payload::Started(s))) => started = s.command == "exec" && s.uuid == id,
                (Some(Id::TargetCompleted(t)), Some(Payload::Completed(c))) => {
                    target |= t.label == "//src:lib" && t.configuration.as_ref().is_some_and(|c| c.id.ends_with("-inner")) && c.success
                }
                (_, Some(Payload::StructuredCommandLine(c))) => lines.push(c.clone()),
                _ => {}
            }
        }
        check(started, "no exec BuildStarted with the invocation id")?;
        check(target, "the build's target isn't in the exec invocation's stream")?;
        // The file's options are in the canonical command line only.
        let has = |label: &str| lines.iter().filter(|c| c.command_line_label == label).any(|c| format!("{c:?}").contains("--remote_executor="));
        check(has("canonical") && !has("original"), "the file's options should be in the canonical command line only")?;
        let lines = std::fs::read_to_string(&log).map_err(|e| e.to_string())?.lines().count();
        check(lines == 1, &format!("expected the build's action in the execution log, got {lines} lines"))?;
        Ok(())
    })
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

        // Not unreachable, with --remote_local_fallback too: the build fails,
        // saying why, rather than build locally.
        let der = f.root.join("ca.der");
        std::fs::write(&der, [0x30, 0x82, 0x01, 0x0a, 0x02, 0x82]).map_err(|e| e.to_string())?;
        let der = format!("--tls_certificate={}", der.display());
        let (plain, misspelt) = (format!("grpc://localhost:{port}"), format!("htps://localhost:{port}"));
        for (what, endpoint, flags, said) in [
            ("an untrusted server", &ep, vec![cert.as_str(), &key], "the server's certificate isn't trusted"),
            // As an alert, or a connection closed (TLS 1.3, the server
            // checking it once the client finished its handshake).
            ("a refused client certificate", &ep, vec![&ca, &other_cert, &other_key], "client's certificate"),
            ("a CA in DER", &ep, vec![&der], "no PEM certificate"),
            ("plaintext to a TLS server", &plain, vec![], "remote execution: GetCapabilities"),
            ("a misspelt scheme", &misspelt, vec![], "unsupported endpoint scheme"),
        ] {
            run_ninja(&f, None, &["-t", "clean"]);
            let (code, out) = run_ninja_output(&f, Some(endpoint), &flags);
            check(code == 1 && out.contains(said) && !out.contains("building locally"), &format!("a build with {what} exited with {code}:\n{out}"))?;
        }
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

/// Builds through the daemon (started by the first): an edited header, same
/// size, must reach the remote actions although the daemon vouched for it
/// after the second build.
fn test_daemon() -> Result<(), String> {
    let f = fixture("daemon", true);
    // A repository: watched.
    std::fs::create_dir_all(f.root.join(".git")).unwrap();
    let dir = f.root.join("daemon");
    let env = [("NINJA_RS_DAEMON_DIR", dir.as_os_str()), ("NINJA_RS_DAEMON_IDLE", std::ffi::OsStr::new("120"))];
    let r = with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let build = || run_ninja_with(&f, Some(&ep), &["--daemon", "--remote_include_scanner=preprocessor", "--remote_strict"], &env);
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
        // The binary's daemon, as it names its path (Bazel's runfile is a
        // link on Unix).
        let exe = if cfg!(unix) { std::fs::canonicalize(ninja_exe()).unwrap() } else { ninja_exe() };
        let loc = ninja_daemon::Location { dir: dir.clone(), ..ninja_daemon::Location::for_exe(&exe).unwrap() };
        let status = ninja_daemon::Client::connect(&loc).and_then(|c| c.status()).map_err(|e| format!("daemon status: {e}"))?;
        check(status.builds == 3, &format!("expected 3 builds, got {status:?}"))?;
        check(status.digests > 0 && status.trees > 0, &format!("nothing kept: {status:?}"))?;
        let root = std::fs::canonicalize(&f.root).unwrap();
        let watched = status.roots.iter().any(|(p, s)| std::fs::canonicalize(p).is_ok_and(|p| p == root) && s == "watched");
        check(watched, &format!("the fixture isn't watched: {:?}", status.roots))?;
        Ok::<(), String>(())
    });
    let _ = std::process::Command::new(ninja_exe()).args(["-t", "daemon", "stop"]).envs(env).status();
    r
}

/// Results a server (or a poisoned cache) returns that can't be used:
/// outputs it wasn't asked for, outside the build directory, a declared
/// output missing or as a symlink, a corrupt blob. Nothing is written from
/// them; the action runs locally instead, with the right output.
/// Calls failing for a transient reason (UNAVAILABLE) are made again, up
/// to --remote_retries times; a broken Execute stream is followed again
/// (WaitExecution). With --remote_strict, a failure that stands fails.
fn test_transient_failures() -> Result<(), String> {
    let f = fixture("transient", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let s = &server.state;
        for (method, times) in [("GetCapabilities", 1), ("FindMissingBlobs", 2), ("BatchUpdateBlobs", 1), ("Write", 1), ("GetActionResult", 1), ("Execute", 1)]
        {
            s.fail_next(method, times);
        }
        s.break_next_executions(1);
        check(run_ninja(&f, Some(&ep), &["--remote_strict", "obj/main.o", "obj/lib.o"]) == 0, "the build failed")?;
        let stats = server.stats();
        check(stats.injected_failures == 7 && stats.wait_executions == 1 && stats.executions == 2, &format!("{stats:?}"))?;
        // Without retries, the failure stands.
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        s.clear_action_cache();
        s.fail_next("Execute", 1);
        check(run_ninja(&f, Some(&ep), &["--remote_strict", "--remote_retries=0", "obj/main.o"]) == 1, "the build should fail without retries")?;
        Ok(())
    })
}

/// A remote service nothing answers for at the start: with
/// --remote_local_fallback (the default), the build runs locally; with
/// --remote_strict or --noremote_local_fallback, it fails.
fn test_unreachable_remote() -> Result<(), String> {
    let f = fixture("unreachable", true);
    // A port nothing listens on, which no test's server gets (they get
    // ephemeral ones; Unix keeps it to root).
    let ep = "grpc://127.0.0.1:1".to_string();
    for strict in ["--remote_strict", "--noremote_local_fallback"] {
        check(run_ninja(&f, Some(&ep), &[strict]) == 1, &format!("a build with {strict} didn't fail"))?;
    }
    check(run_ninja(&f, Some(&ep), &[]) == 0, "the build didn't run locally")?;
    let app = std::fs::read_to_string(f.build.join("app.bin")).map_err(|e| e.to_string())?;
    check(app.starts_with("LINKED"), &format!("app.bin: {app}"))?;
    // A service saying it's unavailable (a proxy without a healthy
    // backend) once the retries are spent is as good as unreachable.
    with_server(&f, |server| {
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        server.state.fail_next("GetCapabilities", 2);
        check(run_ninja(&f, Some(&server.endpoint), &["--remote_strict", "--remote_retries=1"]) == 1, "a build with --remote_strict didn't fail")?;
        server.state.fail_next("GetCapabilities", 2);
        let (code, out) = run_ninja_output(&f, Some(&server.endpoint), &["--remote_retries=1"]);
        check(code == 0 && out.contains("is unreachable, building locally"), &format!("no fallback:\n{out}"))?;
        check(server.stats().executions == 0, "actions ran remotely")
    })
}

/// A remote service accepting connections and never answering anything (a
/// hung one): its endpoint, and the connections it accepted. Stops when
/// dropped.
struct SilentServer {
    endpoint: String,
    accepted: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    addr: std::net::SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SilentServer {
    fn start() -> SilentServer {
        use std::sync::atomic::Ordering::SeqCst;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted, stop) = (std::sync::Arc::<std::sync::atomic::AtomicUsize>::default(), std::sync::Arc::<std::sync::atomic::AtomicBool>::default());
        let (counted, stopping) = (accepted.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for c in listener.incoming() {
                if stopping.load(SeqCst) {
                    break;
                }
                counted.fetch_add(1, SeqCst);
                held.push(c);
            }
        });
        SilentServer { endpoint: format!("grpc://{addr}"), accepted, addr, stop }
    }
}

impl Drop for SilentServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(self.addr);
    }
}

/// A remote service that never answers its first call (GetCapabilities):
/// the build runs locally after one --remote_timeout, without the records
/// of remote outputs, with the disk cache; the invocations its commands run
/// do so too, without trying the service again.
fn test_unreachable_nested() -> Result<(), String> {
    let f = fixture("unreachable_nested", true);
    let inner = f.build.join("inner");
    for file in ["CMakeFiles/rbe_toolchains.json", "CMakeFiles/rbe_targets.json"] {
        write(&inner.join(file), &std::fs::read_to_string(f.build.join(file)).unwrap());
    }
    let (cc, inc) = (f.compiler.to_string_lossy(), f.root.join("include").to_string_lossy().to_string());
    write(
        &inner.join("build.ninja"),
        &format!(
            "rbe_version = 1\nrbe_toolchains = CMakeFiles/rbe_toolchains.json\nrbe_targets = CMakeFiles/rbe_targets.json\n\
             rule CC\n  command = {cc} -I{inc} -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n\
             build obj/lib.o: CC {lib}\n  rbe_mnemonic = CCompile\nbuild obj/main.o: CC {main}\n  rbe_mnemonic = CCompile\n",
            lib = ninja_escape_path(&f.root.join("src/lib.c").to_string_lossy()),
            main = ninja_escape_path(&f.root.join("src/main.c").to_string_lossy())
        ),
    );
    write(
        &f.build.join("build.ninja"),
        &format!(
            "rbe_version = 1\nrbe_toolchains = CMakeFiles/rbe_toolchains.json\nrbe_targets = CMakeFiles/rbe_targets.json\n\
             rule NESTED\n  command = \"{}\" -C inner $target\n\
             build inner/obj/lib.o: NESTED\n  target = obj/lib.o\nbuild inner/obj/main.o: NESTED || inner/obj/lib.o\n  target = obj/main.o\n",
            ninja_exe().display()
        ),
    );
    let records = f.build.join(ninja_remote::remote_outputs::REMOTE_OUTPUTS_FILE);
    write(&records, "left by an earlier build\n");
    let disk = f.root.join("disk-cache");
    let server = SilentServer::start();
    let flags = [format!("--disk_cache={}", disk.display()), "--remote_timeout=2s".to_string()];
    let (code, out) = run_ninja_output(&f, Some(&server.endpoint), &[&flags[0], &flags[1]]);
    check(code == 0, &format!("the build failed:\n{out}"))?;
    check(out.contains(&format!("the remote service {} is unreachable, building locally", server.endpoint)), &format!("no fallback:\n{out}"))?;
    // The nested builds took the outer one's word for it.
    check(out.matches("the invocation running this one found it unreachable").count() == 2, &format!("nested builds:\n{out}"))?;
    let accepted = server.accepted.load(std::sync::atomic::Ordering::SeqCst);
    check(accepted == 1, &format!("{accepted} connections to the remote service:\n{out}"))?;
    check(inner.join("obj/main.o").exists() && inner.join("obj/lib.o").exists(), "the nested builds' objects")?;
    check(!records.exists(), "the records of remote outputs outlived the local build")?;
    check(disk_cache_entries(&disk) == 2, &format!("expected the 2 compiles in the disk cache, got {}", disk_cache_entries(&disk)))?;
    Ok(())
}

/// Large blobs one call uploads go through ByteStream several at once.
fn test_parallel_uploads() -> Result<(), String> {
    let f = fixture("parallel_uploads", true);
    with_server(&f, |server| {
        server.state.set_write_delay(std::time::Duration::from_millis(300));
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let opts = rbe_client::ConnectOptions {
                endpoint: server.endpoint.clone(),
                instance_name: String::new(),
                client_cert_pem: None,
                client_key_pem: None,
                ca_pem: None,
                authority_override: None,
                headers: Vec::new(),
            };
            let mut client = rbe_client::Client::connect(opts).await.map_err(|e| format!("{e:#}"))?;
            client.capabilities().await.map_err(|e| format!("{e:#}"))?;
            let mut blobs = rbe_client::BlobSet::default();
            for i in 0..8u8 {
                let data = vec![i; 4 << 20];
                blobs.insert(rbe_client::digest_of(&data), rbe_client::BlobSource::Bytes(data));
            }
            client.upload_blobs(&blobs).await.map_err(|e| format!("{e:#}"))?;
            let st = server.stats();
            check(st.bytestream_writes == 8 && st.max_concurrent_writes >= 4, &format!("the blobs went one at a time: {st:?}"))
        })
    })
}

/// Large outputs are written as they arrive, never whole in memory: from a
/// remote execution (compressed), from the remote cache, and from the disk
/// cache; a damaged copy in the disk cache isn't restored.
fn test_large_outputs() -> Result<(), String> {
    let f = fixture("largeoutputs", true);
    with_server(&f, |server| {
        server.state.set_compression(true);
        let ep = server.endpoint.clone();
        let disk = f.root.join("disk-cache");
        let flag = format!("--disk_cache={}", disk.to_string_lossy());
        // A small source, a large object: the memory the build uses is
        // the download's.
        let lines = noise_lines(2_000, 11);
        write(&f.root.join("src/big.c"), &format!("{}\n", lines.join("\n")));
        let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
        let changed = manifest.replacen("DEP_FILE = obj/big.o.d\n", "DEP_FILE = obj/big.o.d\n  FLAGS = -DREPEAT=100\n", 1);
        check(changed != manifest, "the manifest didn't change")?;
        write(&f.build.join("build.ninja"), &changed);
        let big_o = f.build.join("obj/big.o");
        let expected = || -> Result<(), String> {
            let data = std::fs::read_to_string(&big_o).map_err(|e| format!("obj/big.o: {e}"))?;
            check(data.len() > 4 << 20 && data.ends_with(&format!("{}\n", lines.last().unwrap())), "obj/big.o content")?;
            // Nothing left behind next to the outputs.
            let left: Vec<_> =
                std::fs::read_dir(f.build.join("obj")).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("ninja-rs-tmp")).collect();
            check(left.is_empty(), &format!("temporary files left: {left:?}"))
        };
        let clean = || check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed");

        // 1. A remote execution, downloaded compressed, kept in the disk cache.
        check(run_ninja(&f, Some(&ep), &[&flag, "obj/big.o"]) == 0, "first build failed")?;
        check(server.stats().executions == 1 && server.stats().compressed_reads >= 1, &format!("{:?}", server.stats()))?;
        expected()?;
        // 2. From the remote cache.
        clean()?;
        check(run_ninja(&f, Some(&ep), &["obj/big.o"]) == 0, "rebuild from the remote cache failed")?;
        check(server.stats().action_cache_hits == 1, &format!("expected a remote cache hit: {:?}", server.stats()))?;
        expected()?;
        // 3. From the disk cache alone, a chunk at a time.
        let size = std::fs::metadata(&big_o).map_or(0, |m| m.len()) as usize;
        clean()?;
        let (code, peak) = peak_during(|| run_ninja_in_process(&f, None, &[&flag, "obj/big.o"]));
        check(code == 0, "rebuild from the disk cache failed")?;
        eprintln!("large_outputs: {size} bytes restored, peak {peak} bytes allocated");
        check(peak < size / 2, &format!("obj/big.o ({size} bytes) was held in memory: {peak} bytes allocated at once"))?;
        expected()?;
        // 4. A chunk damaged in the disk cache (same size): not restored,
        //    and the object built again.
        let hash = rbe_client::digest_of(&std::fs::read(&big_o).unwrap()).hash;
        let layout = std::fs::read(disk.join("chunked").join(&hash[..2]).join(&hash)).map_err(|e| format!("no layout for obj/big.o: {e}"))?;
        let layout = <rbe_client::proto::SplitBlobResponse as prost::Message>::decode(layout.as_slice()).map_err(|e| e.to_string())?;
        let chunk = &layout.chunk_digests[layout.chunk_digests.len() / 2].hash;
        let chunk_path = disk.join("cas").join(&chunk[..2]).join(chunk);
        let mut damaged = std::fs::read(&chunk_path).map_err(|e| e.to_string())?;
        damaged[0] ^= 0x20;
        std::fs::write(&chunk_path, damaged).map_err(|e| e.to_string())?;
        clean()?;
        check(run_ninja(&f, None, &[&flag, "obj/big.o"]) == 0, "rebuild after damage failed")?;
        expected()
    })
}

/// Concurrent actions needing the same blob (the toolchain, on a cold
/// CAS) upload it once: the others wait for that upload.
fn test_shared_uploads() -> Result<(), String> {
    let f = fixture("shared_uploads", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        server.state.set_write_delay(std::time::Duration::from_millis(500));
        check(run_ninja(&f, Some(&ep), &["-j8", "obj/main.o", "obj/lib.o", "obj/big.o"]) == 0, "the build failed")?;
        let stats = server.stats();
        check(stats.bytestream_writes == 1 && stats.executions == 3, &format!("the toolchain wasn't uploaded once: {stats:?}"))
    })
}

fn test_untrusted_results() -> Result<(), String> {
    use rbe_client::proto as re;
    let f = fixture("untrusted", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let main_o = f.build.join("obj/main.o");
        check(run_ninja(&f, Some(&ep), &["obj/main.o"]) == 0, "first build failed")?;
        let good = std::fs::read(&main_o).map_err(|e| e.to_string())?;
        let planted = server.state.put(b"planted".to_vec());
        let escape = f.root.join("escape.txt");
        let absolute = f.root.join("absolute.txt").to_string_lossy().replace('\\', "/");
        let rebuild = |what: &str| -> Result<(), String> {
            let _ = std::fs::remove_file(&main_o);
            check(run_ninja(&f, Some(&ep), &["obj/main.o"]) == 0, &format!("{what}: the build failed"))?;
            check(std::fs::read(&main_o).ok().as_ref() == Some(&good), &format!("{what}: obj/main.o isn't the compile's"))?;
            check(!escape.exists() && !f.root.join("absolute.txt").exists(), &format!("{what}: a file was written outside the build"))
        };
        let file = |path: &str, d: &re::Digest| re::OutputFile { path: path.into(), digest: Some(d.clone()), ..Default::default() };

        let (p, a) = (planted.clone(), absolute.clone());
        server.state.set_tamper(Some(Box::new(move |r| {
            r.output_files.push(file("../escape.txt", &p));
            r.output_files.push(file(&a, &p));
        })));
        rebuild("outputs outside the build directory")?;

        server.state.set_tamper(Some(Box::new(|r| r.output_files.retain(|o| !o.path.ends_with("main.o")))));
        rebuild("a missing output")?;

        server.state.set_tamper(Some(Box::new(|r| {
            r.output_files.retain(|o| !o.path.ends_with("main.o"));
            r.output_symlinks.push(re::OutputSymlink { path: "obj/main.o".into(), target: "../../escape.txt".into(), ..Default::default() });
        })));
        rebuild("a symlink output")?;
        check(std::fs::symlink_metadata(&main_o).is_ok_and(|m| m.is_file()), "obj/main.o should be a file")?;

        server.state.set_tamper(None);
        server.state.corrupt_blob(&rbe_client::digest_of(&good).hash);
        rebuild("a corrupt blob")?;

        // A digest that is a path, with a disk cache (where a blob is read,
        // then removed for not matching its digest), and an output named
        // twice: refused, and the file named is left alone.
        let victim = f.root.join("victim.txt");
        write(&victim, "precious");
        let target = re::Digest { hash: victim.to_string_lossy().into_owned(), size_bytes: 8 };
        let disk = format!("--disk_cache={}", f.root.join("disk").to_string_lossy());
        for tamper in [
            Box::new(move |r: &mut re::ActionResult| r.output_files.iter_mut().for_each(|o| o.digest = Some(target.clone()))) as fake_reapi::Tamper,
            Box::new(|r: &mut re::ActionResult| r.output_files.extend(r.output_files.clone())),
        ] {
            server.state.set_tamper(Some(tamper));
            server.state.clear_action_cache();
            let _ = std::fs::remove_file(&main_o);
            check(run_ninja(&f, Some(&ep), &[&disk, "obj/main.o"]) == 0, "a malformed result: the build failed")?;
            check(std::fs::read(&main_o).ok().as_ref() == Some(&good), "a malformed result: obj/main.o isn't the compile's")?;
            check(std::fs::read_to_string(&victim).is_ok_and(|v| v == "precious"), "a malformed result: the file its digest names was touched")?;
        }
        Ok(())
    })
}

/// Response files of a command run locally from its prepared action (a
/// disk cache miss): rewritten as the remote action reads them, but a
/// checked-in one is copied, never modified; ninja's own is rewritten in
/// place.
fn test_response_files() -> Result<(), String> {
    let f = fixture("rsp", true);
    let inc = f.root.join("include").to_string_lossy().replace('\\', "/");
    let flags = f.root.join("src/flags.rsp");
    let original = format!("-I{inc}\n");
    write(&flags, &original);
    let cc = f.compiler.to_string_lossy().to_string();
    let manifest = format!(
        "rbe_version = 1\nrbe_toolchains = CMakeFiles/rbe_toolchains.json\nrbe_targets = CMakeFiles/rbe_targets.json\n\
rule CC\n  command = {cc} @{flags} -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n\
rule CCRSP\n  command = {cc} @$out.rsp -MD -MF $out.d -c $in -o $out\n  depfile = $out.d\n  deps = gcc\n  rspfile = $out.rsp\n  rspfile_content = -I{inc}\n\
build a.o: CC {main}\n  rbe_mnemonic = CCompile\n\
build b.o: CCRSP {main}\n  rbe_mnemonic = CCompile\n",
        flags = ninja_escape_path(&flags.to_string_lossy()),
        main = ninja_escape_path(&f.root.join("src/main.c").to_string_lossy()),
    );
    write(&f.build.join("build.ninja"), &manifest);
    let disk = format!("--disk_cache={}", f.root.join("disk-cache").to_string_lossy());
    check(run_ninja(&f, None, &[&disk]) == 0, "the build failed")?;
    for o in ["a.o", "b.o"] {
        check(std::fs::read_to_string(f.build.join(o)).is_ok_and(|s| s.contains("#define DEEP 0")), &format!("{o} isn't the compile's"))?;
    }
    check(std::fs::read_to_string(&flags).ok().as_deref() == Some(original.as_str()), "the checked-in response file was modified")?;
    let copies: Vec<String> = std::fs::read_dir(f.build.join(".ninja_rs_rsp"))
        .map(|d| d.flatten().filter_map(|e| std::fs::read_to_string(e.path()).ok()).collect())
        .unwrap_or_default();
    check(copies.len() == 1 && !copies[0].contains(&inc), &format!("expected one relative private copy, got {copies:?}"))?;
    Ok(())
}

fn test_without_executor_runs_locally() -> Result<(), String> {
    let f = fixture("noexec", true);
    check(run_ninja(&f, None, &[]) == 0, "local build failed")?;
    check(f.build.join("app.bin").exists(), "app.bin missing")?;
    let _ = &f.compiler;
    Ok(())
}

/// A local link of many remote-only objects, each smaller than what is
/// fetched into a file as it arrives: they're written a few at a time, not
/// all held in memory at once.
fn test_materialize_memory() -> Result<(), String> {
    let f = fixture("materializememory", true);
    let mut manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    let mut objs = Vec::new();
    for i in 0..20 {
        let src = f.root.join(format!("src/m{i}.c"));
        write(&src, &format!("{}\n", noise_lines(2_000, 100 + i).join("\n")));
        // About 3.3 MB each: under the 4 MiB fetched into files as they
        // arrive.
        manifest.push_str(&format!(
            "build obj/m{i}.o: CC {}\n  DEP_FILE = obj/m{i}.o.d\n  FLAGS = -DREPEAT=32\n  rbe_mnemonic = CCompile\n",
            ninja_escape_path(&src.to_string_lossy())
        ));
        objs.push(format!("obj/m{i}.o"));
    }
    manifest.push_str(&format!("build many.bin: LINK {}\n  rbe_mnemonic = CLink\n", objs.join(" ")));
    write(&f.build.join("build.ninja"), &manifest);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_mnemonics=Compile,Link", "many.bin"]) == 0, "first build failed")?;
        check(!f.build.join("obj/m0.o").exists(), "the objects were downloaded")?;
        // The same local link fetching the objects, then with them there
        // already: what it takes beyond that is what the downloads held.
        let link = ["--remote_download_minimal", "--remote_mnemonics=Compile", "many.bin"];
        std::fs::remove_file(f.build.join("many.bin")).map_err(|e| e.to_string())?;
        let (code, peak) = run_ninja_peak(&f, Some(&ep), &link);
        check(code == 0, "the local link failed")?;
        std::fs::remove_file(f.build.join("many.bin")).map_err(|e| e.to_string())?;
        let (code, base) = run_ninja_peak(&f, Some(&ep), &link);
        check(code == 0, "the local link of the objects there failed")?;
        let (Some(peak), Some(base)) = (peak, base) else {
            harness::not_tested("no peak memory of a process here");
            return Ok(());
        };
        let sizes: Vec<u64> = objs.iter().map(|o| std::fs::metadata(f.build.join(o)).map_or(0, |m| m.len())).collect();
        let total = sizes.iter().sum::<u64>() as usize;
        eprintln!("materialize_memory: {} objects, {total} bytes, peak {peak} bytes, {base} without downloads", objs.len());
        check(sizes.iter().all(|&s| s > 3 << 20 && s < 4 << 20), &format!("object sizes {sizes:?}"))?;
        let held = peak.saturating_sub(base);
        check(held < total / 2, &format!("the objects ({total} bytes) were held in memory: {held} bytes more at the peak than without downloads"))
    })
}

/// A build with the disk cache alone drops the records of remote-only
/// outputs: its local runs replace them, and a later remote build mustn't
/// bring the old outputs back.
fn test_disk_only_build_drops_records() -> Result<(), String> {
    let f = fixture("diskonlyrecords", true);
    let disk = format!("--disk_cache={}", f.root.join("disk-cache").to_string_lossy());
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flags = ["--remote_download_minimal", "--remote_mnemonics=Compile,Link"];
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        check(remote_only(&f).contains(&"obj/main.o".to_string()), &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        // Another command for obj/main.o, built with the disk cache alone.
        let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
        write(&f.build.join("build.ninja"), &manifest.replacen("DEP_FILE = obj/main.o.d\n", "DEP_FILE = obj/main.o.d\n  FLAGS = -DCHANGED\n", 1));
        check(run_ninja(&f, None, &[&disk, "obj/main.o"]) == 0, "the disk-only build failed")?;
        check(!f.build.join(".ninja_remote").exists(), &format!("records survived the disk-only build: {:?}", remote_only(&f)))?;
        // Its output gone, the remote build runs the command again.
        std::fs::remove_file(f.build.join("obj/main.o")).map_err(|e| e.to_string())?;
        check(run_ninja(&f, Some(&ep), &[flags[0], flags[1], "obj/main.o"]) == 0, "the remote build failed")?;
        check(server.stats().executions == 4, &format!("the old remote object came back: {:?}", server.stats()))
    })
}

/// A remote service that refuses connections, after a build that left
/// outputs remote: built locally (with the disk cache) as without records
/// of them, a local command reading one included.
fn test_unreachable_after_minimal_downloads() -> Result<(), String> {
    let f = fixture("unreachable_records", true);
    let disk = format!("--disk_cache={}", f.root.join("disk-cache").to_string_lossy());
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flags = ["--remote_download_minimal", "--remote_mnemonics=Compile,Link"];
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        check(remote_only(&f).contains(&"obj/lib.o".to_string()), &format!("remote-only outputs: {:?}", remote_only(&f)))?;
        let (code, out) = run_ninja_output(&f, Some("grpc://127.0.0.1:1"), &[&disk, flags[0], flags[1], "copy.o"]);
        check(code == 0 && out.contains("building locally"), &format!("the local build failed:\n{out}"))?;
        let lib_o = std::fs::read(f.build.join("obj/lib.o")).map_err(|e| format!("obj/lib.o: {e}"))?;
        check(std::fs::read(f.build.join("copy.o")).ok() == Some(lib_o), "copy.o content")?;
        check(!f.build.join(".ninja_remote").exists(), &format!("records survived the local build: {:?}", remote_only(&f)))
    })
}

/// A local run of an action whose side outputs (split DWARF) were left
/// remote drops their records with its outputs'.
fn test_local_run_drops_side_output_records() -> Result<(), String> {
    let f = fixture("siderecords", true);
    let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    write(&f.build.join("build.ninja"), &manifest.replacen("DEP_FILE = obj/main.o.d\n", "DEP_FILE = obj/main.o.d\n  FLAGS = -gsplit-dwarf\n", 1));
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_mnemonics=Compile,Link", "app.bin"]) == 0, "first build failed")?;
        let recorded = remote_only(&f);
        check(recorded.contains(&"obj/main.o".to_string()) && recorded.contains(&"obj/main.dwo".to_string()), &format!("remote-only outputs: {recorded:?}"))?;
        // Another command for obj/main.o, run locally.
        let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
        write(&f.build.join("build.ninja"), &manifest.replacen("FLAGS = -gsplit-dwarf\n", "FLAGS = -gsplit-dwarf -DCHANGED\n", 1));
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_mnemonics=Link", "obj/main.o"]) == 0, "the local compile failed")?;
        check(f.build.join("obj/main.dwo").exists(), "the local compile wrote no obj/main.dwo")?;
        let recorded = remote_only(&f);
        check(!recorded.iter().any(|r| r.starts_with("obj/main.")), &format!("records of the local run's outputs survived: {recorded:?}"))
    })
}

/// A local command reading a thin archive whose member is outside the
/// build directory (a prebuilt object of the source tree), with remote-only
/// outputs recorded: the member is read where it is.
fn test_thin_archive_member_outside() -> Result<(), String> {
    let f = fixture("thinoutside", true);
    write(&f.root.join("prebuilt/x.o"), "PREBUILT\n");
    let mut manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    manifest.push_str("build lib/libout.a: THINAR ../prebuilt/x.o\n  rbe_mnemonic = CArchive\nbuild out.bin: LINK lib/libout.a\n  rbe_mnemonic = CLink\n");
    write(&f.build.join("build.ninja"), &manifest);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_mnemonics=Compile,Link", "app.bin"]) == 0, "first build failed")?;
        check(!remote_only(&f).is_empty(), "nothing recorded")?;
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_mnemonics=Compile", "out.bin"]) == 0, "the local link failed")?;
        check(std::fs::read_to_string(f.build.join("out.bin")).is_ok_and(|s| s.contains("PREBUILT")), "out.bin")
    })
}

/// A source edited after the build hashed it, before the compile read it:
/// the result isn't what the key describes, and isn't stored.
fn test_input_changed_during_local_run() -> Result<(), String> {
    let f = fixture("concurrentchange", true);
    let disk = f.root.join("disk-cache");
    let flag = format!("--disk_cache={}", disk.to_string_lossy());
    let lib_c = f.root.join("src/lib.c");
    let original = std::fs::read_to_string(&lib_c).unwrap();
    let edit = f.build.join("local-edit-source");
    write(&edit, &format!("{}\n#include \"lib.h\"\nint lib() {{ return 2; }} /* EDITED */\n", lib_c.to_string_lossy()));
    check(run_ninja(&f, None, &[&flag, "obj/lib.o"]) == 0, "first build failed")?;
    std::fs::remove_file(&edit).map_err(|e| e.to_string())?;
    check(std::fs::read_to_string(f.build.join("obj/lib.o")).is_ok_and(|s| s.contains("EDITED")), "the compile didn't read the edit")?;
    check(disk_cache_entries(&disk) == 0, &format!("the result was stored: {} entries", disk_cache_entries(&disk)))?;
    // The edit undone: compiled from the original again.
    write(&lib_c, &original);
    check(run_ninja(&f, None, &[&flag, "obj/lib.o"]) == 0, "second build failed")?;
    check(!std::fs::read_to_string(f.build.join("obj/lib.o")).unwrap_or_default().contains("EDITED"), "the edited object came back for the original source")
}

/// Minimal downloads and a step on the host after the remote one reading
/// its output (a post-build copy): the output is downloaded for it.
fn test_host_step_reads_remote_output() -> Result<(), String> {
    let f = fixture("hoststepoutput", true);
    let cc = f.compiler.to_string_lossy().to_string();
    let cm = f.root.join("cmake/bin").join(if cfg!(windows) { "fake-cmake.exe" } else { "fake-cmake" }).to_string_lossy().to_string();
    let steps = [format!("{cc} --link $in -o $out"), format!("{cm} -E copy $out lib.copy")];
    let chain = if cfg!(windows) { format!("cmd.exe /C \"{}\"", steps.join(" && ")) } else { steps.join(" && ") };
    let mut manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    manifest.push_str(&format!(
        "rule ARCOPY\n  command = {chain}\nbuild libc.a: ARCOPY obj/main.o obj/lib.o\n  rbe_mnemonic = CArchive\nbuild libc.stamp: TOUCH | libc.a\n"
    ));
    write(&f.build.join("build.ninja"), &manifest);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "--remote_mnemonics=Compile,Archive", "libc.stamp"]) == 0, "the build failed")?;
        check(server.stats().executions == 3, &format!("expected 3 remote executions, got {:?}", server.stats()))?;
        check(std::fs::read_to_string(f.build.join("lib.copy")).is_ok_and(|s| s.starts_with("LINKED")), "lib.copy")
    })
}

/// The environment variables a tool reads go into its action's
/// environment, paths rewritten as in arguments: the key describes them,
/// remote runs get them, and local results are stored under it. Those of
/// other tools don't change the key. `--action_env` sets more.
fn test_action_environment() -> Result<(), String> {
    let f = fixture("actionenv", true);
    let disk = f.root.join("disk-cache");
    let flag = format!("--disk_cache={}", disk.to_string_lossy());
    let log = f.root.join("actions.jsonl");
    let log_flag = format!("--execution_log_file={}", log.to_string_lossy());
    let objs = ["obj/main.o", "obj/lib.o"];
    let clean = || check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed");
    let entries = || disk_cache_entries(&disk);
    // The values of `name` in the logged actions' environments.
    let env_of = |name: &str| -> Vec<Option<String>> {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        text.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).map(|v| v["environment"][name].as_str().map(str::to_string)).collect()
    };
    let with_var = |name: &str, value: &str, args: &[&str]| run_ninja_with(&f, None, args, &[(name, std::ffi::OsStr::new(value))]);
    // 1. LIB, which linkers read: the compiles are stored, under their keys.
    check(with_var("LIB", &f.root.to_string_lossy(), &[&flag, &log_flag, objs[0], objs[1]]) == 0, "build with LIB set failed")?;
    check(entries() == 2, &format!("expected the 2 compiles stored, got {}", entries()))?;
    check(env_of("LIB") == [None, None], &format!("LIB in the actions: {:?}", env_of("LIB")))?;
    // 2. CPATH, which GCC-style compilers read: in the actions, relative.
    clean()?;
    let cpath = f.root.join("include").to_string_lossy().into_owned();
    check(with_var("CPATH", &cpath, &[&flag, &log_flag, objs[0], objs[1]]) == 0, "build with CPATH set failed")?;
    check(entries() == 4, &format!("expected the 2 compiles stored under other keys, got {}", entries()))?;
    let values = env_of("CPATH");
    check(
        values.len() == 2 && values.iter().all(|v| v.as_deref().map(|v| v.replace('\\', "/")).as_deref() == Some("../include")),
        &format!("CPATH in the actions: {values:?}"),
    )?;
    // 3. A variable making the compiler write a file the action doesn't
    //    return: not stored.
    clean()?;
    write(&f.root.join("src/lib.c"), "#include \"lib.h\"\nint lib() { return DEEP + 3; }\n");
    check(with_var("DEPENDENCIES_OUTPUT", "deps.txt", &[&flag, &log_flag, objs[1]]) == 0, "build with DEPENDENCIES_OUTPUT set failed")?;
    check(entries() == 4, &format!("a result with DEPENDENCIES_OUTPUT set was stored: {}", entries()))?;
    // 4. Unless the action gets it (--action_env), with other variables.
    clean()?;
    let args = [flag.as_str(), log_flag.as_str(), "--action_env=DEPENDENCIES_OUTPUT", "--action_env=FOO=bar", objs[1]];
    check(with_var("DEPENDENCIES_OUTPUT", "deps.txt", &args) == 0, "build with --action_env failed")?;
    check(entries() == 5, &format!("expected the compile stored, got {}", entries()))?;
    check(
        env_of("FOO") == [Some("bar".to_string())] && env_of("DEPENDENCIES_OUTPUT") == [Some("deps.txt".to_string())],
        &format!("FOO in the action: {:?}", env_of("FOO")),
    )
}

/// A definition keeps its absolute path (the program keeps it): when it
/// names one of the compile's inputs (a header included through the
/// macro), which a remote worker has at another path, it runs locally.
fn test_definition_naming_an_input() -> Result<(), String> {
    let f = fixture("defineinput", true);
    let header = f.root.join("include/lib.h").to_string_lossy().replace('\\', "/");
    let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    write(
        &f.build.join("build.ninja"),
        &manifest.replacen("DEP_FILE = obj/main.o.d\n", &format!("DEP_FILE = obj/main.o.d\n  FLAGS = -DCONFIG_FILE={header}\n"), 1),
    );
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["--remote_strict", "obj/main.o"]) != 0, "the compile ran remotely")?;
        check(run_ninja(&f, Some(&ep), &["obj/main.o", "obj/lib.o"]) == 0, "the build failed")?;
        check(server.stats().executions == 1, &format!("expected obj/lib.o alone remotely, got {:?}", server.stats()))
    })
}

/// Results whose reads can't be checked (a link: no depfile lists what it
/// read) are kept in the disk cache, not stored in the remote cache other
/// machines read.
fn test_unchecked_results_stay_local() -> Result<(), String> {
    let f = fixture("uncheckedresults", true);
    let disk = f.root.join("disk-cache");
    with_server(&f, |server| {
        let flags =
            [format!("--remote_cache={}", server.endpoint), format!("--disk_cache={}", disk.to_string_lossy()), "--remote_mnemonics=Compile,Link".into()];
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        check(run_ninja(&f, None, &flags) == 0, "the build failed")?;
        let st = server.stats();
        check(st.action_cache_updates == 2, &format!("expected the 2 compiles stored remotely, got {st:?}"))?;
        check(disk_cache_entries(&disk) == 3, &format!("expected the compiles and the link in the disk cache, got {}", disk_cache_entries(&disk)))
    })
}

/// A disk cache hit whose blob can't be read is a miss: the action runs
/// and its result replaces the entry (with --remote_strict too).
fn test_unreadable_disk_hit() -> Result<(), String> {
    let f = fixture("unreadabledisk", true);
    let disk = f.root.join("disk-cache");
    let flag = format!("--disk_cache={}", disk.to_string_lossy());
    check(run_ninja(&f, None, &[&flag, "obj/main.o"]) == 0, "first build failed")?;
    let main_o = std::fs::read(f.build.join("obj/main.o")).map_err(|e| e.to_string())?;
    let blob = rbe_client::digest_of(&main_o);
    let blob_path = disk.join("cas").join(&blob.hash[..2]).join(&blob.hash);
    let mut altered = main_o.clone();
    altered[0] ^= 0x20;
    std::fs::write(&blob_path, &altered).map_err(|e| e.to_string())?;
    check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
    check(run_ninja(&f, None, &[&flag, "--remote_strict", "obj/main.o"]) == 0, "the build after a corrupt blob failed")?;
    check(std::fs::read(f.build.join("obj/main.o")).ok() == Some(main_o.clone()), "obj/main.o")?;
    check(std::fs::read(&blob_path).ok() == Some(main_o), "the entry wasn't replaced")
}

/// A remote cache hit whose outputs the CAS evicted is a miss (full
/// downloads): the action runs again.
fn test_evicted_remote_hit() -> Result<(), String> {
    let f = fixture("evictedhit", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["obj/main.o"]) == 0, "first build failed")?;
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        server.state.clear_cas();
        check(run_ninja(&f, Some(&ep), &["--remote_strict", "obj/main.o"]) == 0, "the build after the eviction failed")?;
        check(server.stats().executions == 2, &format!("expected the compile again, got {:?}", server.stats()))?;
        check(std::fs::read_to_string(f.build.join("obj/main.o")).is_ok_and(|s| s.contains("int main()")), "obj/main.o")
    })
}

/// A disk cache that can't store blobs (full, read only) doesn't fail the
/// downloads that would have filled it.
fn test_disk_cache_write_errors() -> Result<(), String> {
    let f = fixture("diskwriteerrors", true);
    let disk = f.root.join("disk-cache");
    // Files where the blobs' directories go.
    for i in 0..=255u8 {
        write(&disk.join("cas").join(format!("{i:02x}")), "");
    }
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        let flag = format!("--disk_cache={}", disk.to_string_lossy());
        check(run_ninja(&f, Some(&ep), &[&flag, "--remote_strict", "obj/main.o"]) == 0, "the build failed")?;
        check(server.stats().executions == 1, &format!("expected a remote execution, got {:?}", server.stats()))?;
        check(std::fs::read_to_string(f.build.join("obj/main.o")).is_ok_and(|s| s.contains("int main()")), "obj/main.o")?;
        check(disk_cache_entries(&disk) == 0, "a result was stored without its blobs")
    })
}

/// `restat`: an output a remote action left as it was (same contents) keeps
/// its time, so what depends on it doesn't run again.
fn test_restat_unchanged_output() -> Result<(), String> {
    let f = fixture("restatremote", true);
    let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    let cc_rule = manifest.lines().skip_while(|l| !l.starts_with("rule CC")).take(5).collect::<Vec<_>>().join("\n");
    let restat_rule = format!("{}\n  restat = 1\n", cc_rule.replacen("rule CC", "rule CCRESTAT", 1));
    let manifest = manifest.replacen("rule LINK\n", &format!("{restat_rule}rule LINK\n"), 1).replacen("build obj/lib.o: CC ", "build obj/lib.o: CCRESTAT ", 1);
    write(&f.build.join("build.ninja"), &format!("{manifest}build lib.copy: COPY obj/lib.o\n"));
    // Runs of lib.copy, as the build log has them.
    let copies = || {
        let log = std::fs::read_to_string(f.build.join(ninja_core::build_log::BUILD_LOG_FILE)).unwrap_or_default();
        log.lines().filter(|l| l.split('\t').nth(3) == Some("lib.copy")).count()
    };
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["lib.copy"]) == 0, "first build failed")?;
        check(copies() == 1, &format!("lib.copy ran {} times", copies()))?;
        // The header's line endings change: the same object.
        write(&f.root.join("include/nested/deep.h"), "#define DEEP 0\r\n");
        check(run_ninja(&f, Some(&ep), &["lib.copy"]) == 0, "second build failed")?;
        check(server.stats().executions == 2, &format!("expected the compile again, got {:?}", server.stats()))?;
        check(copies() == 1, "lib.copy ran again for an unchanged obj/lib.o")
    })
}

/// A source edited while the local compile ran and edited back before it
/// ended: its contents are the key's again, but the object isn't theirs.
/// The result isn't stored (its stamp changed), so the original source
/// never gets the edited object.
fn test_input_edited_back_during_local_run() -> Result<(), String> {
    let f = fixture("editedback", true);
    let disk = f.root.join("disk-cache");
    let flag = format!("--disk_cache={}", disk.to_string_lossy());
    let lib_c = f.root.join("src/lib.c");
    let original = std::fs::read_to_string(&lib_c).unwrap();
    let edit = f.build.join("local-edit-source-back");
    write(&edit, &format!("{}\n#include \"lib.h\"\nint lib() {{ return 2; }} /* EDITED */\n", lib_c.to_string_lossy()));
    check(run_ninja(&f, None, &[&flag, "obj/lib.o"]) == 0, "first build failed")?;
    std::fs::remove_file(&edit).map_err(|e| e.to_string())?;
    check(std::fs::read_to_string(&lib_c).unwrap() == original, "the source wasn't edited back")?;
    check(std::fs::read_to_string(f.build.join("obj/lib.o")).is_ok_and(|s| s.contains("EDITED")), "the compile didn't read the edit")?;
    check(disk_cache_entries(&disk) == 0, &format!("the result was stored: {} entries", disk_cache_entries(&disk)))?;
    check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
    check(run_ninja(&f, None, &[&flag, "obj/lib.o"]) == 0, "second build failed")?;
    check(!std::fs::read_to_string(f.build.join("obj/lib.o")).unwrap_or_default().contains("EDITED"), "the edited object came back for the original source")
}

/// A disk cache hit whose blobs can't be read (damaged) is a miss before
/// anything runs on the host: a command's step on the host before its
/// remote one runs once, for the remote cache hit that follows.
fn test_host_steps_once_on_disk_miss() -> Result<(), String> {
    let f = fixture("stepsonce", true);
    let cm = f.root.join("cmake/bin").join(if cfg!(windows) { "fake-cmake.exe" } else { "fake-cmake" }).to_string_lossy().to_string();
    let ar = f.root.join("tc/bin").join(if cfg!(windows) { "fake-ar.exe" } else { "fake-ar" }).to_string_lossy().to_string();
    let steps = [format!("{cm} -E append steps.log"), format!("{ar} qcT $out $in")];
    let chain = if cfg!(windows) { format!("cmd.exe /C \"{}\"", steps.join(" && ")) } else { steps.join(" && ") };
    let mut manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    manifest.push_str(&format!("rule STEPAR\n  command = {chain}\nbuild libsteps.a: STEPAR obj/main.o\n  rbe_mnemonic = CArchive\n"));
    write(&f.build.join("build.ninja"), &manifest);
    let disk = f.root.join("disk-cache");
    let flags = [format!("--disk_cache={}", disk.to_string_lossy()), "--remote_mnemonics=Compile,Archive".into(), "libsteps.a".into()];
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let log = f.build.join("steps.log");
    let runs = || std::fs::read_to_string(&log).unwrap_or_default().lines().count();
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        check(runs() == 1, &format!("the host step ran {} times", runs()))?;
        // The archive's blob in the disk cache, damaged.
        let lib = std::fs::read(f.build.join("libsteps.a")).map_err(|e| e.to_string())?;
        let blob = rbe_client::digest_of(&lib);
        let blob_path = disk.join("cas").join(&blob.hash[..2]).join(&blob.hash);
        let mut altered = lib.clone();
        altered[1] ^= 0x20;
        std::fs::write(&blob_path, &altered).map_err(|e| e.to_string())?;
        std::fs::remove_file(&log).map_err(|e| e.to_string())?;
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        check(run_ninja(&f, Some(&ep), &flags) == 0, "second build failed")?;
        check(std::fs::read(f.build.join("libsteps.a")).ok() == Some(lib), "libsteps.a")?;
        check(runs() == 1, &format!("the host step ran {} times", runs()))
    })
}

/// A disk cache hit for a command whose step on the host before the remote
/// one deletes the archive it makes, which the host has already: written
/// after the step.
fn test_disk_hit_with_host_steps() -> Result<(), String> {
    let f = fixture("diskhitsteps", true);
    let disk = f.root.join("disk-cache");
    let flags = [format!("--disk_cache={}", disk.to_string_lossy()), "--remote_mnemonics=Compile,Archive".into(), "--remote_strict".into(), "lib.a".into()];
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &flags) == 0, "first build failed")?;
        let lib = std::fs::read(f.build.join("lib.a")).map_err(|e| e.to_string())?;
        // Older than its inputs, the same contents: out of date, the same key.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(f.build.join("lib.a")).and_then(|f| f.set_modified(past)).map_err(|e| e.to_string())?;
        write(&f.build.join("pre.flag"), "");
        let executions = server.stats().executions;
        check(run_ninja(&f, Some(&ep), &flags) == 0, "the build from the disk cache failed")?;
        check(server.stats().executions == executions, &format!("not from the disk cache: {:?}", server.stats()))?;
        check(!f.build.join("pre.flag").exists(), "the step on the host didn't run")?;
        check(std::fs::read(f.build.join("lib.a")).ok() == Some(lib), "lib.a")
    })
}

/// Minimal downloads, an output of a remote action built while the
/// manifest is regenerated: recorded, as the build's are, for the build
/// after the regeneration, which then doesn't run its action again.
fn test_regeneration_keeps_records() -> Result<(), String> {
    let f = fixture("regenrecords", true);
    let cm = f.root.join("cmake/bin").join(if cfg!(windows) { "fake-cmake.exe" } else { "fake-cmake" }).to_string_lossy().to_string();
    let manifest = std::fs::read_to_string(f.build.join("build.ninja")).unwrap();
    let manifest = format!(
        "{manifest}rule REGEN
  command = {cm} -E copy $in $out
  generator = 1
build build.ninja: REGEN build.ninja.in || obj/main.o
"
    );
    write(&f.build.join("build.ninja.in"), &manifest);
    write(&f.build.join("build.ninja"), &manifest);
    // Older than its input: regenerated first.
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options().write(true).open(f.build.join("build.ninja")).and_then(|f| f.set_modified(past)).map_err(|e| e.to_string())?;
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["--remote_download_minimal", "app.bin"]) == 0, "the build failed")?;
        let st = server.stats();
        check(st.executions == 2 && st.action_cache_hits == 0, &format!("expected each compile once: {st:?}"))?;
        check(std::fs::read_to_string(f.build.join("app.bin")).is_ok_and(|s| s.contains("int main()")), "app.bin")
    })
}

/// A disk cache that can't store chunks (a read-only shared one) and a
/// remote that splits blobs: a large output's chunks are written as they
/// arrive, never downloaded twice.
fn test_chunks_with_unwritable_disk_cache() -> Result<(), String> {
    let f = fixture("chunksunstored", true);
    let disk = f.root.join("disk-cache");
    // Files where the cache's shards go: nothing can be stored.
    for i in 0..=255u8 {
        write(&disk.join("cas").join(format!("{i:02x}")), "");
    }
    with_server(&f, |server| {
        server.state.set_chunking(Some(13));
        let ep = server.endpoint.clone();
        let flag = format!("--disk_cache={}", disk.to_string_lossy());
        write(&f.root.join("src/big.c"), &format!("#include \"lib.h\"\n{}\n", noise_lines(160_000, 7).join("\n")));
        check(run_ninja(&f, Some(&ep), &[&flag, "obj/big.o"]) == 0, "the build failed")?;
        let size = std::fs::metadata(f.build.join("obj/big.o")).map_err(|e| e.to_string())?.len();
        let st = server.stats();
        check(st.splits >= 1, &format!("the output wasn't fetched as chunks: {st:?}"))?;
        check(st.bytes_read < size + size / 2, &format!("{} bytes read for an output of {size}: {st:?}", st.bytes_read))
    })
}

/// Racing, a remote cache hit whose outputs the CAS lost: the action runs
/// again remotely while the local run goes on, which then wins (its
/// outputs aren't claimed until the remote ones are fetched).
fn test_racing_evicted_hit() -> Result<(), String> {
    use std::time::Instant;
    let f = fixture("racingevicted", true);
    with_server(&f, |server| {
        let ep = server.endpoint.clone();
        check(run_ninja(&f, Some(&ep), &["obj/main.o"]) == 0, "first build failed")?;
        check(run_ninja(&f, None, &["-t", "clean"]) == 0, "clean failed")?;
        server.state.clear_cas();
        write(&f.build.join("race-local-mark"), "");
        // The remote side waits for the local run's output, or for longer
        // than the test would.
        let (main_o, deadline) = (f.build.join("obj/main.o"), Instant::now() + harness::PATIENCE);
        let local_made = move |o: &Path| std::fs::read_to_string(o).is_ok_and(|s| s.contains("RACE-LOCAL"));
        let watched = main_o.clone();
        server.state.set_execute_gate(Some(std::sync::Arc::new(move || local_made(&watched) || Instant::now() > deadline)));
        let race = ["--remote_racing", "--remote_racing_delay=0", "-j4", "obj/main.o"];
        check(run_ninja(&f, Some(&ep), &race) == 0, "racing build failed")?;
        server.state.set_execute_gate(None);
        check(std::fs::read_to_string(&main_o).is_ok_and(|s| s.contains("RACE-LOCAL")), "obj/main.o isn't the local run's")
    })
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
    let tests: Vec<harness::Test> = vec![
        ("remote_build_and_cache", test_remote_build_and_cache),
        ("untrusted_results", test_untrusted_results),
        ("transient_failures", test_transient_failures),
        ("shared_uploads", test_shared_uploads),
        ("parallel_uploads", test_parallel_uploads),
        ("large_outputs", test_large_outputs),
        ("parallel_materialization", test_parallel_materialization),
        ("response_files", test_response_files),
        ("local_tool_steps", test_local_tool_steps),
        ("no_rbe_version_runs_locally", test_no_rbe_version_runs_locally),
        ("build_events", test_build_events),
        ("disk_cache_alone", test_disk_cache_alone),
        ("capture_guards", test_capture_guards),
        ("compile_side_outputs", test_compile_side_outputs),
        ("disk_cache_with_remote", test_disk_cache_with_remote),
        ("remote_cache_only", test_remote_cache_only),
        ("fallback_guards", test_fallback_guards),
        ("racing", test_racing),
        ("without_executor_runs_locally", test_without_executor_runs_locally),
        ("minimal_downloads", test_minimal_downloads),
        ("local_run_drops_records", test_local_run_drops_records),
        ("action_rewinding", test_action_rewinding),
        ("disk_cache_minimal", test_disk_cache_minimal),
        ("compression", test_compression),
        ("splice_compressed", test_splice_compressed),
        ("nested_invocation", test_nested_invocation),
        ("exec", test_exec),
        ("remote_only_scanned_input", test_remote_only_scanned_input),
        ("thin_archives", test_thin_archives),
        ("mtls", test_mtls),
        ("include_scanner_fallback", test_include_scanner_fallback),
        ("daemon", test_daemon),
        ("split_and_splice_v2_12", || test_split_and_splice(12)),
        ("split_and_splice_v2_13", || test_split_and_splice(13)),
        ("materialize_memory", test_materialize_memory),
        ("disk_only_build_drops_records", test_disk_only_build_drops_records),
        ("local_run_drops_side_output_records", test_local_run_drops_side_output_records),
        ("thin_archive_member_outside", test_thin_archive_member_outside),
        ("input_changed_during_local_run", test_input_changed_during_local_run),
        ("host_step_reads_remote_output", test_host_step_reads_remote_output),
        ("action_environment", test_action_environment),
        ("definition_naming_an_input", test_definition_naming_an_input),
        ("unchecked_results_stay_local", test_unchecked_results_stay_local),
        ("unreadable_disk_hit", test_unreadable_disk_hit),
        ("evicted_remote_hit", test_evicted_remote_hit),
        ("disk_cache_write_errors", test_disk_cache_write_errors),
        ("restat_unchanged_output", test_restat_unchanged_output),
        ("unreachable_remote", test_unreachable_remote),
        ("input_edited_back_during_local_run", test_input_edited_back_during_local_run),
        ("host_steps_once_on_disk_miss", test_host_steps_once_on_disk_miss),
        ("disk_hit_with_host_steps", test_disk_hit_with_host_steps),
        ("regeneration_keeps_records", test_regeneration_keeps_records),
        ("chunks_with_unwritable_disk_cache", test_chunks_with_unwritable_disk_cache),
        ("racing_evicted_hit", test_racing_evicted_hit),
        ("unreachable_nested", test_unreachable_nested),
        ("unreachable_after_minimal_downloads", test_unreachable_after_minimal_downloads),
    ];
    // `cargo test --test remote_e2e -- NAME`: the tests whose name has NAME.
    harness::run("e2e", tests)
}
