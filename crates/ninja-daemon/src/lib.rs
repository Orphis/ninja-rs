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

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Locks taken though a panic left them poisoned: a request that panics is
/// answered with an error (see `server::Daemon::answer`), and every later
/// one used to fail for it. What they guard is whole between two uses (a
/// map updated in one call); what a panic may have left half done (entries
/// some listeners dropped, others not) goes with everything vouched for
/// then.
pub(crate) trait Locked<T: ?Sized> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> Locked<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `Locked`, for readers and writers.
pub(crate) trait RwLocked<T: ?Sized> {
    fn reading(&self) -> RwLockReadGuard<'_, T>;
    fn writing(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLocked<T> for RwLock<T> {
    fn reading(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn writing(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}
