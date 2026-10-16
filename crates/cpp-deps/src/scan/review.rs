//! Adversarial coverage checks: accepted scans must contain the compiler's
//! inputs. Unknown values and pragma replay must never hide a dependency.

use super::*;

fn cases() -> Vec<(&'static str, String)> {
    let prefix = r#"#define X 1
#define PUSH _Pragma("push_macro(\"X\")")
#define POP _Pragma("pop_macro(\"X\")")
#define IGNORE(x)
PUSH
#undef X
#define X 0
"#;
    let mut out = vec![
        ("inactive_parens", format!("{prefix}#if 0\n)\n#endif\nIGNORE(\nPOP\n)\n#if X == 0\n#include \"actual.h\"\n#endif\n")),
        ("macro_parens", format!("{prefix}#define OPEN IGNORE(\nOPEN\nPOP\n)\n#if X == 0\n#include \"actual.h\"\n#endif\n")),
        ("wrapper_parens", format!("{prefix}#define OPEN IGNORE(\n#define WRAP OPEN\nWRAP\nPOP\n)\n#if X == 0\n#include \"actual.h\"\n#endif\n")),
        ("late_parens", format!("{prefix}#define WRAP OPEN\n#define OPEN IGNORE(\nWRAP\nPOP\n)\n#if X == 0\n#include \"actual.h\"\n#endif\n")),
        ("pasted_parens", format!("{prefix}#define CAT(a,b) a##b\n#define OPEN IGNORE(\nCAT(OP,EN)\nPOP\n)\n#if X == 0\n#include \"actual.h\"\n#endif\n")),
        ("computed_pop", format!("{prefix}#define OPERAND \"pop_macro(\\\"X\\\")\"\n_Pragma(OPERAND)\n#if X == 1\n#include \"actual.h\"\n#endif\n")),
        ("pasted_operator", format!("{prefix}#define OPERAND \"pop_macro(\\\"X\\\")\"\n#define CAT(a,b) a##b\nCAT(_Pra,gma)(OPERAND)\n#if X == 1\n#include \"actual.h\"\n#endif\n")),
        ("numeric_pragma_fragment", format!("{prefix}#define OPERAND \"pop_macro(\\\"X\\\")\"\n#define DP1(x) _Pragma(x)\n#define ONE 1\n#define CAT(a,b) a##b\n#define XCAT(a,b) CAT(a,b)\nXCAT(DP,ONE)(OPERAND)\n#if X == 1\n#include \"actual.h\"\n#endif\n")),
        ("function_pop", format!("{prefix}#define FPOP() _Pragma(\"pop_macro(\\\"X\\\")\")\nFPOP()\n#if X == 1\n#include \"actual.h\"\n#endif\n")),
        ("suppressed_literal", format!("{prefix}IGNORE(_Pragma(\"pop_macro(\\\"X\\\")\"))\n#if X == 0\n#include \"actual.h\"\n#endif\n")),
        ("unknown_true_type", "#if (1 ? -1 : (__COUNTER__ + 0U)) < 0\n#else\n#include \"actual.h\"\n#endif\n".into()),
        ("unknown_false_type", "#if (0 ? (__COUNTER__ + 0U) : -1) < 0\n#else\n#include \"actual.h\"\n#endif\n".into()),
        ("conditional_comma", "#if (1 || __COUNTER__ ? 0 : 0, 1)\n#include \"actual.h\"\n#endif\n".into()),
        ("pragma_condition", "#define X 1\n#pragma push_macro(\"X\")\n#undef X\n#define X 0\n#define POPVAL _Pragma(\"pop_macro(\\\"X\\\")\") 0\n#if POPVAL\n#endif\n#if X == 1\n#include \"actual.h\"\n#endif\n".into()),
        ("undefined_pusher", "#define X 1\n#pragma push_macro(\"X\")\n#undef X\n#define X 0\n#if __COUNTER__ == 0\n#define POP _Pragma(\"pop_macro(\\\"X\\\")\")\n#else\n#undef POP\n#endif\nPOP\n#if X == 1\n#include \"actual.h\"\n#endif\n".into()),
        ("alternate_target", "#define X 1\n#pragma push_macro(\"X\")\n#undef X\n#define X 0\n#if __COUNTER__ == 0\n#define POP _Pragma(\"pop_macro(\\\"X\\\")\")\n#else\n#define POP _Pragma(\"pop_macro(\\\"Y\\\")\")\n#endif\nPOP\n#if X == 1\n#include \"actual.h\"\n#endif\n".into()),
        ("suppressed_once", "#if __COUNTER__ == 0\n#include \"once.h\"\n#endif\n#include \"once.h\"\n#define AGAIN\n#include \"once.h\"\n".into()),
        ("alternate_context", "#define X 1\n#pragma push_macro(\"X\")\n#undef X\n#define X 0\n#define IGNORE(x)\n#if __COUNTER__ == 0\n#define OPEN IGNORE(\n#else\n#define OPEN\n#endif\n#define POP _Pragma(\"pop_macro(\\\"X\\\")\")\nOPEN\nPOP\n)\n#if X == 0\n#include \"actual.h\"\n#endif\n".into()),
        ("restored_context", "#define IGNORE(x)\n#define OPEN IGNORE(\n#pragma push_macro(\"OPEN\")\n#undef OPEN\n#define OPEN\n#define X 1\n#define PUSH _Pragma(\"push_macro(\\\"X\\\")\")\n#define POP _Pragma(\"pop_macro(\\\"X\\\")\")\nPUSH\n#undef X\n#define X 0\n#define RESTORE() _Pragma(\"pop_macro(\\\"OPEN\\\")\")\nRESTORE()\nOPEN\nPOP\n)\n#if X == 0\n#include \"actual.h\"\n#endif\n".into()),
    ];
    // Existence dependencies aren't listed by -MD or /showIncludes.
    let existence: Vec<_> = out.iter().map(|(name, text)| (*name, text.replace("#include \"actual.h\"", "#if __has_include(\"actual.h\")\n#endif"))).collect();
    out.extend(existence);
    out
}

fn fixtures(dir: &Path) {
    std::fs::write(dir.join("actual.h"), "int actual;\n").unwrap();
    std::fs::write(dir.join("once.h"), "#define IGNORE(x)\nIGNORE(_Pragma(\"once\"))\n#ifdef AGAIN\n#include \"actual.h\"\n#endif\n").unwrap();
}

#[test]
fn accepted_tracking_covers_reads_and_existence() {
    let dir = test_dirs::TestDir::new("cpp-deps-tracking-review");
    let header = dir.join("actual.h");
    fixtures(&dir);
    let cfg = Config { dialect: expr::Dialect::CXX, ..Default::default() };
    let mut failures = Vec::new();
    for (i, (name, source)) in cases().into_iter().enumerate() {
        let main = dir.join(format!("{name}_{i}.c"));
        std::fs::write(&main, source).unwrap();
        let out = scan(&Cache::new(vec![], cfg!(windows)), &cfg, &NoOracle, &main);
        if out.exact() && out.missing.is_empty() && !out.files.contains(&header) && !out.found.contains(&header) {
            failures.push(format!("{name}: complete scan omitted actual.h (union {:?})", out.union));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn compare(compiler: &str, cl: bool, msvc: bool) {
    let dir = test_dirs::TestDir::new("cpp-deps-tracking-review-real");
    fixtures(&dir);
    let driver = crate::Driver::new(Cache::new(vec![], cfg!(windows)));
    let key = |p: &Path| {
        let s = normalize(p).to_string_lossy().replace('\\', "/");
        if cfg!(windows) { s.to_ascii_lowercase() } else { s }
    };
    let cases = cases();
    let reads = cases.len() / 2;
    for (i, (name, mut source)) in cases.into_iter().take(reads).enumerate() {
        if msvc {
            for var in ["X", "Y"] {
                for op in ["push", "pop"] {
                    source = source.replace(&format!("_Pragma(\"{op}_macro(\\\"{var}\\\")\")"), &format!("__pragma({op}_macro(\"{var}\"))"));
                }
            }
            // cl's spelling; a computed _Pragma string is Clang/GCC only.
            source = source.replace("_Pragma(\"once\")", "__pragma(once)");
            if source.contains("_Pragma") || matches!(name, "suppressed_once" | "conditional_comma" | "pragma_condition") {
                continue;
            }
        }
        let file = format!("{name}_{i}.c");
        std::fs::write(dir.join(&file), source).unwrap();
        driver.cache.new_session();
        let argv: Vec<_> = [compiler, if cl { "/X" } else { "-nostdinc" }, if cl { "/c" } else { "-c" }, &file].into_iter().map(str::to_string).collect();
        let out = driver.scan(&argv, &dir).unwrap();
        let reference = crate::external::compiler_deps(&argv, &dir, None).unwrap();
        if !msvc {
            assert!(reference.iter().any(|f| f.ends_with("actual.h")), "{name}: the compiler didn't read the expected header");
        }
        if out.exact() && out.missing.is_empty() {
            for file in reference {
                assert!(
                    out.files.iter().chain(&out.found).any(|p| key(p) == key(&file)),
                    "{name}: complete scan omitted {} (found {:?})",
                    file.display(),
                    out.files
                );
            }
        }
    }
}

#[test]
#[cfg_attr(not(cpp_deps_test_cxx), ignore = "needs CPP_DEPS_TEST_CXX")]
fn real_tracking_review() {
    compare(&crate::test_compiler("CPP_DEPS_TEST_CXX").to_string_lossy(), false, false);
}

#[test]
#[cfg_attr(not(cpp_deps_test_clang_cl), ignore = "needs CPP_DEPS_TEST_CLANG_CL")]
fn real_clang_cl_tracking_review() {
    compare(&crate::test_compiler("CPP_DEPS_TEST_CLANG_CL").to_string_lossy(), true, false);
}

#[test]
#[cfg_attr(not(cpp_deps_test_cl), ignore = "needs CPP_DEPS_TEST_CL")]
fn real_cl_tracking_review() {
    compare(&crate::test_compiler("CPP_DEPS_TEST_CL").to_string_lossy(), true, true);
}

fn generated(compiler: &str, cl: bool) {
    let dir = test_dirs::TestDir::new("cpp-deps-tracking-review-generated");
    for step in 0..6 {
        for var in ["X", "Y"] {
            for value in [0, 1] {
                std::fs::write(dir.join(format!("{var}_{step}_{value}.h")), "").unwrap();
            }
        }
        std::fs::write(dir.join(format!("probe_{step}.h")), "").unwrap();
        std::fs::write(dir.join(format!("hit_{step}.h")), "").unwrap();
    }
    // Create all files before caching any directory listing.
    for seed in 1..=128u64 {
        let mut rng = seed;
        let mut source = String::from(
            r#"#define X 0
#define Y 1
#define PUSHX _Pragma("push_macro(\"X\")")
#define POPX _Pragma("pop_macro(\"X\")")
#define PUSHY _Pragma("push_macro(\"Y\")")
#define POPY _Pragma("pop_macro(\"Y\")")
#define IGNORE(x)
"#,
        );
        for step in 0..6 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            source.push_str(match rng % 12 {
                0 => "PUSHX\n",
                1 => "POPX\n",
                2 => "#undef X\n#define X 1\n",
                3 => "#if __COUNTER__ % 2\n#undef X\n#define X 1\n#else\n#undef X\n#define X 0\n#endif\n",
                4 => "#if __COUNTER__ % 2\n#undef ACTION\n#define ACTION POPX\n#else\n#undef ACTION\n#define ACTION POPY\n#endif\n",
                5 => "#ifdef ACTION\nACTION\n#endif\n",
                6 => "#define CMD \"pop_macro(\\\"X\\\")\"\n_Pragma(CMD)\n",
                7 => "IGNORE(_Pragma(\"pop_macro(\\\"X\\\")\"))\n",
                8 => "#define OPEN IGNORE(\nOPEN\nPOPX\n)\n",
                9 => "PUSHY\n#undef Y\n#define Y 0\n",
                10 => "POPY\n",
                _ => "#if __COUNTER__ % 2\n#define ACTION POPX\n#else\n#undef ACTION\n#endif\n#ifdef ACTION\nACTION\n#endif\n",
            });
            for var in ["X", "Y"] {
                source.push_str(&format!("#if {var} == 0\n#include \"{var}_{step}_0.h\"\n#else\n#include \"{var}_{step}_1.h\"\n#endif\n"));
            }
            source.push_str(&format!("#if X == 0\n#if __has_include(\"probe_{step}.h\")\n#include \"hit_{step}.h\"\n#endif\n#endif\n"));
        }
        std::fs::write(dir.join(format!("seed{seed}.c")), source).unwrap();
    }
    let driver = crate::Driver::new(Cache::new(vec![], cfg!(windows)));
    let key = |p: &Path| {
        let s = normalize(p).to_string_lossy().replace('\\', "/");
        if cfg!(windows) { s.to_ascii_lowercase() } else { s }
    };
    let mut accepted = 0;
    for seed in 1..=128 {
        let file = format!("seed{seed}.c");
        let argv: Vec<_> = [compiler, if cl { "/X" } else { "-nostdinc" }, if cl { "/c" } else { "-c" }, &file].into_iter().map(str::to_string).collect();
        let out = driver.scan(&argv, &dir).unwrap();
        let reference = crate::external::compiler_deps(&argv, &dir, None).unwrap();
        if !(out.exact() && out.missing.is_empty()) {
            continue;
        }
        accepted += 1;
        for file in &reference {
            assert!(out.files.iter().chain(&out.found).any(|p| key(p) == key(file)), "seed {seed}: omitted {}", file.display());
            // The hit proves the compiler tested the corresponding file;
            // dependency lists omit that existence dependency.
            if let Some(step) = file.file_stem().unwrap().to_str().unwrap().strip_prefix("hit_") {
                let probe = dir.join(format!("probe_{step}.h"));
                assert!(out.files.iter().chain(&out.found).any(|p| key(p) == key(&probe)), "seed {seed}: omitted existence dependency {}", probe.display());
            }
        }
    }
    assert!(accepted >= 64, "only {accepted}/128 accepted; the test must exercise complete scans");
}

#[test]
#[cfg_attr(not(cpp_deps_test_cxx), ignore = "needs CPP_DEPS_TEST_CXX")]
fn real_generated_tracking_review() {
    generated(&crate::test_compiler("CPP_DEPS_TEST_CXX").to_string_lossy(), false);
}

#[test]
#[cfg_attr(not(cpp_deps_test_clang_cl), ignore = "needs CPP_DEPS_TEST_CLANG_CL")]
fn real_clang_cl_generated_tracking_review() {
    generated(&crate::test_compiler("CPP_DEPS_TEST_CLANG_CL").to_string_lossy(), true);
}
