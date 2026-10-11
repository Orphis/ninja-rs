//! Compares the scanner with the compiler's own dependency list (`-M`) over
//! a compilation database, and times both.
//!
//! `compare <compile_commands.json>`

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cpp_deps::scan::normalize;
use cpp_deps::{Cache, Driver};

/// The compile command without its outputs, asking for the dependencies.
fn deps_command(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if matches!(a.as_str(), "-o" | "-MF" | "-MT" | "-MQ") {
            i += 1;
            continue;
        }
        if a == "-c" || a == "-MD" || a == "-MMD" || a == "-MP" {
            continue;
        }
        out.push(a.clone());
    }
    out.push("-M".into());
    out
}

/// The files of a make rule (`target: dep dep \`).
fn make_deps(text: &str, cwd: &Path) -> BTreeSet<PathBuf> {
    let joined = text.replace("\\\n", " ");
    let body = joined.split_once(": ").map(|(_, b)| b).unwrap_or("");
    let mut deps = BTreeSet::new();
    let mut cur = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                cur.push(' ');
                chars.next();
            }
            ' ' | '\n' | '\t' => {
                if !cur.is_empty() {
                    deps.insert(normalize(&cwd.join(&cur)));
                    cur.clear();
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        deps.insert(normalize(&cwd.join(&cur)));
    }
    deps
}

fn main() {
    let db = std::env::args().nth(1).expect("compile_commands.json");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(&db).unwrap()).unwrap();
    let driver = Driver::new(Cache::new(vec![], false));
    let (mut scan_time, mut ref_time, mut first) = (Duration::ZERO, Duration::ZERO, None);
    let (mut exact, mut inexact, mut errors, mut missing_total, mut extra_total, mut files_total) = (0, 0, 0, 0, 0, 0);
    for (n, e) in entries.iter().enumerate() {
        let cwd = PathBuf::from(e["directory"].as_str().unwrap());
        let argv: Vec<String> = match e.get("arguments") {
            Some(a) => a.as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect(),
            None => e["command"].as_str().unwrap().split_whitespace().map(str::to_string).collect(),
        };
        let t = Instant::now();
        let out = driver.scan(&argv, &cwd);
        let dt = t.elapsed();
        if n == 0 {
            first = Some(dt);
        } else {
            scan_time += dt;
        }
        let out = match out {
            Ok(o) => o,
            Err(err) => {
                errors += 1;
                println!("{}: scan failed: {err}", e["file"]);
                continue;
            }
        };
        let t = Instant::now();
        let r = std::process::Command::new(&argv[0]).args(&deps_command(&argv)[1..]).current_dir(&cwd).output().unwrap();
        ref_time += t.elapsed();
        let reference = make_deps(&String::from_utf8_lossy(&r.stdout), &cwd);
        // Clang lists files __has_include found too.
        let ours: BTreeSet<PathBuf> = out.files.iter().chain(&out.found).cloned().collect();
        let missing: Vec<_> = reference.difference(&ours).collect();
        let extra: Vec<_> = ours.difference(&reference).collect();
        files_total += reference.len();
        missing_total += missing.len();
        extra_total += extra.len();
        if out.exact() {
            exact += 1;
        } else {
            inexact += 1;
        }
        if !missing.is_empty() || !extra.is_empty() || !out.exact() {
            println!("{}: {} files, {} missing, {} extra, exact: {}", e["file"], reference.len(), missing.len(), extra.len(), out.exact());
            for m in missing.iter().take(5) {
                println!("  missing {}", m.display());
            }
            for x in extra.iter().take(5) {
                println!("  extra {}", x.display());
            }
            for w in out.inexact.iter().take(5) {
                println!("  inexact: {w}");
            }
            for q in out.unknown.iter().take(5) {
                println!("  unknown: {}({})", q.0, q.1);
            }
        }
    }
    let n = entries.len();
    let stats = *driver.cache.stats.lock().unwrap();
    println!("{n} compiles: {exact} exact, {inexact} inexact, {errors} errors; {files_total} files from -M, {missing_total} missing, {extra_total} extra");
    println!(
        "scan: first {:.1} ms (probing the compiler), then {:.2} ms/compile; compiler -M: {:.1} ms/compile; {} files lexed, {} reused",
        first.unwrap_or_default().as_secs_f64() * 1000.0,
        scan_time.as_secs_f64() * 1000.0 / (n.max(2) - 1) as f64,
        ref_time.as_secs_f64() * 1000.0 / n as f64,
        stats.files_lexed,
        stats.files_reused
    );
}
