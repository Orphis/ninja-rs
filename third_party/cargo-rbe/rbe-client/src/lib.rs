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
pub use chunking::{Chunking, FastCdc};
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
    /// The name the server's certificate is checked against (and sent as
    /// SNI) instead of the endpoint's host.
    pub authority_override: Option<String>,
    /// Extra gRPC headers sent with every call (`--remote_header`).
    pub headers: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct Client {
    channel: Channel,
    instance: String,
    max_batch_bytes: i64,
    /// Whether blobs may travel compressed (`with_compression`).
    compression: bool,
    /// zstd for ByteStream reads and writes and batch reads, and for batch
    /// updates (the server lists those separately).
    zstd: bool,
    batch_zstd: bool,
    /// What the server supports of split and spliced blobs.
    chunking: Arc<Chunking>,
    transfers: Arc<TransferCounters>,
    /// Sent with every call. Per-action fields are set by [`Client::for_action`].
    metadata: Option<Arc<re::RequestMetadata>>,
    /// Extra headers sent with every call.
    headers: Arc<Vec<(tonic::metadata::AsciiMetadataKey, tonic::metadata::AsciiMetadataValue)>>,
}

/// Blobs smaller than this travel uncompressed: too little to gain.
const MIN_COMPRESSED: usize = 1024;

/// Chunks of one blob uploaded at once through ByteStream (as Bazel's
/// `ChunkedBlobUploader`).
const CHUNK_STREAMS: usize = 16;

#[derive(Default)]
struct TransferCounters {
    read: AtomicU64,
    wire_read: AtomicU64,
    sent: AtomicU64,
    wire_sent: AtomicU64,
}

/// Blob bytes read from and sent to the CAS since connecting, as blobs and
/// as they travelled (compressed or not).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transfers {
    pub read: u64,
    pub wire_read: u64,
    pub sent: u64,
    pub wire_sent: u64,
}

/// `data` compressed with zstd when that makes it smaller, for sending.
fn compressed(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < MIN_COMPRESSED {
        return None;
    }
    zstd::bulk::compress(data, 3).ok().filter(|c| c.len() < data.len())
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
        let domain = opts.authority_override.clone().unwrap_or(host);
        let mut cfg = ClientTlsConfig::new().domain_name(domain).with_webpki_roots();
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

/// Checks that `data` is the blob `d`: its size and SHA-256. Bytes that
/// aren't (a server bug, corruption, a truncated stream) must not become
/// outputs or cache entries.
pub fn verify(d: &re::Digest, data: &[u8]) -> Result<()> {
    if data.len() as i64 != d.size_bytes {
        bail!("blob {}: received {} bytes", digest_str(d), data.len());
    }
    let hash = hex::encode(Sha256::digest(data));
    if hash != d.hash {
        bail!("blob {}: received data hashing to {hash}", digest_str(d));
    }
    Ok(())
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
            compression: true,
            zstd: false,
            batch_zstd: false,
            chunking: Default::default(),
            transfers: Default::default(),
            metadata: None,
            headers: Arc::new(Vec::new()),
        }
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Whether blobs may travel compressed with zstd, when the server
    /// supports it (known after [`Client::capabilities`]). On by default.
    pub fn with_compression(mut self, on: bool) -> Self {
        self.compression = on;
        self.zstd &= on;
        self.batch_zstd &= on;
        self
    }

    /// Whether blobs travel compressed (ByteStream, batch reads), and
    /// whether batch uploads do.
    pub fn compressing(&self) -> (bool, bool) {
        (self.zstd, self.batch_zstd)
    }

    /// Blob bytes transferred since connecting.
    pub fn transfers(&self) -> Transfers {
        let t = &self.transfers;
        Transfers {
            read: t.read.load(Ordering::Relaxed),
            wire_read: t.wire_read.load(Ordering::Relaxed),
            sent: t.sent.load(Ordering::Relaxed),
            wire_sent: t.wire_sent.load(Ordering::Relaxed),
        }
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
        let zstd = re::compressor::Value::Zstd as i32;
        let cache = caps.cache_capabilities.as_ref();
        self.zstd = self.compression && cache.is_some_and(|c| c.supported_compressors.contains(&zstd));
        self.batch_zstd = self.compression && cache.is_some_and(|c| c.supported_batch_update_compressors.contains(&zstd));
        self.chunking = Arc::new(Chunking::from_capabilities(&caps));
        Ok(caps)
    }

    /// What the server supports of split and spliced blobs (known after
    /// [`Client::capabilities`]).
    pub fn chunking(&self) -> &Chunking {
        &self.chunking
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
    /// batched, large ones go through ByteStream, or, when the server
    /// splices blobs, as their chunks (only those it lacks), then spliced.
    /// Chunks are batched too, unless the server compresses ByteStream
    /// writes and not batches: then each goes through ByteStream,
    /// compressed, several at once, as Bazel uploads them.
    /// Returns the number of blobs uploaded and the bytes sent.
    pub async fn upload_blobs(&self, blobs: &BlobSet) -> Result<(usize, u64)> {
        let digests = blobs.0.values().map(|(d, _)| d.clone()).collect();
        let missing = self.find_missing(digests).await?;
        // Blobs that only existed in the CAS can't be uploaded again.
        let lost: Vec<std::path::PathBuf> = missing
            .iter()
            .filter_map(|d| match &blobs.0[&d.hash].1 {
                BlobSource::Remote(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        if !lost.is_empty() {
            return Err(MissingRemoteBlobs(lost).into());
        }
        let mut up = Uploader::new(self);
        let mut spliced = Vec::new();
        for d in &missing {
            let (_, src) = &blobs.0[&d.hash];
            let data = src.load()?;
            if let Some(cdc) = self.chunking.splice_chunker(d) {
                let chunks: Vec<(re::Digest, &[u8])> = cdc.chunks(&data).map(|c| (digest_of(c), c)).collect();
                let mut lacking: HashSet<String> =
                    self.find_missing(chunks.iter().map(|(cd, _)| cd.clone()).collect()).await?.into_iter().map(|cd| cd.hash).collect();
                if self.zstd && !self.batch_zstd {
                    let mut streams = tokio::task::JoinSet::new();
                    for (cd, c) in &chunks {
                        if !lacking.remove(&cd.hash) {
                            continue;
                        }
                        if streams.len() >= CHUNK_STREAMS {
                            streams.join_next().await.expect("uploads in flight")??;
                        }
                        up.sent += c.len() as u64;
                        let (client, cd, c) = (self.clone(), cd.clone(), c.to_vec());
                        streams.spawn(async move { client.bytestream_write(&cd, &c).await });
                    }
                    while let Some(r) = streams.join_next().await {
                        r??;
                    }
                } else {
                    for (cd, c) in &chunks {
                        if lacking.remove(&cd.hash) {
                            up.add(cd, c.to_vec()).await?;
                        }
                    }
                }
                spliced.push((d.clone(), chunks.into_iter().map(|(cd, _)| cd).collect::<Vec<_>>()));
                continue;
            }
            up.add(d, data).await?;
        }
        up.flush().await?;
        // Once all the chunks are in the CAS. A server refusing the splice
        // gets the whole blob.
        for (d, chunks) in spliced {
            if self.splice_blob(&d, &chunks).await.is_err() {
                up.add(&d, blobs.0[&d.hash].1.load()?).await?;
            }
        }
        up.flush().await?;
        Ok((missing.len(), up.sent))
    }

    /// The chunks a blob is made of, from the server (`GetChunkMapping`, or
    /// `SplitBlob` before RE API v2.13), and the function that split it.
    /// `None` when it doesn't split blobs or can't split this one.
    pub async fn split_blob(&self, d: &re::Digest) -> Result<Option<re::SplitBlobResponse>> {
        if !self.chunking.split {
            return Ok(None);
        }
        let not_found = |s: &tonic::Status| s.code() == tonic::Code::NotFound;
        let split = if self.chunking.v2_13 {
            let request = re::GetChunkMappingRequest {
                instance_name: self.instance.clone(),
                blob_digest: Some(d.clone()),
                digest_function: re::digest_function::Value::Sha256 as i32,
                chunking_function: self.chunking.function(),
            };
            let mut stream = match self.cas().get_chunk_mapping(self.req(request)).await {
                Ok(r) => r.into_inner(),
                Err(s) if not_found(&s) => return Ok(None),
                Err(s) => return Err(anyhow::Error::from(s).context(format!("GetChunkMapping {}", digest_str(d)))),
            };
            let mut split = re::SplitBlobResponse::default();
            let mut first = true;
            loop {
                match stream.message().await {
                    Ok(Some(m)) => {
                        // The function comes with the first message.
                        if std::mem::take(&mut first) {
                            split.chunking_function = m.chunking_function;
                        }
                        split.chunk_digests.extend(m.chunk_digests);
                    }
                    Ok(None) => break,
                    Err(s) if not_found(&s) => return Ok(None),
                    Err(s) => return Err(anyhow::Error::from(s).context(format!("GetChunkMapping {}", digest_str(d)))),
                }
            }
            split
        } else {
            let request = re::SplitBlobRequest {
                instance_name: self.instance.clone(),
                blob_digest: Some(d.clone()),
                digest_function: re::digest_function::Value::Sha256 as i32,
                chunking_function: self.chunking.function(),
            };
            match self.cas().split_blob(self.req(request)).await {
                Ok(r) => r.into_inner(),
                Err(s) if not_found(&s) => return Ok(None),
                Err(s) => return Err(anyhow::Error::from(s).context(format!("SplitBlob {}", digest_str(d)))),
            }
        };
        if split.chunk_digests.iter().map(|c| c.size_bytes).sum::<i64>() != d.size_bytes {
            bail!("the chunks of {} don't add up to its size", digest_str(d));
        }
        Ok(Some(split))
    }

    /// Tells the server that a blob is the concatenation of chunks it holds
    /// (`RegisterChunkMapping`, or `SpliceBlob` before RE API v2.13).
    pub async fn splice_blob(&self, d: &re::Digest, chunks: &[re::Digest]) -> Result<()> {
        let context = || format!("splicing {}", digest_str(d));
        if self.chunking.v2_13 {
            // The list may be long: spread it over messages, the first one
            // naming the blob.
            let mut requests: Vec<re::RegisterChunkMappingRequest> = chunks
                .chunks(10_000)
                .map(|part| re::RegisterChunkMappingRequest { chunk_digests: part.to_vec(), ..Default::default() })
                .collect();
            if requests.is_empty() {
                requests.push(Default::default());
            }
            let first = &mut requests[0];
            first.instance_name = self.instance.clone();
            first.blob_digest = Some(d.clone());
            first.digest_function = re::digest_function::Value::Sha256 as i32;
            first.chunking_function = self.chunking.function();
            self.cas().register_chunk_mapping(self.req(tokio_stream::iter(requests))).await.with_context(context)?;
        } else {
            let request = re::SpliceBlobRequest {
                instance_name: self.instance.clone(),
                blob_digest: Some(d.clone()),
                chunk_digests: chunks.to_vec(),
                digest_function: re::digest_function::Value::Sha256 as i32,
                chunking_function: self.chunking.function(),
            };
            self.cas().splice_blob(self.req(request)).await.with_context(context)?;
        }
        Ok(())
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
        let packed = if self.zstd { compressed(data) } else { None };
        let (kind, data) = match &packed {
            Some(c) => ("compressed-blobs/zstd", c.as_slice()),
            None => ("blobs", data),
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
        self.transfers.sent.fetch_add(d.size_bytes as u64, Ordering::Relaxed);
        self.transfers.wire_sent.fetch_add(data.len() as u64, Ordering::Relaxed);
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

    /// The blob `d` as read: decompressed, and checked to be `d`.
    fn decode(&self, compressor: i32, data: Vec<u8>, d: &re::Digest) -> Result<Vec<u8>> {
        let size = d.size_bytes;
        self.transfers.wire_read.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.transfers.read.fetch_add(size.max(0) as u64, Ordering::Relaxed);
        let data = if compressor == re::compressor::Value::Zstd as i32 {
            zstd::bulk::decompress(&data, size.max(0) as usize).with_context(|| format!("decompressing {}", digest_str(d)))?
        } else {
            data
        };
        verify(d, &data)?;
        Ok(data)
    }

    async fn batch_read(&self, digests: Vec<re::Digest>) -> Result<Vec<(String, Vec<u8>)>> {
        let requested: HashSet<(String, i64)> = digests.iter().map(|d| (d.hash.clone(), d.size_bytes)).collect();
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
            // Only what was asked for (whose size bounds what is allocated).
            if !requested.contains(&(d.hash.clone(), d.size_bytes)) {
                bail!("BatchReadBlobs returned {}, which wasn't requested", digest_str(&d));
            }
            out.push((d.hash.clone(), self.decode(r.compressor, r.data, &d)?));
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
        self.decode(compressor, wire, d)
    }

    /// Reads `limit` bytes of a blob from `offset` (ByteStream's
    /// `read_offset` and `read_limit`), uncompressed: a part of a large blob
    /// without fetching all of it.
    pub async fn read_range(&self, d: &re::Digest, offset: i64, limit: i64) -> Result<Vec<u8>> {
        let mut stream = ByteStreamClient::new(self.channel.clone())
            .read(self.req(ReadRequest {
                resource_name: format!("{}blobs/{}/{}", self.prefix(), d.hash, d.size_bytes),
                read_offset: offset,
                read_limit: limit,
            }))
            .await
            .with_context(|| format!("ByteStream read {} at {offset}", digest_str(d)))?
            .into_inner();
        let mut data = Vec::new();
        while let Some(chunk) = stream.message().await? {
            data.extend_from_slice(&chunk.data);
        }
        self.transfers.wire_read.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.transfers.read.fetch_add(data.len() as u64, Ordering::Relaxed);
        if limit > 0 && data.len() as i64 > limit {
            data.truncate(limit as usize);
        }
        Ok(data)
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
/// The error of [`Client::upload_blobs`] when blobs that only existed in the
/// CAS (`BlobSource::Remote`: outputs of earlier actions never downloaded)
/// are gone from it: the local paths they stand for.
#[derive(Debug)]
pub struct MissingRemoteBlobs(pub Vec<std::path::PathBuf>);

impl std::fmt::Display for MissingRemoteBlobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let paths: Vec<String> = self.0.iter().map(|p| p.display().to_string()).collect();
        write!(f, "no longer in the remote CAS: {}", paths.join(", "))
    }
}

impl std::error::Error for MissingRemoteBlobs {}

/// Uploads blobs: small ones batched, large ones through ByteStream.
struct Uploader<'a> {
    client: &'a Client,
    batch: Vec<re::batch_update_blobs_request::Request>,
    batch_bytes: i64,
    /// Bytes uploaded.
    sent: u64,
}

impl<'a> Uploader<'a> {
    fn new(client: &'a Client) -> Self {
        Self { client, batch: Vec::new(), batch_bytes: 0, sent: 0 }
    }

    async fn add(&mut self, d: &re::Digest, data: Vec<u8>) -> Result<()> {
        self.sent += data.len() as u64;
        let limit = self.client.max_batch_bytes - 1024;
        if d.size_bytes > limit {
            return self.client.bytestream_write(d, &data).await;
        }
        let (data, compressor) = match self.client.batch_zstd.then(|| compressed(&data)).flatten() {
            Some(c) => (c, re::compressor::Value::Zstd as i32),
            None => (data, re::compressor::Value::Identity as i32),
        };
        let wire = data.len() as i64;
        if self.batch_bytes + wire > limit {
            self.flush().await?;
        }
        self.batch_bytes += wire;
        let transfers = &self.client.transfers;
        transfers.sent.fetch_add(d.size_bytes as u64, Ordering::Relaxed);
        transfers.wire_sent.fetch_add(wire as u64, Ordering::Relaxed);
        self.batch.push(re::batch_update_blobs_request::Request { digest: Some(d.clone()), data, compressor });
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.batch_bytes = 0;
        self.client.flush_batch(std::mem::take(&mut self.batch)).await
    }
}

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

    #[test]
    fn compresses_what_shrinks() {
        assert_eq!(compressed(b"tiny"), None);
        let text = "int x = 1;
".repeat(1000);
        let c = compressed(text.as_bytes()).unwrap();
        assert!(c.len() < text.len() / 10);
        assert_eq!(zstd::bulk::decompress(&c, text.len()).unwrap(), text.as_bytes());
        // Random bytes don't shrink: sent as they are.
        let mut x = 0x9e3779b97f4a7c15u64;
        let noise: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        assert_eq!(compressed(&noise), None);
    }

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
