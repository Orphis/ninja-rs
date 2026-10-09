//! Remote execution (REAPI v2) for ninja-rs.
//!
//! - [`backend::Backend`] implements the engine's `RemoteBackend`: the
//!   execution policy, action cache lookups, uploads, `Execute` and output
//!   downloads.
//! - [`action`] turns a ninja task into a REAPI action.
//! - [`cmdline`] splits command lines into the commands they run.
//! - [`scanner`] finds the headers a C/C++ compile reads.
//! - [`paths`] maps local paths into the remote input root and back.
//! - [`toolchains`] reads the `rbe_toolchains` file.
//! - [`targets`] reads the `rbe_targets` file.
//! - [`store::Store`] holds the caches shared across actions and builds.

pub mod action;
pub mod backend;
pub mod cmdline;
pub mod paths;
pub mod scanner;
pub mod store;
pub mod targets;
pub mod toolchains;

pub use backend::{Backend, BackendConfig};
pub use rbe_client::Client;
pub use store::Store;

#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// `grpc://host:port` or `grpcs://host:port`.
    pub endpoint: String,
    pub instance_name: String,
    pub headers: Vec<(String, String)>,
}

/// Connects to a REAPI endpoint. TLS (`grpcs://`) uses the web PKI roots;
/// no credentials are configured here (use `--remote_header` for tokens).
pub async fn connect(opts: &ConnectOptions) -> anyhow::Result<Client> {
    Client::connect(rbe_client::ConnectOptions {
        endpoint: opts.endpoint.clone(),
        instance_name: opts.instance_name.clone(),
        client_cert_pem: None,
        client_key_pem: None,
        ca_pem: None,
        headers: opts.headers.clone(),
    })
    .await
}
