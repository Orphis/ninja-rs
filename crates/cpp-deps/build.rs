//! The tests against real compilers run when variables name them, and are
//! reported ignored otherwise (rather than passing without testing
//! anything): `CPP_DEPS_TEST_CXX` (a GCC or Clang), `CPP_DEPS_TEST_CL`
//! (cl.exe, with INCLUDE set), `CPP_DEPS_TEST_CLANG_CL`, and the libraries
//! to test with them (`CPP_DEPS_TEST_LIBCXX`, `CPP_DEPS_TEST_BOOST`,
//! `CPP_DEPS_TEST_GLIB`) each set the cfg of the same name in lower case. Bazel builds don't run this: BUILD.bazel
//! gives the cfgs, and the hermetic toolchain's compilers.

#![allow(clippy::disallowed_macros)] // Cargo reads its directives on stdout.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for var in ["CPP_DEPS_TEST_CXX", "CPP_DEPS_TEST_CL", "CPP_DEPS_TEST_CLANG_CL", "CPP_DEPS_TEST_LIBCXX", "CPP_DEPS_TEST_BOOST", "CPP_DEPS_TEST_GLIB"] {
        let cfg = var.to_lowercase();
        println!("cargo:rustc-check-cfg=cfg({cfg})");
        println!("cargo:rerun-if-env-changed={var}");
        if std::env::var_os(var).is_some_and(|v| !v.is_empty()) {
            println!("cargo:rustc-cfg={cfg}");
        }
    }
}
