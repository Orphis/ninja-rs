//! A daemon for developers' builds. It keeps what builds learn between
//! them: the preprocessor's lexed headers and compiler profiles, file
//! digests and toolchain input trees. It watches the files they come from,
//! so that a build checks again only the files that changed, instead of
//! every header and toolchain file.
//!
//! - [`watch`]: roots watched recursively (the repositories of the files
//!   checked, and toolchain directories), whose events drop the entries
//!   they concern; what the watcher can't vouch for is checked once per
//!   build, as without the daemon.
//! - [`files`]: file digests and directory trees.
//! - [`server`]: the daemon: one per user and ninja-rs binary, serving
//!   every build until it's idle.
//! - [`client`]: what builds use; a build that can't reach the daemon
//!   does the work itself.
//! - [`proto`], [`ipc`]: length-prefixed JSON over a Unix socket or a
//!   Windows named pipe.

pub mod client;
pub mod files;
pub mod ipc;
pub mod proto;
pub mod server;
pub mod watch;

pub use client::Client;
pub use ipc::Location;
