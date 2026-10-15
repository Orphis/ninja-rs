//! Asking a compiler what the scanner needs, for a set of flags: its
//! predefined macros and `__has_*` operators, the dialect of its `#if`
//! (`true`, `and`...), whether it replaces trigraphs, the files every
//! compile reads first, and the answers to queries. Three command styles:
//!
//! - GCC and Clang: `-dM -E`, `-E -P`, `-E -M` on stdin;
//! - `clang-cl`: `/clang:-dM /E`, `/EP` on a temporary file;
//! - `cl.exe`: `/EP /Zc:preprocessor /PD` (MSVC prints its macros only with
//!   the conforming preprocessor), `/EP` on a temporary file.
//!
//! Results depend on the flags (`-std`, `-target`, `-O`, `-m...`, `-f...`,
//! `/std:`, `/EH...`, `/MD`...), so callers cache them per compiler and flags.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::expr::Dialect;

/// The operators compilers may have, besides `__has_include`.
pub const OPERATORS: &[&str] = &[
    "__has_include",
    "__has_include_next",
    "__has_builtin",
    "__has_constexpr_builtin",
    "__has_feature",
    "__has_extension",
    "__has_attribute",
    "__has_cpp_attribute",
    "__has_c_attribute",
    "__has_declspec_attribute",
    "__has_warning",
    "__has_embed",
    "__is_identifier",
    "__is_target_arch",
    "__is_target_vendor",
    "__is_target_os",
    "__is_target_environment",
    "__is_target_variant_os",
    "__is_target_variant_environment",
    "__building_module",
];

const OPERATOR_MARK: &str = "NINJA_RS_OPERATOR_";
const QUERY_MARK: &str = "NINJA_RS_QUERY";
/// Defined by a trigraph (`??=` is `#`) when trigraphs are on.
const TRIGRAPHS: &str = "??=define NINJA_RS_TRIGRAPHS 1\n";
const TRIGRAPHS_MARK: &str = "NINJA_RS_TRIGRAPHS";

/// How a compiler takes its flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    Gcc,
    ClangCl,
    Msvc,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// `#define` lines.
    pub predefines: String,
    /// The operators of `OPERATORS` the compiler has.
    pub operators: Vec<String>,
    /// Files every compile reads before its source (GCC on glibc targets
    /// pre-includes `stdc-predef.h`), from the dependencies of an empty
    /// source.
    pub implicit: Vec<String>,
    pub dialect: Dialect,
    /// Trigraphs are replaced (GCC's ISO modes, Clang's outside GNU modes
    /// and Windows, cl's `/Zc:trigraphs`).
    pub trigraphs: bool,
}

/// The files of make rules (`target: dep dep \` lines).
pub fn make_deps(text: &str) -> Vec<String> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut deps = Vec::new();
    for rule in joined.lines() {
        let Some((_, body)) = rule.split_once(": ") else { continue };
        let mut cur = String::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    cur.push(' ');
                    chars.next();
                }
                ' ' | '\t' => {
                    if !cur.is_empty() {
                        deps.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            deps.push(cur);
        }
    }
    deps
}

/// A compiler with the flags and the environment it runs with.
#[derive(Clone, Copy)]
pub struct Compiler<'a> {
    pub path: &'a Path,
    /// The flags that may change what it predefines (see `probe`).
    pub args: &'a [String],
    pub style: Style,
    /// `c` or `c++`.
    pub lang: &'a str,
    pub cwd: &'a Path,
    /// Its environment (None: this process's).
    pub env: Option<&'a [(String, String)]>,
}

/// A temporary source file, deleted when dropped.
struct TempSource(PathBuf);

impl TempSource {
    fn new(input: &str, lang: &str) -> Result<TempSource, String> {
        static N: AtomicU64 = AtomicU64::new(0);
        let ext = if lang == "c" { "c" } else { "cpp" };
        let p = std::env::temp_dir().join(format!("cpp-deps-probe-{}-{}.{ext}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&p, input).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(TempSource(p))
    }
}

impl Drop for TempSource {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub struct Ran {
    pub stdout: String,
    pub stderr: String,
}

/// What identifies the compiler behind its path: its version banner
/// (`--version`, or `cl`'s), which a wrapper passes through (ccache's
/// masquerade directory, an xcrun shim), unlike the metadata of the file
/// the path names. Empty when it can't run.
pub fn fingerprint(c: &Compiler) -> String {
    let mut cmd = Command::new(c.path);
    if let Some(env) = c.env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    cmd.env("LC_ALL", "C").current_dir(c.cwd).stdin(Stdio::null());
    if c.style != Style::Msvc {
        cmd.arg("--version");
    }
    match cmd.output() {
        Ok(out) => format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        Err(_) => String::new(),
    }
}

/// Runs the compiler on `input` with its flags and the `extra` flags of the
/// operation; Err when it fails.
pub fn run(c: &Compiler, extra: &[&str], input: &str) -> Result<Ran, String> {
    let (compiler, lang) = (c.path, c.lang);
    let mut cmd = Command::new(compiler);
    if let Some(env) = c.env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    // GCC translates the `-v` output read below.
    cmd.env("LC_ALL", "C");
    cmd.args(c.args).current_dir(c.cwd).stdout(Stdio::piped()).stderr(Stdio::piped());
    let _source;
    match c.style {
        Style::Gcc => {
            cmd.args(["-x", lang]).args(extra).arg("-").stdin(Stdio::piped());
        }
        Style::ClangCl | Style::Msvc => {
            let src = TempSource::new(input, lang)?;
            cmd.arg("/nologo").arg(if lang == "c" { "/TC" } else { "/TP" }).args(extra).arg(&src.0).stdin(Stdio::null());
            _source = src;
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("running {}: {e}", compiler.display()))?;
    let writer = child.stdin.take().map(|mut stdin| {
        let input = input.to_string();
        std::thread::spawn(move || stdin.write_all(input.as_bytes()))
    });
    let out = child.wait_with_output().map_err(|e| format!("running {}: {e}", compiler.display()))?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    let ran = Ran { stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::from_utf8_lossy(&out.stderr).into_owned() };
    if !out.status.success() {
        let first = ran.stderr.lines().chain(ran.stdout.lines()).find(|l| l.contains("error")).unwrap_or("").to_string();
        return Err(format!("{} failed: {first}", compiler.display()));
    }
    Ok(ran)
}

/// The flags preprocessing to stdout without line markers.
fn text_flags(style: Style) -> &'static [&'static str] {
    match style {
        Style::Gcc => &["-E", "-P"],
        Style::ClangCl | Style::Msvc => &["/EP"],
    }
}

/// The dialect of the compiler's `#if`: whether `true` is 1, and whether
/// `and` is an operator (an error otherwise).
fn dialect(c: &Compiler) -> Dialect {
    let marked = |input: &str, mark: &str| run(c, text_flags(c.style), input).is_ok_and(|r| r.stdout.contains(mark));
    Dialect {
        true_false: marked("#if true\nNINJA_RS_TRUE\n#endif\n", "NINJA_RS_TRUE"),
        alt_operators: marked("#if 1 and 1\nNINJA_RS_AND\n#endif\n", "NINJA_RS_AND"),
    }
}

/// The predefined macros, operators, implicit files and dialect, for `args`
/// (the compile's flags that may change them: without inputs, outputs,
/// macros and forced includes).
pub fn probe(c: &Compiler) -> Result<Probe, String> {
    let (compiler, style) = (c.path, c.style);
    let mut probe = Probe::default();
    match style {
        Style::Gcc | Style::ClangCl => {
            let mut input: String = OPERATORS.iter().map(|op| format!("#ifdef {op}\n#define {OPERATOR_MARK}{op} 1\n#endif\n")).collect();
            input.push_str(TRIGRAPHS);
            let flags: &[&str] = if style == Style::Gcc { &["-dM", "-E"] } else { &["/clang:-dM", "/E"] };
            let out = run(c, flags, &input)?;
            for line in out.stdout.lines() {
                match line.strip_prefix("#define ").and_then(|l| l.strip_prefix(OPERATOR_MARK)) {
                    Some(rest) => probe.operators.push(rest.split(' ').next().unwrap_or_default().to_string()),
                    None if line.strip_prefix("#define ").is_some_and(|l| l.starts_with(TRIGRAPHS_MARK)) => probe.trigraphs = true,
                    None if line.starts_with("#define ") => {
                        probe.predefines.push_str(line);
                        probe.predefines.push('\n');
                    }
                    None => {}
                }
            }
        }
        Style::Msvc => {
            let out = run(c, &["/EP", "/Zc:preprocessor", "/PD"], "")?;
            for line in out.stdout.lines().filter(|l| l.starts_with("#define ")) {
                probe.predefines.push_str(line);
                probe.predefines.push('\n');
            }
            let mut input: String = OPERATORS.iter().map(|op| format!("#ifdef {op}\n{OPERATOR_MARK} {op}\n#endif\n")).collect();
            // (Not the macro's name, which the text would expand.)
            input.push_str(&format!("{TRIGRAPHS}#ifdef {TRIGRAPHS_MARK}\n{OPERATOR_MARK} trigraphs\n#endif\n"));
            let out = run(c, &["/EP"], &input)?;
            for op in out.stdout.lines().filter_map(|l| l.trim().strip_prefix(OPERATOR_MARK)).map(str::trim) {
                if op == "trigraphs" {
                    probe.trigraphs = true;
                } else {
                    probe.operators.push(op.to_string());
                }
            }
        }
    }
    if !probe.predefines.contains("#define") {
        return Err(format!("{} printed no predefined macros", compiler.display()));
    }
    if style == Style::Gcc {
        let deps = run(c, &["-E", "-M"], "")?;
        probe.implicit = make_deps(&deps.stdout).into_iter().filter(|d| d != "-" && d != "<stdin>").collect();
    }
    probe.dialect = dialect(c);
    Ok(probe)
}

/// The built-in include directories GCC, Clang and `clang-cl` report
/// (`-v`): (directory, whether it's a framework directory).
pub fn builtin_dirs(c: &Compiler) -> Result<Vec<(String, bool)>, String> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let extra: Vec<&str> = match c.style {
        Style::Gcc => vec!["-E", "-v", "-o", null],
        Style::ClangCl => vec!["-v", "/E"],
        Style::Msvc => return Err("cl.exe has no built-in directories (INCLUDE)".into()),
    };
    let out = run(c, &extra, "")?;
    let mut dirs = Vec::new();
    let mut inside = false;
    for line in out.stderr.lines() {
        if line.starts_with("#include <...> search starts here:") {
            inside = true;
        } else if line.starts_with("End of search list.") {
            return Ok(dirs);
        } else if inside {
            let d = line.trim();
            match d.strip_suffix(" (framework directory)") {
                Some(f) => dirs.push((f.to_string(), true)),
                None => dirs.push((d.to_string(), false)),
            }
        }
    }
    Err(format!("{} -v printed no search list", c.path.display()))
}

/// Parses a query's answer: an integer literal (`201603L`).
fn integer(s: &str) -> Option<i64> {
    let digits = s.trim().trim_end_matches(['L', 'l', 'U', 'u']);
    digits.parse().ok()
}

fn answers_of(text: &str, n: usize) -> Vec<Option<i64>> {
    let mut answers = vec![None; n];
    for line in text.lines() {
        let mut parts = line.trim().splitn(3, ' ');
        if parts.next() != Some(QUERY_MARK) {
            continue;
        }
        if let (Some(Ok(i)), Some(v)) = (parts.next().map(str::parse::<usize>), parts.next())
            && i < n
        {
            // `+N`: a part of the value (a bit, see `answer`).
            answers[i] = match v.strip_prefix('+') {
                Some(part) => integer(part).map(|p| answers[i].unwrap_or(0) + p),
                None => integer(v),
            };
        }
    }
    answers
}

/// The answers to queries (operator, argument), with the same flags as
/// `probe`: their values as the compiler expands them in text, or, for
/// those it doesn't (MSVC, some GCC operators), as `#if` evaluates them,
/// one bit at a time (`__has_cpp_attribute(nodiscard) >= 201907L` needs
/// the value, not whether it's true). None for those the compiler can't
/// evaluate.
pub fn answer(c: &Compiler, queries: &[(String, String)]) -> Result<Vec<Option<i64>>, String> {
    let all: Vec<usize> = (0..queries.len()).collect();
    let text = |i: usize| {
        let (op, arg) = &queries[i];
        format!("{QUERY_MARK} {i} {op}({arg})\n")
    };
    let mut answers = vec![None; queries.len()];
    // A query the compiler can't evaluate fails the whole run: the others
    // are asked again in halves, so that only it stays unanswered.
    ask(c, &all, &text, &mut answers)?;
    let missing: Vec<usize> = all.into_iter().filter(|&i| answers[i].is_none()).collect();
    let as_if = |i: usize| {
        let (op, arg) = &queries[i];
        let mut lines = format!("#if {op}({arg})\n");
        for bit in 0..32 {
            lines.push_str(&format!("#if (({op}({arg})) >> {bit}) & 1\n{QUERY_MARK} {i} +{}\n#endif\n", 1i64 << bit));
        }
        lines.push_str(&format!("#else\n{QUERY_MARK} {i} 0\n#endif\n"));
        lines
    };
    let _ = ask(c, &missing, &as_if, &mut answers);
    Ok(answers)
}

/// Asks the queries `which` (lines from `line`), splitting the batch when
/// the compiler fails on it. Err only when the compiler fails on every
/// query alone, which likely means it can't run at all.
fn ask(c: &Compiler, which: &[usize], line: &dyn Fn(usize) -> String, answers: &mut [Option<i64>]) -> Result<(), String> {
    if which.is_empty() {
        return Ok(());
    }
    let input: String = which.iter().map(|&i| line(i)).collect();
    match run(c, text_flags(c.style), &input) {
        Ok(out) => {
            for (i, v) in answers_of(&out.stdout, answers.len()).into_iter().enumerate() {
                if answers[i].is_none() {
                    answers[i] = v;
                }
            }
            Ok(())
        }
        Err(e) if which.len() == 1 => Err(e),
        Err(_) => {
            let (a, b) = which.split_at(which.len() / 2);
            let ra = ask(c, a, line, answers);
            let rb = ask(c, b, line, answers);
            // Fine if some half worked.
            ra.or(rb)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_rules() {
        assert_eq!(make_deps("-: /usr/include/stdc-predef.h\n"), vec!["/usr/include/stdc-predef.h"]);
        assert_eq!(make_deps("x.o: a.c b\\ c.h \\\n  /d/e.h\n"), vec!["a.c", "b c.h", "/d/e.h"]);
        assert!(make_deps("").is_empty());
    }

    #[test]
    fn integers() {
        assert_eq!(integer("201603L"), Some(201603));
        assert_eq!(integer(" 1"), Some(1));
        assert_eq!(integer("__has_feature(x)"), None);
        assert_eq!(answers_of("NINJA_RS_QUERY 1 7\n  NINJA_RS_QUERY 0 2L\nother\n", 3), vec![Some(2), Some(7), None]);
        // A value from `#if`, bit by bit.
        assert_eq!(answers_of("NINJA_RS_QUERY 0 +1\nNINJA_RS_QUERY 0 +4\nNINJA_RS_QUERY 1 0\n", 2), vec![Some(5), Some(0)]);
    }

    /// Against a real compiler, when `CPP_DEPS_TEST_CXX` names one.
    #[test]
    fn real_compiler() {
        let Some(cxx) = std::env::var_os("CPP_DEPS_TEST_CXX") else { return };
        let cwd = std::env::temp_dir();
        let style = if crate::external::is_cl_style(&[cxx.to_string_lossy().into_owned()]) { Style::ClangCl } else { Style::Gcc };
        let std = [if style == Style::Gcc { "-std=c++17" } else { "/std:c++17" }.to_string()];
        let c = Compiler { path: Path::new(&cxx), args: &std, style, lang: "c++", cwd: &cwd, env: None };
        let p = probe(&c).unwrap();
        // Clang's banner, or GCC's.
        assert!(fingerprint(&c).contains("version") || fingerprint(&c).contains("Free Software Foundation"), "{}", fingerprint(&c));
        assert!(p.predefines.contains("#define __cplusplus 201703L"), "{}", p.predefines);
        assert!(p.operators.iter().any(|o| o == "__has_include"));
        assert!(p.operators.iter().any(|o| o == "__has_builtin"));
        assert!(p.dialect.true_false);
        // Never in C++17; asked for in GNU modes.
        assert!(!p.trigraphs);
        assert!(!p.predefines.contains("NINJA_RS"), "{}", p.predefines);
        if style == Style::Gcc {
            let trigraphs = ["-std=gnu11".to_string(), "-trigraphs".to_string()];
            assert!(probe(&Compiler { args: &trigraphs, lang: "c", ..c }).unwrap().trigraphs);
        }
        let a = answer(&c, &[("__has_builtin".into(), "__builtin_expect".into()), ("__has_cpp_attribute".into(), "nodiscard".into())]).unwrap();
        assert_eq!(a[0], Some(1));
        assert!(a[1].is_some_and(|v| v >= 201603));
        // A query the compiler rejects doesn't take the others with it.
        let a = answer(
            &c,
            &[("__has_builtin".into(), "__builtin_expect".into()), ("__has_builtin".into(), "(".into()), ("__has_cpp_attribute".into(), "nodiscard".into())],
        )
        .unwrap();
        assert_eq!(a[0], Some(1));
        assert_eq!(a[1], None);
        assert!(a[2].is_some());
        let p = probe(&Compiler { args: &[], lang: "c", ..c }).unwrap();
        assert!(!p.dialect.alt_operators);
        // In a given environment.
        let env: Vec<(String, String)> = std::env::vars().collect();
        assert!(probe(&Compiler { env: Some(&env), ..c }).is_ok());
    }

    /// Against cl.exe, when `CPP_DEPS_TEST_CL` names one.
    #[test]
    fn real_cl() {
        let Some(cl) = std::env::var_os("CPP_DEPS_TEST_CL") else { return };
        let cwd = std::env::temp_dir();
        let args = ["/std:c++20".to_string(), "/EHsc".to_string()];
        let c = Compiler { path: Path::new(&cl), args: &args, style: Style::Msvc, lang: "c++", cwd: &cwd, env: None };
        let p = probe(&c).unwrap();
        assert!(fingerprint(&c).contains("Version"), "{}", fingerprint(&c));
        assert!(p.predefines.contains("#define _MSC_VER "), "{}", p.predefines);
        assert!(p.predefines.contains("#define _MSVC_LANG 202002L"), "{}", p.predefines);
        assert!(p.predefines.contains("#define _CPPUNWIND 1"));
        assert!(p.operators.iter().any(|o| o == "__has_include"));
        // C++20 implies /permissive-: `and` is an operator.
        assert!(p.dialect.true_false && p.dialect.alt_operators, "{:?}", p.dialect);
        assert!(!p.trigraphs);
        let trigraphs = ["/Zc:trigraphs".to_string()];
        assert!(probe(&Compiler { args: &trigraphs, ..c }).unwrap().trigraphs);
        let a = answer(&c, &[("__has_cpp_attribute".into(), "nodiscard".into())]).unwrap();
        // cl doesn't expand it in text: its value comes from `#if`.
        assert!(a[0].is_some_and(|v| v >= 201907), "{a:?}");
    }
}
