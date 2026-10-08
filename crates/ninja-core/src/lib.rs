//! ninja-core: a Rust implementation of ninja's manifest language, build
//! graph and execution semantics.
//!
//! The engine is written to be embedded in a long-running service: there is
//! no global state, all filesystem access goes through [`disk::Disk`] and
//! commands are executed by a [`runner::CommandRunner`].

pub mod build;
pub mod build_log;
pub mod canon;
pub mod clparser;
pub mod depfile;
pub mod deps_log;
pub mod disk;
pub mod dyndep;
pub mod eval;
pub mod graph;
pub mod hash;
pub mod lexer;
pub mod parser;
pub mod runner;
pub mod shell;
pub mod status;
pub mod subprocess;

#[cfg(test)]
mod build_test;
#[cfg(test)]
mod parser_test;
