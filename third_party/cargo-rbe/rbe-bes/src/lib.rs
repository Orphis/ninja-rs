//! Build Event Service client.
//!
//! - [`proto`]: generated BES service and Bazel BEP messages.
//! - [`uploader`]: streams events to a BES backend (acks, retries, lifecycle events).
//! - [`events`]: builds a valid BEP event stream (announcements, progress chain, last message).

#[allow(clippy::all, clippy::pedantic, rustdoc::all)]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

pub mod events;
pub mod uploader;
