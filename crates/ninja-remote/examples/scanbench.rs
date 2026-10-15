//! Benchmarks the ways of finding a compile's inputs against each other,
//! over a compilation database: the conservative scanner, the `cpp-deps`
//! preprocessor, `clang-scan-deps` (once per compile, and as one batch) and
//! the compiler itself, which is also the reference for accuracy.
//!
//! `scanbench <compile_commands.json> [--threads N] [--only a,b]`
//!
//! Each runs over every compile twice with its caches kept: the first pass
//! is a cold build (probing compilers, reading headers), the second a warm
//! one (as a daemon would see). Times are wall clock with N threads taking
//! compiles from a queue, as a build would.

#![allow(clippy::disallowed_macros)] // It prints what it found.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use cpp_deps::external;
use ninja_remote::scanner::{self, ScanCache, Scanner, Style};

struct Compile {
    argv: Vec<String>,
    cwd: PathBuf,
}

type Inputs = Result<Vec<PathBuf>, String>;

/// The conservative scanner, as `action::prepare` runs it.
fn conservative(cache: &ScanCache, c: &Compile) -> Inputs {
    let windows = cfg!(windows);
    let (expanded, _) = scanner::expand_response_files(&c.argv, &c.cwd, windows);
    let mut info = scanner::parse_compile(&expanded, &c.cwd);
    // (The build directory stands for the directories the build writes to.)
    let written = scanner::WrittenDirs::new([c.cwd.clone()], windows);
    let s = Scanner { cache, windows, written: &written };
    let builtin = s.builtin_dirs(Path::new(&c.argv[0]), &info)?;
    let mut sp = scanner::search_path(&info, &builtin);
    sp.msvc = s.msvc_lookup(Path::new(&c.argv[0]), &info);
    if sp.msvc != info.msvc_lookup {
        info.place_forced_includes(sp.msvc);
    }
    if info.style == Some(Style::Msvc)
        && let Some(inc) = std::env::var_os("INCLUDE")
    {
        sp.angled.extend(std::env::split_paths(&inc).map(|p| scanner::normalize(&p)));
    }
    let mut roots = info.sources.clone();
    roots.extend(info.forced_includes.iter().cloned());
    let r = s.scan(&roots, &sp);
    if let Some(why) = r.local {
        return Err(why);
    }
    let mut files: Vec<PathBuf> = r.files.iter().cloned().collect();
    files.extend(s.computed_files(&r, &sp, &|d| written.contains(d), &|n| s.predefined(Path::new(&c.argv[0]), &info, n))?);
    files.extend(info.extra_files);
    Ok(files)
}

/// Runs `f` over every compile with `threads` threads; the results in
/// order, and the wall time.
fn run(compiles: &[Compile], threads: usize, f: &(dyn Fn(&Compile) -> Inputs + Sync)) -> (Vec<Inputs>, Duration) {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Inputs>>> = Mutex::new((0..compiles.len()).map(|_| None).collect());
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = compiles.get(i) else { break };
                    let r = f(c);
                    results.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    let elapsed = t.elapsed();
    (results.into_inner().unwrap().into_iter().map(|r| r.unwrap()).collect(), elapsed)
}

/// A path as a comparison key (case and separators ignored on Windows).
fn key(p: &Path) -> String {
    let s = cpp_deps::scan::normalize(p).to_string_lossy().into_owned();
    if cfg!(windows) { s.replace('\\', "/").to_lowercase() } else { s }
}

/// `clang-scan-deps` over the whole database in one process.
fn scan_deps_batch(compiles: &[Compile], threads: usize, dir: &Path) -> Result<Duration, String> {
    let compiler = Path::new(&compiles.first().ok_or("no compiles")?.argv[0]);
    let tool = compiler.with_file_name(if cfg!(windows) { "clang-scan-deps.exe" } else { "clang-scan-deps" });
    if !tool.is_file() {
        return Err(format!("no clang-scan-deps next to {}", compiler.display()));
    }
    let db: Vec<serde_json::Value> = compiles
        .iter()
        .map(|c| {
            let file = c.argv.iter().rev().find(|a| !a.starts_with('-') && !a.starts_with('/')).cloned().unwrap_or_default();
            serde_json::json!({"directory": c.cwd, "arguments": c.argv, "file": file})
        })
        .collect();
    let path = dir.join("scanbench-compile_commands.json");
    std::fs::write(&path, serde_json::to_vec(&db).unwrap()).map_err(|e| e.to_string())?;
    let t = Instant::now();
    let out = std::process::Command::new(&tool)
        .args(["-format=make", "-j", &threads.to_string(), "-compilation-database"])
        .arg(&path)
        .output()
        .map_err(|e| e.to_string())?;
    let elapsed = t.elapsed();
    let _ = std::fs::remove_file(&path);
    // It fails when some compiles have errors, after scanning the others.
    if !out.status.success() && out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("clang-scan-deps failed").to_string());
    }
    Ok(elapsed)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("compile_commands.json");
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut only: Option<Vec<String>> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--threads" => threads = args.next().and_then(|v| v.parse().ok()).expect("--threads N"),
            "--only" => only = Some(args.next().expect("--only a,b").split(',').map(str::to_string).collect()),
            other => panic!("unknown argument {other}"),
        }
    }
    let entries: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(&db).unwrap()).unwrap();
    let compiles: Vec<Compile> = entries
        .iter()
        .filter(|e| ["c", "cc", "cpp", "cxx"].iter().any(|x| e["file"].as_str().unwrap_or_default().to_ascii_lowercase().ends_with(&format!(".{x}"))))
        .map(|e| Compile {
            argv: match e.get("arguments") {
                Some(a) => a.as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect(),
                None => external::split_command(e["command"].as_str().unwrap()),
            },
            cwd: PathBuf::from(e["directory"].as_str().unwrap()),
        })
        .filter(|c| c.argv.len() > 1)
        .collect();
    let n = compiles.len();
    let wanted = |name: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == name));
    println!("{n} compiles, {threads} threads\n");

    // The compiler first: it's the reference.
    let (reference, ref_time) = run(&compiles, threads, &|c| external::compiler_deps(&c.argv, &c.cwd, None));
    let reference: Vec<Option<BTreeSet<String>>> = reference.into_iter().map(|r| r.ok().map(|v| v.iter().map(|p| key(p)).collect())).collect();
    let valid = reference.iter().filter(|r| r.is_some()).count();

    println!("{:<26} {:>10} {:>10} {:>10} {:>10} {:>8} {:>8} {:>7}", "", "cold", "per compile", "warm", "per compile", "inputs", "missing", "extra");
    let row = |name: &str, cold: Duration, warm: Option<Duration>, results: &[Inputs]| {
        let (mut inputs, mut missing, mut extra, mut errors, mut counted) = (0usize, 0usize, 0usize, 0usize, 0usize);
        for (r, reference) in results.iter().zip(&reference) {
            match r {
                Ok(files) => {
                    let ours: BTreeSet<String> = files.iter().map(|p| key(p)).collect();
                    inputs += ours.len();
                    counted += 1;
                    if let Some(reference) = reference {
                        missing += reference.difference(&ours).count();
                        extra += ours.difference(reference).count();
                    }
                }
                Err(e) => {
                    if errors == 0 && std::env::var_os("SCANBENCH_VERBOSE").is_some() {
                        eprintln!("{name}: {e}");
                    }
                    errors += 1;
                }
            }
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let warm_cols = match warm {
            Some(w) => format!("{:>9.0}ms {:>9.2}ms", ms(w), ms(w) / n as f64),
            None => format!("{:>11} {:>11}", "-", "-"),
        };
        println!(
            "{name:<26} {:>9.0}ms {:>9.2}ms {warm_cols} {:>8.0} {:>8} {:>7}{}",
            ms(cold),
            ms(cold) / n as f64,
            inputs as f64 / counted.max(1) as f64,
            missing,
            extra,
            if errors > 0 { format!("  ({errors} failed)") } else { String::new() }
        );
    };
    let compiler_results: Vec<Inputs> =
        reference.iter().map(|r| r.as_ref().map(|s| s.iter().map(PathBuf::from).collect()).ok_or_else(|| "failed".to_string())).collect();
    row("compiler (-M, /showIncludes)", ref_time, None, &compiler_results);

    if wanted("conservative") {
        let cache = ScanCache::default();
        let (results, cold) = run(&compiles, threads, &|c| conservative(&cache, c));
        let (_, warm) = run(&compiles, threads, &|c| conservative(&cache, c));
        row("conservative scanner", cold, Some(warm), &results);
    }
    if wanted("preprocessor") {
        let driver = cpp_deps::Driver::new(cpp_deps::Cache::new(vec![], cfg!(windows)));
        let scan = |c: &Compile| -> Inputs {
            let out = driver.scan(&c.argv, &c.cwd)?;
            if !out.exact() {
                let why = out.inexact.first().cloned().or_else(|| out.unknown.first().map(|(o, a)| format!("unknown {o}({a})"))).unwrap_or_default();
                return Err(format!("inexact: {why}"));
            }
            Ok(out.files.into_iter().chain(out.found).collect())
        };
        let (results, cold) = run(&compiles, threads, &scan);
        let (_, warm) = run(&compiles, threads, &scan);
        row("preprocessor (cpp-deps)", cold, Some(warm), &results);
    }
    if wanted("clang-scan-deps") {
        let (results, cold) = run(&compiles, threads, &|c| external::clang_scan_deps(&c.argv, &c.cwd));
        match results.iter().find_map(|r| r.as_ref().err()) {
            Some(e) if results.iter().all(Result::is_err) => println!("{:<26} not available: {e}", "clang-scan-deps"),
            _ => row("clang-scan-deps, per compile", cold, None, &results),
        }
        #[allow(clippy::disallowed_methods)] // The benchmark's scratch files, not a test's.
        let scratch = std::env::temp_dir();
        match scan_deps_batch(&compiles, threads, &scratch) {
            Ok(t) => println!("{:<26} {:>9.0}ms {:>9.2}ms", "clang-scan-deps, one batch", t.as_secs_f64() * 1000.0, t.as_secs_f64() * 1000.0 / n as f64),
            Err(e) => println!("{:<26} {e}", "clang-scan-deps, one batch"),
        }
    }
    println!("\n{valid} of {n} compiles have a reference (the compiler succeeded).");
}
