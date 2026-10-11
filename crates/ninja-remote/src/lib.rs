//! Remote execution (REAPI v2) for ninja-rs.
//!
//! - [`backend::Backend`] implements the engine's `RemoteBackend`: the
//!   execution policy, action cache lookups, uploads, `Execute` and output
//!   downloads.
//! - [`action`] turns a ninja task into a REAPI action.
//! - [`cmdline`] splits command lines into the commands they run.
//! - [`scanner`] finds the headers a C/C++ compile reads.
//! - [`link`] finds what a link reads, and [`archive`] the members of thin
//!   archives.
//! - [`paths`] maps local paths into the remote input root and back.
//! - [`toolchains`] reads the `rbe_toolchains` file.
//! - [`targets`] reads the `rbe_targets` file.
//! - [`store::Store`] holds the caches shared across actions and builds.
//! - [`disk_cache::DiskCache`] is the local action cache and CAS.

pub mod action;
pub mod archive;
pub mod backend;
pub mod cmdline;
pub mod disk_cache;
pub mod link;
pub mod paths;
pub mod remote_outputs;
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
    pub tls: Tls,
}

/// TLS settings for `grpcs://` endpoints (unused for `grpc://`): servers
/// are checked against the web PKI roots plus `ca`; a client certificate
/// and its key authenticate the client (mTLS). PEM contents.
#[derive(Clone, Debug, Default)]
pub struct Tls {
    pub ca: Option<Vec<u8>>,
    pub client_cert: Option<Vec<u8>>,
    pub client_key: Option<Vec<u8>>,
    /// The name the server's certificate must have, instead of the
    /// endpoint's host.
    pub authority_override: Option<String>,
}

impl Tls {
    /// Connection options for `endpoint` with these settings.
    pub fn connect_options(&self, endpoint: &str, instance_name: &str, headers: &[(String, String)]) -> rbe_client::ConnectOptions {
        rbe_client::ConnectOptions {
            endpoint: endpoint.to_string(),
            instance_name: instance_name.to_string(),
            client_cert_pem: self.client_cert.clone(),
            client_key_pem: self.client_key.clone(),
            ca_pem: self.ca.clone(),
            authority_override: self.authority_override.clone(),
            headers: headers.to_vec(),
        }
    }
}

/// Connects to a REAPI endpoint (`--remote_header` carries tokens, `tls`
/// certificates).
pub async fn connect(opts: &ConnectOptions) -> anyhow::Result<Client> {
    Client::connect(opts.tls.connect_options(&opts.endpoint, &opts.instance_name, &opts.headers)).await
}
