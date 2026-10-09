// Copied from cargo-rbe (crates/rbe-client, commit a22f361); see ../README.md.
//! Minimal REAPI v2 client: mTLS connection, CAS upload/download, Execute.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use bazel_remote_apis::google::bytestream::{
    ReadRequest, WriteRequest, byte_stream_client::ByteStreamClient,
};
use bazel_remote_apis::google::longrunning::operation;
use prost::Message;
use re::{
    action_cache_client::ActionCacheClient, capabilities_client::CapabilitiesClient,
    content_addressable_storage_client::ContentAddressableStorageClient,
    execution_client::ExecutionClient,
};
use sha2::{Digest as _, Sha256};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// gRPC header carrying [`re::RequestMetadata`] on every REAPI call, as Bazel
/// sends it. Servers use it to group executions by invocation and target.
pub const REQUEST_METADATA_HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";

pub use bazel_remote_apis::build::bazel::remote::execution::v2 as proto;
pub use bazel_remote_apis::google::protobuf as pb;

pub mod chunking;
pub mod tree;
pub use chunking::FastCdc;
pub use tree::{BlobSet, BlobSource, DigestCache, InputTree};

/// gRPC messages are capped at 4 MiB by default; stay well below for batches.
const DEFAULT_MAX_BATCH_BYTES: i64 = 3 * 1024 * 1024;
const BYTESTREAM_CHUNK: usize = 1024 * 1024;

pub struct ConnectOptions {
    /// e.g. `grpcs://glass.cluster.engflow.com`
    pub endpoint: String,
    pub instance_name: String,
    pub client_cert_pem: Option<Vec<u8>>,
    pub client_key_pem: Option<Vec<u8>>,
    pub ca_pem: Option<Vec<u8>>,
    /// Extra gRPC headers sent with every call (`--remote_header`).
    pub headers: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct Client {
    channel: Channel,
    instance: String,
    max_batch_bytes: i64,
    zstd: bool,
    wire_read: Arc<AtomicU64>,
    /// Sent with every call. Per-action fields are set by [`Client::for_action`].
    metadata: Option<Arc<re::RequestMetadata>>,
    /// Extra headers sent with every call.
    headers: Arc<Vec<(tonic::metadata::AsciiMetadataKey, tonic::metadata::AsciiMetadataValue)>>,
}

/// What a call is for: the per-action part of [`re::RequestMetadata`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActionMeta {
    /// e.g. `Rustc`, `RustcMetadata`, `CargoBuildScriptRun`.
    pub mnemonic: String,
    /// The target the action belongs to, as a label.
    pub target_id: String,
    pub configuration_id: String,
}

/// Connects a gRPC channel (TLS for `grpcs://`, with an optional client
/// identity). Shared by the REAPI client and other services (e.g. BES).
pub async fn connect_channel(opts: &ConnectOptions) -> Result<Channel> {
    let (scheme, rest) = opts.endpoint.split_once("://").unwrap_or(("grpcs", opts.endpoint.as_str()));
    let host = rest.split([':', '/']).next().unwrap_or(rest).to_string();
    let (url, tls) = match scheme {
        "grpcs" | "https" => (format!("https://{rest}"), true),
        "grpc" | "http" => (format!("http://{rest}"), false),
        other => bail!("unsupported endpoint scheme `{other}`"),
    };
    let mut endpoint = Endpoint::from_shared(url)?
        .connect_timeout(Duration::from_secs(20))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_while_idle(true);
    if tls {
        let mut cfg = ClientTlsConfig::new().domain_name(host).with_webpki_roots();
        if let Some(ca) = &opts.ca_pem {
            cfg = cfg.ca_certificate(Certificate::from_pem(ca));
        }
        match (&opts.client_cert_pem, &opts.client_key_pem) {
            (Some(cert), Some(key)) => cfg = cfg.identity(Identity::from_pem(cert, key)),
            (None, None) => {}
            _ => bail!("client certificate and key must be given together"),
        }
        endpoint = endpoint.tls_config(cfg)?;
    }
    endpoint.connect().await.with_context(|| format!("connecting to {}", opts.endpoint))
}

pub fn digest_of(data: &[u8]) -> re::Digest {
    re::Digest {
        hash: hex::encode(Sha256::digest(data)),
        size_bytes: data.len() as i64,
    }
}

pub fn digest_of_message<M: Message>(m: &M) -> (re::Digest, Vec<u8>) {
    let bytes = m.encode_to_vec();
    (digest_of(&bytes), bytes)
}

pub fn digest_str(d: &re::Digest) -> String {
    format!("{}/{}", d.hash, d.size_bytes)
}

impl Client {
    pub async fn connect(opts: ConnectOptions) -> Result<Self> {
        let channel = connect_channel(&opts).await.context("connecting to REAPI endpoint")?;
        Self::from_channel(channel, &opts.instance_name).with_headers(&opts.headers)
    }

    /// Sends these extra headers with every call.
    pub fn with_headers(mut self, headers: &[(String, String)]) -> Result<Self> {
        let mut v = Vec::new();
        for (k, val) in headers {
            let key = tonic::metadata::AsciiMetadataKey::from_bytes(k.to_ascii_lowercase().as_bytes())
                .with_context(|| format!("invalid header name `{k}`"))?;
            let value = val.parse().with_context(|| format!("invalid value for header `{k}`"))?;
            v.push((key, value));
        }
        self.headers = Arc::new(v);
        Ok(self)
    }

    /// A client over an existing channel (tests, custom transports).
    pub fn from_channel(channel: Channel, instance: &str) -> Self {
        Self {
            channel,
            instance: instance.to_string(),
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
            zstd: false,
            wire_read: Default::default(),
            metadata: None,
            headers: Arc::new(Vec::new()),
        }
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Sends `metadata` (tool details, invocation ids) with every call.
    pub fn with_request_metadata(mut self, metadata: re::RequestMetadata) -> Self {
        self.metadata = Some(Arc::new(metadata));
        self
    }

    pub fn request_metadata(&self) -> Option<&re::RequestMetadata> {
        self.metadata.as_deref()
    }

    /// A client whose calls are attributed to one action (`action_id` is the
    /// action digest's hash). Cheap: shares the channel and counters.
    pub fn for_action(&self, meta: &ActionMeta, action_id: &str) -> Self {
        let mut m = self.metadata.as_deref().cloned().unwrap_or_default();
        m.action_id = action_id.to_string();
        m.action_mnemonic = meta.mnemonic.clone();
        m.target_id = meta.target_id.clone();
        m.configuration_id = meta.configuration_id.clone();
        Self { metadata: Some(Arc::new(m)), ..self.clone() }
    }

    /// Wraps a message (or a request stream) with the metadata header.
    fn req<T>(&self, message: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(message);
        for (k, v) in self.headers.iter() {
            r.metadata_mut().insert(k.clone(), v.clone());
        }
        if let Some(m) = &self.metadata {
            r.metadata_mut().insert_bin(
                REQUEST_METADATA_HEADER,
                tonic::metadata::MetadataValue::from_bytes(&m.encode_to_vec()),
            );
        }
        r
    }

    pub async fn capabilities(&mut self) -> Result<re::ServerCapabilities> {
        let caps = CapabilitiesClient::new(self.channel.clone())
            .get_capabilities(self.req(re::GetCapabilitiesRequest {
                instance_name: self.instance.clone(),
            }))
            .await?
            .into_inner();
        if let Some(cache) = &caps.cache_capabilities
            && cache.max_batch_total_size_bytes > 0
        {
            self.max_batch_bytes = cache.max_batch_total_size_bytes.min(DEFAULT_MAX_BATCH_BYTES);
        }
        self.zstd = caps
            .cache_capabilities
            .as_ref()
            .is_some_and(|c| c.supported_compressors.contains(&(re::compressor::Value::Zstd as i32)));
        Ok(caps)
    }

    fn cas(&self) -> ContentAddressableStorageClient<Channel> {
        ContentAddressableStorageClient::new(self.channel.clone())
    }

    pub async fn find_missing(&self, digests: Vec<re::Digest>) -> Result<Vec<re::Digest>> {
        let mut missing = Vec::new();
        for chunk in digests.chunks(10_000) {
            let resp = self
                .cas()
                .find_missing_blobs(self.req(re::FindMissingBlobsRequest {
                    instance_name: self.instance.clone(),
                    blob_digests: chunk.to_vec(),
                    digest_function: re::digest_function::Value::Sha256 as i32,
                }))
                .await?
                .into_inner();
            missing.extend(resp.missing_blob_digests);
        }
        Ok(missing)
    }

    /// Uploads every blob the server doesn't already have. Small blobs are
    /// batched, large ones go through ByteStream. Returns (count, bytes) uploaded.
    pub async fn upload_blobs(&self, blobs: &BlobSet) -> Result<(usize, u64)> {
        let digests = blobs.0.values().map(|(d, _)| d.clone()).collect();
        let missing = self.find_missing(digests).await?;
        let mut batch: Vec<re::batch_update_blobs_request::Request> = Vec::new();
        let mut batch_bytes = 0i64;
        let mut total = 0u64;
        for d in &missing {
            let (_, src) = &blobs.0[&d.hash];
            let data = src.load()?;
            total += data.len() as u64;
            if d.size_bytes > self.max_batch_bytes - 1024 {
                self.bytestream_write(d, &data).await?;
                continue;
            }
            if batch_bytes + d.size_bytes > self.max_batch_bytes - 1024 {
                self.flush_batch(std::mem::take(&mut batch)).await?;
                batch_bytes = 0;
            }
            batch_bytes += d.size_bytes;
            batch.push(re::batch_update_blobs_request::Request {
                digest: Some(d.clone()),
                data,
                compressor: 0,
            });
        }
        self.flush_batch(batch).await?;
        Ok((missing.len(), total))
    }

    pub async fn fetch_tree(&self, d: &re::Digest) -> Result<re::Tree> {
        Ok(re::Tree::decode(self.read_blob(d).await?.as_slice())?)
    }

    async fn flush_batch(&self, requests: Vec<re::batch_update_blobs_request::Request>) -> Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        let resp = self
            .cas()
            .batch_update_blobs(self.req(re::BatchUpdateBlobsRequest {
                instance_name: self.instance.clone(),
                requests,
                digest_function: re::digest_function::Value::Sha256 as i32,
            }))
            .await?
            .into_inner();
        for r in resp.responses {
            if let Some(st) = r.status
                && st.code != 0
            {
                bail!("upload of {:?} failed: {} {}", r.digest, st.code, st.message);
            }
        }
        Ok(())
    }

    async fn bytestream_write(&self, d: &re::Digest, data: &[u8]) -> Result<()> {
        let compressed;
        let (kind, data) = if self.zstd {
            compressed = zstd::bulk::compress(data, 3)?;
            ("compressed-blobs/zstd", compressed.as_slice())
        } else {
            ("blobs", data)
        };
        let resource = format!(
            "{}uploads/{}/{kind}/{}/{}",
            self.prefix(),
            upload_id(),
            d.hash,
            d.size_bytes
        );
        let mut reqs = Vec::new();
        let mut offset = 0usize;
        loop {
            let end = (offset + BYTESTREAM_CHUNK).min(data.len());
            reqs.push(WriteRequest {
                resource_name: if offset == 0 { resource.clone() } else { String::new() },
                write_offset: offset as i64,
                finish_write: end == data.len(),
                data: data[offset..end].to_vec(),
            });
            offset = end;
            if offset >= data.len() {
                break;
            }
        }
        ByteStreamClient::new(self.channel.clone())
            .write(self.req(tokio_stream::iter(reqs)))
            .await
            .with_context(|| format!("ByteStream write {}", digest_str(d)))?;
        Ok(())
    }

    pub async fn read_blob(&self, d: &re::Digest) -> Result<Vec<u8>> {
        let mut m = self.read_blobs(std::slice::from_ref(d)).await?;
        m.remove(&d.hash).context("blob missing from read")
    }

    /// Fetches many blobs: small ones through batched `BatchReadBlobs`, large
    /// ones through parallel ByteStream reads. Both use zstd when the server
    /// supports it. Returns blobs keyed by hash.
    pub async fn read_blobs(&self, digests: &[re::Digest]) -> Result<HashMap<String, Vec<u8>>> {
        let mut out = HashMap::new();
        let mut seen = HashSet::new();
        let mut batches: Vec<Vec<re::Digest>> = Vec::new();
        let mut cur: Vec<re::Digest> = Vec::new();
        let mut cur_bytes = 0i64;
        let mut tasks: tokio::task::JoinSet<Result<Vec<(String, Vec<u8>)>>> = tokio::task::JoinSet::new();
        for d in digests {
            if !seen.insert(d.hash.clone()) {
                continue;
            }
            if d.size_bytes == 0 {
                out.insert(d.hash.clone(), Vec::new());
            } else if d.size_bytes > self.max_batch_bytes - 1024 {
                let (c, d) = (self.clone(), d.clone());
                tasks.spawn(async move { Ok(vec![(d.hash.clone(), c.bytestream_read(&d).await?)]) });
            } else {
                if cur_bytes + d.size_bytes > self.max_batch_bytes - 1024 {
                    batches.push(std::mem::take(&mut cur));
                    cur_bytes = 0;
                }
                cur_bytes += d.size_bytes;
                cur.push(d.clone());
            }
        }
        if !cur.is_empty() {
            batches.push(cur);
        }
        for batch in batches {
            let c = self.clone();
            tasks.spawn(async move { c.batch_read(batch).await });
        }
        while let Some(r) = tasks.join_next().await {
            out.extend(r??);
        }
        Ok(out)
    }

    fn compressors(&self) -> Vec<i32> {
        if self.zstd { vec![re::compressor::Value::Zstd as i32] } else { vec![] }
    }

    fn decode(&self, compressor: i32, data: Vec<u8>, size: i64) -> Result<Vec<u8>> {
        self.wire_read.fetch_add(data.len() as u64, Ordering::Relaxed);
        if compressor == re::compressor::Value::Zstd as i32 {
            Ok(zstd::bulk::decompress(&data, size as usize)?)
        } else {
            Ok(data)
        }
    }

    async fn batch_read(&self, digests: Vec<re::Digest>) -> Result<Vec<(String, Vec<u8>)>> {
        let resp = self
            .cas()
            .batch_read_blobs(self.req(re::BatchReadBlobsRequest {
                instance_name: self.instance.clone(),
                digests,
                acceptable_compressors: self.compressors(),
                digest_function: re::digest_function::Value::Sha256 as i32,
            }))
            .await?
            .into_inner();
        let mut out = Vec::with_capacity(resp.responses.len());
        for r in resp.responses {
            let d = r.digest.clone().unwrap_or_default();
            if let Some(st) = &r.status
                && st.code != 0
            {
                bail!("read of {} failed: {} {}", digest_str(&d), st.code, st.message);
            }
            out.push((d.hash.clone(), self.decode(r.compressor, r.data, d.size_bytes)?));
        }
        Ok(out)
    }

    async fn bytestream_read(&self, d: &re::Digest) -> Result<Vec<u8>> {
        let kind = if self.zstd { "compressed-blobs/zstd" } else { "blobs" };
        let mut stream = ByteStreamClient::new(self.channel.clone())
            .read(self.req(ReadRequest {
                resource_name: format!("{}{kind}/{}/{}", self.prefix(), d.hash, d.size_bytes),
                read_offset: 0,
                read_limit: 0,
            }))
            .await
            .with_context(|| format!("ByteStream read {}", digest_str(d)))?
            .into_inner();
        let mut wire = Vec::new();
        while let Some(chunk) = stream.message().await? {
            wire.extend_from_slice(&chunk.data);
        }
        let compressor = if self.zstd { re::compressor::Value::Zstd as i32 } else { 0 };
        self.decode(compressor, wire, d.size_bytes)
    }

    /// Bytes received for blob reads (after compression), since connect.
    pub fn wire_bytes_read(&self) -> u64 {
        self.wire_read.load(Ordering::Relaxed)
    }

    pub async fn get_action_result(&self, action: &re::Digest) -> Result<Option<re::ActionResult>> {
        let res = ActionCacheClient::new(self.channel.clone())
            .get_action_result(self.req(re::GetActionResultRequest {
                instance_name: self.instance.clone(),
                action_digest: Some(action.clone()),
                inline_stdout: true,
                inline_stderr: true,
                inline_output_files: vec![],
                digest_function: re::digest_function::Value::Sha256 as i32,
            }))
            .await;
        match res {
            Ok(r) => Ok(Some(r.into_inner())),
            Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
            Err(s) => Err(s.into()),
        }
    }

    /// Publishes `result` under `action` in the action cache (needs the
    /// server's `update_enabled`). Inlined stdout/stderr are dropped when a
    /// digest is present.
    pub async fn update_action_result(&self, action: &re::Digest, mut result: re::ActionResult) -> Result<()> {
        if result.stdout_digest.is_some() {
            result.stdout_raw.clear();
        }
        if result.stderr_digest.is_some() {
            result.stderr_raw.clear();
        }
        ActionCacheClient::new(self.channel.clone())
            .update_action_result(self.req(re::UpdateActionResultRequest {
                instance_name: self.instance.clone(),
                action_digest: Some(action.clone()),
                action_result: Some(result),
                results_cache_policy: None,
                digest_function: re::digest_function::Value::Sha256 as i32,
            }))
            .await?;
        Ok(())
    }

    /// Runs `Execute` and waits for the operation to finish.
    pub async fn execute(&self, action: &re::Digest, skip_cache_lookup: bool) -> Result<re::ExecuteResponse> {
        let mut stream = ExecutionClient::new(self.channel.clone())
            .execute(self.req(re::ExecuteRequest {
                instance_name: self.instance.clone(),
                skip_cache_lookup,
                action_digest: Some(action.clone()),
                execution_policy: None,
                results_cache_policy: None,
                digest_function: re::digest_function::Value::Sha256 as i32,
                inline_stdout: true,
                inline_stderr: true,
                inline_output_files: vec![],
            }))
            .await?
            .into_inner();
        while let Some(op) = stream.message().await? {
            if !op.done {
                continue;
            }
            return match op.result {
                Some(operation::Result::Response(any)) => {
                    Ok(re::ExecuteResponse::decode(any.value.as_slice())?)
                }
                Some(operation::Result::Error(st)) => {
                    bail!("execution failed: code {} {}", st.code, st.message)
                }
                None => bail!("operation done without result"),
            };
        }
        bail!("Execute stream ended before the operation completed")
    }

    /// stdout/stderr may be inlined or only referenced by digest.
    pub async fn output_bytes(&self, raw: &[u8], digest: Option<&re::Digest>) -> Result<Vec<u8>> {
        match digest {
            Some(d) if raw.is_empty() && d.size_bytes > 0 => self.read_blob(d).await,
            _ => Ok(raw.to_vec()),
        }
    }

    fn prefix(&self) -> String {
        if self.instance.is_empty() {
            String::new()
        } else {
            format!("{}/", self.instance)
        }
    }
}

fn upload_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Must be unique per upload; a UUID-shaped string keeps strict servers happy.
    let h = Sha256::digest(format!("{}-{nanos}-{n}", std::process::id()));
    let x = hex::encode(&h[..16]);
    format!("{}-{}-{}-{}-{}", &x[0..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

/// Helpers to build `Platform` and `Command` with the canonical ordering REAPI requires.
pub fn platform(props: &[(String, String)]) -> re::Platform {
    let mut properties: Vec<_> = props
        .iter()
        .map(|(n, v)| re::platform::Property { name: n.clone(), value: v.clone() })
        .collect();
    properties.sort_by(|a, b| (&a.name, &a.value).cmp(&(&b.name, &b.value)));
    re::Platform { properties }
}

pub fn env_vars(vars: &[(String, String)]) -> Vec<re::command::EnvironmentVariable> {
    let mut v: Vec<_> = vars
        .iter()
        .map(|(n, val)| re::command::EnvironmentVariable { name: n.clone(), value: val.clone() })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_header<T>(r: &tonic::Request<T>) -> Option<re::RequestMetadata> {
        let v = r.metadata().get_bin(REQUEST_METADATA_HEADER)?;
        Some(re::RequestMetadata::decode(v.to_bytes().ok()?.as_ref()).unwrap())
    }

    fn client() -> Client {
        let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        Client::from_channel(channel, "")
    }

    #[tokio::test]
    async fn no_header_without_metadata() {
        assert!(decode_header(&client().req(())).is_none());
    }

    #[tokio::test]
    async fn for_action_keeps_invocation_fields_and_sets_the_action() {
        let base = re::RequestMetadata {
            tool_details: Some(re::ToolDetails { tool_name: "cargo-rbe".into(), tool_version: "0.1.0".into() }),
            tool_invocation_id: "inv".into(),
            correlated_invocations_id: "build".into(),
            ..Default::default()
        };
        let c = client().with_request_metadata(base.clone());
        assert_eq!(decode_header(&c.req(())), Some(base.clone()));

        let meta = ActionMeta {
            mnemonic: "Rustc".into(),
            target_id: "@crates__serde-1.0.219//:lib/serde".into(),
            configuration_id: "release-x86_64-unknown-linux-gnu".into(),
        };
        let a = c.for_action(&meta, "abc123");
        let got = decode_header(&a.req(())).unwrap();
        assert_eq!(got.tool_details, base.tool_details);
        assert_eq!(got.tool_invocation_id, "inv");
        assert_eq!(got.correlated_invocations_id, "build");
        assert_eq!(got.action_id, "abc123");
        assert_eq!(got.action_mnemonic, "Rustc");
        assert_eq!(got.target_id, meta.target_id);
        assert_eq!(got.configuration_id, meta.configuration_id);
        // The original client is unchanged.
        assert_eq!(decode_header(&c.req(())), Some(base));
    }
}
