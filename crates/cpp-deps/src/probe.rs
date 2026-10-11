//! Asking a GCC or Clang compatible compiler what the scanner needs: its
//! predefined macros and `__has_*` operators for a set of flags
//! (`-dM -E`), and the answers to queries (`-E` on generated lines).
//! Results depend on the flags (`-std`, `-target`, `-O`, `-m...`, `-f...`),
//! so callers cache them per compiler and flags.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// Runs `compiler args -x lang <extra> -` with `input` on stdin, in `cwd`.
fn run(compiler: &Path, args: &[String], lang: &str, extra: &[&str], input: &str, cwd: &Path) -> Result<String, String> {
    let mut child = Command::new(compiler)
        .args(args)
        .args(["-x", lang])
        .args(extra)
        .arg("-")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running {}: {e}", compiler.display()))?;
    let mut stdin = child.stdin.take().expect("stdin");
    let input = input.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let out = child.wait_with_output().map_err(|e| format!("running {}: {e}", compiler.display()))?;
    let _ = writer.join();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} failed: {}", compiler.display(), err.lines().next().unwrap_or("")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The predefined macros and operators, for `args` (the compile's flags
/// that may change them: without inputs, outputs, `-D`, `-U`, `-include`).
/// `lang`: `c` or `c++`.
pub fn probe(compiler: &Path, args: &[String], lang: &str, cwd: &Path) -> Result<Probe, String> {
    let input: String = OPERATORS.iter().map(|op| format!("#ifdef {op}\n#define {OPERATOR_MARK}{op} 1\n#endif\n")).collect();
    let out = run(compiler, args, lang, &["-dM", "-E"], &input, cwd)?;
    let mut probe = Probe::default();
    for line in out.lines() {
        match line.strip_prefix("#define ").and_then(|l| l.strip_prefix(OPERATOR_MARK)) {
            Some(rest) => probe.operators.push(rest.split(' ').next().unwrap_or_default().to_string()),
            None => {
                probe.predefines.push_str(line);
                probe.predefines.push('\n');
            }
        }
    }
    if !probe.predefines.contains("#define") {
        return Err(format!("{} printed no predefined macros", compiler.display()));
    }
    let deps = run(compiler, args, lang, &["-E", "-M"], "", cwd)?;
    probe.implicit = make_deps(&deps).into_iter().filter(|d| d != "-" && d != "<stdin>").collect();
    Ok(probe)
}

/// Parses a query's answer: an integer literal (`201603L`).
fn integer(s: &str) -> Option<i64> {
    let digits = s.trim().trim_end_matches(['L', 'l', 'U', 'u']);
    digits.parse().ok()
}

/// The answers to queries (operator, argument), with the same flags as
/// `probe`. None for those the compiler didn't evaluate.
pub fn answer(compiler: &Path, args: &[String], lang: &str, cwd: &Path, queries: &[(String, String)]) -> Result<Vec<Option<i64>>, String> {
    let input: String = queries.iter().enumerate().map(|(i, (op, arg))| format!("{QUERY_MARK} {i} {op}({arg})\n")).collect();
    let out = run(compiler, args, lang, &["-E", "-P"], &input, cwd)?;
    let mut answers = vec![None; queries.len()];
    for line in out.lines() {
        let mut parts = line.splitn(3, ' ');
        if parts.next() != Some(QUERY_MARK) {
            continue;
        }
        if let (Some(Ok(i)), Some(v)) = (parts.next().map(str::parse::<usize>), parts.next())
            && i < answers.len()
        {
            answers[i] = integer(v);
        }
    }
    Ok(answers)
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
    }

    /// Against a real compiler, when `CPP_DEPS_TEST_CXX` names one.
    #[test]
    fn real_compiler() {
        let Some(cxx) = std::env::var_os("CPP_DEPS_TEST_CXX") else { return };
        let cwd = std::env::temp_dir();
        let p = probe(Path::new(&cxx), &["-std=c++17".to_string()], "c++", &cwd).unwrap();
        assert!(p.predefines.contains("#define __cplusplus 201703L"), "{}", p.predefines);
        assert!(p.operators.iter().any(|o| o == "__has_include"));
        assert!(p.operators.iter().any(|o| o == "__has_builtin"));
        let a = answer(
            Path::new(&cxx),
            &["-std=c++17".to_string()],
            "c++",
            &cwd,
            &[("__has_builtin".into(), "__builtin_expect".into()), ("__has_cpp_attribute".into(), "nodiscard".into())],
        )
        .unwrap();
        assert_eq!(a[0], Some(1));
        assert!(a[1].is_some_and(|v| v >= 201603));
    }
}
