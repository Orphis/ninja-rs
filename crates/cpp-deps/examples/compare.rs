//! Compares the scanner with the compiler's own dependency list over a
//! compilation database (`-E -MD` for GCC and Clang, `/E /showIncludes` for
//! `cl` and `clang-cl`), and times both. A scan is exact as builds take it:
//! nothing unknown, and every include found (else they scan otherwise).
//!
//! `compare <compile_commands.json>`

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpp_deps::scan::normalize;
use cpp_deps::vfs::Overlay;
use cpp_deps::{Cache, Driver, driver, external};

/// A path as a comparison key (case and separators ignored on Windows).
fn key(p: &Path) -> String {
    let s = normalize(p).to_string_lossy().into_owned();
    if cfg!(windows) { s.replace('\\', "/").to_lowercase() } else { s }
}

fn main() {
    let db = std::env::args().nth(1).expect("compile_commands.json");
    let entries: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(&db).unwrap()).unwrap();
    let driver = Driver::new(Cache::new(vec![], cfg!(windows)));
    let (mut scan_time, mut ref_time, mut first, mut n) = (Duration::ZERO, Duration::ZERO, None, 0);
    let (mut exact, mut inexact, mut errors, mut missing_total, mut extra_total, mut files_total) = (0, 0, 0, 0, 0, 0);
    for e in &entries {
        let file = e["file"].as_str().unwrap_or_default();
        if !["c", "cc", "cpp", "cxx"].iter().any(|x| file.to_ascii_lowercase().ends_with(&format!(".{x}"))) {
            continue;
        }
        let cwd = PathBuf::from(e["directory"].as_str().unwrap());
        let argv: Vec<String> = match e.get("arguments") {
            Some(a) => a.as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect(),
            None => external::split_command(e["command"].as_str().unwrap()),
        };
        n += 1;
        let t = Instant::now();
        let out = driver.scan(&argv, &cwd);
        let dt = t.elapsed();
        if std::env::var_os("COMPARE_VERBOSE").is_some() {
            println!("{file}: scanned in {:.1} ms", dt.as_secs_f64() * 1000.0);
        }
        if first.is_none() {
            first = Some(dt);
        } else {
            scan_time += dt;
        }
        let out = match out {
            Ok(o) => o,
            Err(err) => {
                errors += 1;
                println!("{file}: scan failed: {err}");
                continue;
            }
        };
        let t = Instant::now();
        let reference = match external::compiler_deps(&argv, &cwd, None) {
            Ok(r) => r,
            Err(err) => {
                errors += 1;
                println!("{file}: the compiler failed: {err}");
                continue;
            }
        };
        ref_time += t.elapsed();
        // The compiler may name files by their virtual paths (VFS
        // overlays): compare the real ones.
        let overlays: Vec<Arc<Overlay>> =
            driver::parse(&argv, &cwd).map(|inv| inv.overlays.iter().filter_map(|o| Overlay::load(o, &cwd).ok().map(Arc::new)).collect()).unwrap_or_default();
        let real = |p: &PathBuf| overlays.iter().rev().find_map(|o| o.map(p)).unwrap_or_else(|| p.clone());
        let reference: BTreeSet<String> = reference.iter().map(|p| key(&real(p))).collect();
        // Clang lists files __has_include found too.
        let ours: BTreeSet<String> = out.files.iter().chain(&out.found).map(|p| key(p)).collect();
        let missing: Vec<_> = reference.difference(&ours).collect();
        let extra: Vec<_> = ours.difference(&reference).collect();
        files_total += reference.len();
        missing_total += missing.len();
        extra_total += extra.len();
        let scan_exact = out.exact() && out.missing.is_empty();
        if scan_exact {
            exact += 1;
        } else {
            inexact += 1;
        }
        if !missing.is_empty() || !extra.is_empty() || !scan_exact {
            println!("{file}: {} files, {} missing, {} extra, exact: {scan_exact}", reference.len(), missing.len(), extra.len());
            for m in out.missing.iter().take(5) {
                println!("  not found: {m}");
            }
            for m in missing.iter().take(5) {
                println!("  missing {m}");
            }
            for x in extra.iter().take(5) {
                println!("  extra {x}");
            }
            for w in out.inexact.iter().take(5) {
                println!("  inexact: {w}");
            }
            for q in out.unknown.iter().take(5) {
                println!("  unknown: {}({})", q.0, q.1);
            }
        }
    }
    let stats = driver.cache.stats();
    println!(
        "{n} compiles: {exact} exact, {inexact} inexact, {errors} errors; {files_total} files from the compiler, {missing_total} missing, {extra_total} extra"
    );
    println!(
        "scan: first {:.1} ms (probing the compiler), then {:.2} ms/compile; the compiler: {:.1} ms/compile; {} files lexed, {} reused",
        first.unwrap_or_default().as_secs_f64() * 1000.0,
        scan_time.as_secs_f64() * 1000.0 / (n.max(2) - 1) as f64,
        ref_time.as_secs_f64() * 1000.0 / n.max(1) as f64,
        stats.files_lexed,
        stats.files_reused
    );
}
