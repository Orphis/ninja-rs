//! Scanner-only timings with a shared daemon-style cache. The first pass
//! includes compiler probing and file lexing; later passes reuse both.
//! `scanperf compile_commands.json [--threads N] [--passes N]`
//! Use `compare` or ninja-remote's `scanbench` to check compiler coverage.

#![allow(clippy::disallowed_macros)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use cpp_deps::{Cache, Driver, external};

fn main() {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("compile_commands.json");
    let (mut threads, mut passes) = (4usize, 6usize);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--threads" => threads = args.next().unwrap().parse().unwrap(),
            "--passes" => passes = args.next().unwrap().parse().unwrap(),
            _ => panic!("unknown argument {arg}"),
        }
    }
    assert!(threads > 0 && passes > 0);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(db).unwrap()).unwrap();
    let compiles: Vec<_> = entries
        .iter()
        .filter(|e| ["c", "cc", "cpp", "cxx"].iter().any(|x| e["file"].as_str().unwrap_or_default().to_ascii_lowercase().ends_with(&format!(".{x}"))))
        .map(|e| {
            let argv: Vec<String> = match e.get("arguments") {
                Some(a) => a.as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect(),
                None => external::split_command(e["command"].as_str().unwrap()),
            };
            (argv, PathBuf::from(e["directory"].as_str().unwrap()))
        })
        .collect();
    let driver = Driver::new(Cache::new(vec![], cfg!(windows)));
    println!("{} commands, {threads} threads, {passes} passes", compiles.len());
    for pass in 0..passes {
        let next = AtomicUsize::new(0);
        let accepted = AtomicUsize::new(0);
        let inexact = AtomicUsize::new(0);
        let errors = AtomicUsize::new(0);
        let inputs = AtomicUsize::new(0);
        let start = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((argv, cwd)) = compiles.get(i) else { break };
                        match driver.scan(argv, cwd) {
                            Ok(out) => {
                                if out.exact() && out.missing.is_empty() {
                                    accepted.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    inexact.fetch_add(1, Ordering::Relaxed);
                                }
                                inputs.fetch_add(out.files.len() + out.found.len(), Ordering::Relaxed);
                            }
                            Err(_) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        });
        println!(
            "pass {pass}: {:.3} ms, {} accepted, {} inexact, {} errors, {} inputs",
            start.elapsed().as_secs_f64() * 1000.0,
            accepted.load(Ordering::Relaxed),
            inexact.load(Ordering::Relaxed),
            errors.load(Ordering::Relaxed),
            inputs.load(Ordering::Relaxed),
        );
    }
}
