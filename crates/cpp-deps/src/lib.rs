//! Finds the files a C/C++ compile reads, with a fast preprocessor that
//! evaluates directives exactly and skips everything else.
//!
//! - [`lex`]: a file down to its directives (cached per file version).
//! - [`macros`]: definitions and expansion.
//! - [`expr`]: `#if` expressions.
//! - [`scan`]: a translation unit: conditionals, includes, guards.
//! - [`probe`]: what a compiler predefines and answers to `__has_*`.
//! - [`driver`]: a GCC or Clang command line scanned, with the compiler
//!   probed and its answers cached.
//!
//! Compiler-specific knowledge comes from outside: the predefined macros,
//! the `__has_*` operators the compiler has, and an [`scan::Oracle`]
//! answering their queries (e.g. from [`probe`]).

pub mod driver;
pub mod expr;
pub mod intern;
pub mod lex;
pub mod macros;
pub mod probe;
pub mod scan;

pub use driver::Driver;
pub use scan::{Cache, CommandMacro, Config, NoOracle, Oracle, Output, scan};
