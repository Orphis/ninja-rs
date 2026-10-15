// Copied from cargo-rbe (crates/rbe-client, commit a22f361); see ../README.md.
//! Minimal REAPI v2 client: mTLS connection, CAS upload/download, Execute.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use bazel_remote_apis::google::bytestream::{
    QueryWriteStatusRequest, ReadRequest, WriteRequest, byte_stream_client::ByteStreamClient,
};
use bazel_remote_apis::google::longrunning::operation;
use prost::Message;
use re::{
    action_cache_client::ActionCacheClient, capabilities_client::CapabilitiesClient,
    content_addressable_storage_client::ContentAddressableStorageClient,
    execution_client::ExecutionClient,
};
use sha2::{Digest as _, Sha256};
use tokio::sync::watch;
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
/// What a blob adds to a batch besides its data (its digest, compressor
/// and status, and their framing): a batch of many small blobs must stay
/// within the limit too.
const BATCH_ENTRY_OVERHEAD: i64 = 128;
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
    /// Calls failing for a transient reason are made again this many times
    /// (`--remote_retries`), after exponential backoff.
    retries: u32,
    /// How long a call may take (`--remote_timeout`), and a stream go
    /// without moving. Executions aren't bounded: a broken one is followed
    /// again (`WaitExecution`).
    timeout: Duration,
    /// Whether the server answered a call since connecting. Until it has,
    /// a call it left unanswered isn't made again (see `retrying`). Shared
    /// by clones.
    answered: Arc<std::sync::atomic::AtomicBool>,
    /// Blobs being uploaded, by hash: a call needing one another call is
    /// uploading waits for it (`true` once there, `false` if that upload
    /// failed), instead of uploading it too. Shared by clones.
    inflight: Arc<Mutex<HashMap<String, watch::Receiver<Option<bool>>>>>,
    /// Permits for reads (`MAX_READS`), shared by clones.
    reads: Arc<tokio::sync::Semaphore>,
    /// Permits for uploads (`MAX_UPLOADS`), shared by clones.
    uploads: Arc<tokio::sync::Semaphore>,
}

/// `--remote_retries` and `--remote_timeout` defaults, Bazel's.
pub const DEFAULT_RETRIES: u32 = 5;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The gRPC status an error is, or wraps.
pub fn status_of(e: &anyhow::Error) -> Option<&tonic::Status> {
    e.chain().find_map(|c| c.downcast_ref::<tonic::Status>())
}

/// Codes worth another call, as Bazel's `RemoteRetrier` retries them.
pub fn transient(code: tonic::Code) -> bool {
    use tonic::Code::*;
    matches!(code, Unavailable | Unknown | DeadlineExceeded | Aborted | Internal | ResourceExhausted | Cancelled)
}

/// Calls a server that never answered anything gets again when the
/// connection broke (reset, refused): a server restarting, or a load
/// balancer's hiccup. One leaving calls unanswered gets none.
const FIRST_CONTACT_RETRIES: u32 = 2;

/// What a call left unanswered fails with (see `no_answer`): its status's
/// source, which tells it apart from the server's own DEADLINE_EXCEEDED.
#[derive(Debug)]
struct NoAnswer;

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no answer")
    }
}

impl std::error::Error for NoAnswer {}

/// DEADLINE_EXCEEDED for a call or a stream the server left unanswered.
fn no_answer(message: String) -> tonic::Status {
    let mut s = tonic::Status::deadline_exceeded(message);
    s.set_source(Arc::new(NoAnswer));
    s
}

/// Whether `s` is the server's own status: an answer. One standing for a
/// broken connection carries the transport's error as its source, one for
/// a call left unanswered `NoAnswer`.
fn from_server(s: &tonic::Status) -> bool {
    std::error::Error::source(s).is_none()
}

/// Whether `s` stands for a connection that broke (reset, refused, closed),
/// rather than an answer or a call left unanswered.
fn broken_connection(s: &tonic::Status) -> bool {
    let cause = std::error::Error::source(s);
    cause.is_some() && broke(cause)
}

/// Whether a transport error (with its causes) may go away when the call
/// is made again: not a timeout (a server leaving connections unanswered
/// would cost one each time), nor a certificate refused.
fn broke(mut cause: Option<&(dyn std::error::Error + 'static)>) -> bool {
    while let Some(e) = cause {
        let io_timeout = e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut);
        let text = e.to_string().to_ascii_lowercase();
        if io_timeout || e.is::<NoAnswer>() || text.contains("timed out") || text.contains("timeout") || text.contains("certificate") {
            return false;
        }
        cause = e.source();
    }
    true
}

/// How long to wait before attempt `attempt + 1`: 100 ms doubling up to
/// 5 s (Bazel's), give or take 10%.
fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(100).saturating_mul(1 << attempt.min(16)).min(Duration::from_secs(5));
    base.mul_f64(0.9 + 0.2 * (ninja_os::random_u64() % 1000) as f64 / 1000.0)
}

/// Reads until `buf` is full or the end: fewer bytes only at the end.
fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// A ByteStream write's requests, `BYTESTREAM_CHUNK` of the blob at a time,
/// compressed as they go (see `size`): a large file is never in memory
/// whole. `reader` is at `offset`, where the write resumes (uncompressed
/// only). Stops when the call has ended (`tx` closed). `wire`: bytes sent.
fn produce_writes(
    mut reader: Box<dyn Read + Send>,
    size: Option<u64>,
    resource: String,
    offset: u64,
    tx: tokio::sync::mpsc::Sender<WriteRequest>,
    wire: Arc<AtomicU64>,
) -> Result<()> {
    // Compressed when `size` is given: the frame says it (and the encoder
    // fails should the file have changed size since it was hashed).
    let mut encoder = match size {
        Some(size) => {
            let mut e = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
            e.set_pledged_src_size(Some(size))?;
            Some(e)
        }
        None => None,
    };
    let mut buf = vec![0u8; BYTESTREAM_CHUNK];
    let (start, mut offset) = (offset, offset);
    loop {
        let n = read_full(&mut reader, &mut buf)?;
        let end = n < buf.len();
        let mut data = match encoder.as_mut() {
            Some(e) => {
                e.write_all(&buf[..n])?;
                std::mem::take(e.get_mut())
            }
            None => buf[..n].to_vec(),
        };
        if end && let Some(e) = encoder.take() {
            data.extend(e.finish()?);
        }
        // The compressor holds data back: nothing to send yet.
        if data.is_empty() && !end {
            continue;
        }
        let request = WriteRequest {
            resource_name: if offset == start { resource.clone() } else { String::new() },
            write_offset: offset as i64,
            finish_write: end,
            data,
        };
        offset += request.data.len() as u64;
        wire.fetch_add(request.data.len() as u64, Ordering::Relaxed);
        if tx.blocking_send(request).is_err() || end {
            return Ok(());
        }
    }
}

/// Blobs smaller than this travel uncompressed: too little to gain.
const MIN_COMPRESSED: usize = 1024;

/// Chunks of one blob uploaded at once through ByteStream (as Bazel's
/// `ChunkedBlobUploader`).
const CHUNK_STREAMS: usize = 16;

/// Batches and streams one call uploads at once.
const UPLOADS: usize = 16;

/// Uploads (batches and streams) in flight at once, all calls together:
/// `--remote_jobs` actions uploading 16 each would queue hundreds of calls
/// on the connection, each stream with a thread reading its file.
const MAX_UPLOADS: usize = 32;

/// Reads (batches and streams) in flight at once, all calls together. This
/// bounds calls, not bytes: callers bound what they hold in memory.
const MAX_READS: usize = 32;

/// How often an HTTP/2 ping checks a connection that's quiet while calls
/// are in flight (an execution can say nothing for minutes): no more often
/// than gRPC servers allow by default (every 5 minutes; they close
/// connections pinged more often, `too_many_pings`).
const KEEP_ALIVE: Duration = Duration::from_secs(330);

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

/// How long connecting may take: resolving the name, the TCP connection,
/// the TLS handshake and HTTP/2's.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Connects a gRPC channel (TLS for `grpcs://`, with an optional client
/// identity). Shared by the REAPI client and other services (e.g. BES).
pub async fn connect_channel(opts: &ConnectOptions) -> Result<Channel> {
    connect_within(opts, CONNECT_TIMEOUT).await
}

/// The host of an endpoint without its scheme (`host:port/...`), as its
/// certificate names it: an IPv6 address without its brackets.
fn endpoint_host(rest: &str) -> &str {
    match rest.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => rest.split([':', '/']).next().unwrap_or(rest),
    }
}

/// `connect_channel`, failing after `timeout`. A connection refused or
/// reset is tried again, twice (a server restarting); one that times out
/// isn't.
async fn connect_within(opts: &ConnectOptions, timeout: Duration) -> Result<Channel> {
    let (scheme, rest) = opts.endpoint.split_once("://").unwrap_or(("grpcs", opts.endpoint.as_str()));
    let host = endpoint_host(rest).to_string();
    let (url, tls) = match scheme {
        "grpcs" | "https" => (format!("https://{rest}"), true),
        "grpc" | "http" => (format!("http://{rest}"), false),
        other => bail!("unsupported endpoint scheme `{other}`"),
    };
    // Flow-control windows sized to the connection's bandwidth-delay
    // product (as grpc-java tunes them), not 64 KiB: one large download
    // isn't held back by them. Pings only while calls are in flight, as
    // Bazel's (`--grpc_keepalive_time`): servers close connections pinged
    // while idle.
    let mut endpoint = Endpoint::from_shared(url)?
        .connect_timeout(timeout)
        .http2_keep_alive_interval(KEEP_ALIVE)
        .keep_alive_while_idle(false)
        .tcp_nodelay(true)
        .http2_adaptive_window(true);
    if tls {
        let domain = opts.authority_override.clone().unwrap_or(host);
        // The connect timeout covers the TCP connection only: a server
        // accepting connections and never answering would hold the
        // handshake (and every reconnection) forever.
        let mut cfg = ClientTlsConfig::new().domain_name(domain).timeout(timeout);
        // A CA given is the only one trusted, as with Bazel's
        // `--tls_certificate`: a server pinned to a private CA can't be
        // impersonated with a publicly issued certificate.
        cfg = match &opts.ca_pem {
            Some(ca) => cfg.ca_certificate(Certificate::from_pem(ca)),
            None => cfg.with_webpki_roots(),
        };
        match (&opts.client_cert_pem, &opts.client_key_pem) {
            (Some(cert), Some(key)) => cfg = cfg.identity(Identity::from_pem(cert, key)),
            (None, None) => {}
            _ => bail!("client certificate and key must be given together"),
        }
        endpoint = endpoint.tls_config(cfg)?;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut attempt = 0;
    loop {
        let e = match tokio::time::timeout_at(deadline, endpoint.connect()).await {
            Ok(Ok(channel)) => return Ok(channel),
            Ok(Err(e)) => e,
            Err(_) => bail!("connecting to {}: no connection within {timeout:?}", opts.endpoint),
        };
        if attempt >= FIRST_CONTACT_RETRIES || !broke(Some(&e)) {
            return Err(e).with_context(|| format!("connecting to {}", opts.endpoint));
        }
        tokio::time::sleep(backoff(attempt)).await;
        attempt += 1;
    }
}

/// An extra gRPC header (`--remote_header`, `--bes_header`) as metadata:
/// a valid name (lowercased) and an ASCII value.
pub fn header(name: &str, value: &str) -> Result<(tonic::metadata::AsciiMetadataKey, tonic::metadata::AsciiMetadataValue)> {
    let key = tonic::metadata::AsciiMetadataKey::from_bytes(name.to_ascii_lowercase().as_bytes()).with_context(|| format!("invalid header name `{name}`"))?;
    let value = value.parse().with_context(|| format!("invalid value for header `{name}`"))?;
    Ok((key, value))
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

/// Whether `hash` is a SHA-256 in lowercase hex, as every digest is. A hash
/// from a server, a cache or a record becomes a file name (in a disk cache,
/// a resource name): anything else (`..`, a path) must be refused first.
pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a digest from elsewhere is well formed (see [`valid_hash`]).
pub fn valid_digest(d: &re::Digest) -> bool {
    valid_hash(&d.hash) && d.size_bytes >= 0
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
        self.headers = Arc::new(headers.iter().map(|(k, v)| header(k, v)).collect::<Result<_>>()?);
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
            retries: DEFAULT_RETRIES,
            timeout: DEFAULT_TIMEOUT,
            answered: Default::default(),
            inflight: Default::default(),
            reads: Arc::new(tokio::sync::Semaphore::new(MAX_READS)),
            uploads: Arc::new(tokio::sync::Semaphore::new(MAX_UPLOADS)),
        }
    }

    /// Calls failing for a transient reason are made again `retries` times;
    /// each may take `timeout` (a stream, go that long without moving).
    pub fn with_retries(mut self, retries: u32, timeout: Duration) -> Self {
        self.retries = retries;
        self.timeout = timeout;
        self
    }

    /// Runs `call` until it succeeds, fails for good, or has failed for a
    /// transient reason `retries` times, waiting longer each time (see
    /// `retry_after`).
    async fn retrying<T, F, Fut>(&self, mut call: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut attempt = 0;
        loop {
            let e = match call().await {
                Ok(v) => {
                    self.answer();
                    return Ok(v);
                }
                Err(e) => e,
            };
            if status_of(&e).is_some_and(from_server) {
                self.answer();
            }
            let Some(wait) = self.retry_after(&e, attempt) else { return Err(e) };
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// How long to wait before attempt `attempt + 1` of a call that failed
    /// with `e`, if it's worth one: a transient failure, up to `retries`
    /// times. Until the server answered anything, only a connection that
    /// broke (reset, refused) is, twice at most (the call reconnects): a
    /// server leaving calls unanswered (one accepting connections and
    /// saying nothing, as a hung one does) would cost `--remote_timeout`
    /// each time.
    fn retry_after(&self, e: &anyhow::Error, attempt: u32) -> Option<Duration> {
        let status = status_of(e)?;
        let budget = if self.answered.load(Ordering::Relaxed) {
            self.retries
        } else if broken_connection(status) {
            self.retries.min(FIRST_CONTACT_RETRIES)
        } else {
            0
        };
        (attempt < budget && transient(status.code())).then(|| backoff(attempt))
    }

    /// The server answered something: a call, a stream's message or its
    /// own status.
    fn answer(&self) {
        self.answered.store(true, Ordering::Relaxed);
    }

    /// `call`, failing with DEADLINE_EXCEEDED after `--remote_timeout`.
    async fn timed<T>(&self, call: impl std::future::Future<Output = Result<T, tonic::Status>>) -> Result<T, tonic::Status> {
        match tokio::time::timeout(self.timeout, call).await {
            Ok(r) => {
                if r.as_ref().err().is_none_or(from_server) {
                    self.answer();
                }
                r
            }
            Err(_) => Err(no_answer(format!("no answer within {:?} (--remote_timeout)", self.timeout))),
        }
    }

    /// Resolves when `progress` (milliseconds after `start`) is older than
    /// `--remote_timeout`: a stream that stopped moving.
    async fn stalled(&self, progress: &AtomicU64, start: Instant) {
        loop {
            tokio::time::sleep(self.timeout / 4).await;
            if start.elapsed().saturating_sub(Duration::from_millis(progress.load(Ordering::Relaxed))) > self.timeout {
                return;
            }
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
        let this = &*self;
        let caps = this
            .retrying(|| async {
                let request = this.req(re::GetCapabilitiesRequest { instance_name: this.instance.clone() });
                Ok(this.timed(CapabilitiesClient::new(this.channel.clone()).get_capabilities(request)).await?.into_inner())
            })
            .await?;
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
                .retrying(|| async {
                    let request = self.req(re::FindMissingBlobsRequest {
                        instance_name: self.instance.clone(),
                        blob_digests: chunk.to_vec(),
                        digest_function: re::digest_function::Value::Sha256 as i32,
                    });
                    Ok(self.timed(self.cas().find_missing_blobs(request)).await?.into_inner())
                })
                .await?;
            missing.extend(resp.missing_blob_digests);
        }
        Ok(missing)
    }

    /// Uploads every blob the server doesn't already have. Small blobs are
    /// batched, large ones go through ByteStream, or, when the server
    /// splices blobs, as their chunks (only those it lacks), then spliced.
    /// Chunks are batched too, unless the server compresses ByteStream
    /// writes and not batches: then each goes through ByteStream,
    /// compressed, several at once, as Bazel uploads them. Large files are
    /// read as they are sent. A blob another call is uploading (concurrent
    /// actions sharing a toolchain) is waited for, not sent again.
    /// Returns the number of blobs uploaded and the bytes sent.
    pub async fn upload_blobs(&self, blobs: &BlobSet) -> Result<(usize, u64)> {
        let digests = blobs.0.values().map(|(d, _)| d.clone()).collect();
        // A digest the server lists that it wasn't asked about (its bug) is
        // left out: there's nothing to upload for it.
        let mut listed = HashSet::new();
        let missing: Vec<re::Digest> = self
            .find_missing(digests)
            .await?
            .into_iter()
            .filter_map(|d| blobs.0.get(&d.hash).filter(|_| listed.insert(d.hash.clone())).map(|(ours, _)| ours.clone()))
            .collect();
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
        // This call uploads the blobs no other call is uploading.
        let (mut up, waits) = {
            let mut inflight = self.inflight.lock().unwrap();
            let (mut claims, mut waits) = (HashMap::new(), Vec::new());
            for d in &missing {
                match inflight.get(&d.hash) {
                    Some(rx) => waits.push((d.clone(), rx.clone())),
                    None => {
                        let (tx, rx) = watch::channel(None);
                        inflight.insert(d.hash.clone(), rx);
                        claims.insert(d.hash.clone(), tx);
                    }
                }
            }
            (Uploader::new(self, claims), waits)
        };
        let mut spliced = Vec::new();
        for d in &missing {
            if !up.claims.contains_key(&d.hash) {
                continue;
            }
            let (_, src) = &blobs.0[&d.hash];
            if let Some(cdc) = self.chunking.splice_chunker(d) {
                let chunks = self.upload_chunks(&mut up, cdc, d, src).await?;
                spliced.push((d.clone(), chunks));
                continue;
            }
            up.add(d, src).await?;
        }
        up.flush().await?;
        // Once all the chunks are in the CAS. A server refusing the splice
        // gets the whole blob.
        for (d, chunks) in spliced {
            if self.splice_blob(&d, &chunks).await.is_err() {
                up.add(&d, &blobs.0[&d.hash].1).await?;
            } else {
                up.done(&d.hash);
            }
        }
        up.flush().await?;
        let mut sent = up.sent;
        drop(up);
        // What other calls upload: a blob whose upload failed (or stopped)
        // is uploaded by this call.
        let mut again = Uploader::new(self, HashMap::new());
        for (d, mut rx) in waits {
            let uploaded = matches!(rx.wait_for(|v| v.is_some()).await.map(|v| *v), Ok(Some(true)));
            if !uploaded {
                again.add(&d, &blobs.0[&d.hash].1).await?;
            }
        }
        again.flush().await?;
        sent += again.sent;
        Ok((missing.len(), sent))
    }

    /// Uploads the chunks of a blob to splice that the server lacks, and
    /// returns them all. The blob is never whole in memory: its chunks are
    /// found reading it once (off the runtime's workers), and those to send
    /// read again as they're sent. Batched, unless the server compresses
    /// ByteStream writes and not batches: then each goes through
    /// ByteStream, compressed, several at once, as Bazel uploads them.
    async fn upload_chunks(&self, up: &mut Uploader<'_>, cdc: FastCdc, d: &re::Digest, src: &BlobSource) -> Result<Vec<re::Digest>> {
        let chunks = {
            let src = src.clone();
            tokio::task::spawn_blocking(move || chunk_offsets(cdc, &src)).await??
        };
        if chunks.iter().map(|(c, _)| c.size_bytes).sum::<i64>() != d.size_bytes {
            bail!("{}: changed since it was hashed", digest_str(d));
        }
        let mut lacking: HashSet<String> = self.find_missing(chunks.iter().map(|(c, _)| c.clone()).collect()).await?.into_iter().map(|c| c.hash).collect();
        let streamed = self.zstd && !self.batch_zstd;
        let mut streams = tokio::task::JoinSet::new();
        for (c, offset) in &chunks {
            if !lacking.remove(&c.hash) {
                continue;
            }
            let data = match src {
                BlobSource::File(p) => {
                    let (p, c, offset) = (p.clone(), c.clone(), *offset);
                    tokio::task::spawn_blocking(move || read_chunk(&p, &c, offset)).await??
                }
                BlobSource::Bytes(b) => b[*offset as usize..][..c.size_bytes as usize].to_vec(),
                BlobSource::Remote(_) => src.load()?,
            };
            if !streamed {
                up.add_bytes(c, data).await?;
                continue;
            }
            if streams.len() >= CHUNK_STREAMS {
                streams.join_next().await.expect("uploads in flight")??;
            }
            up.sent += data.len() as u64;
            let (client, c) = (self.clone(), c.clone());
            streams.spawn(async move { client.bytestream_write(&c, &BlobSource::Bytes(data)).await });
        }
        while let Some(r) = streams.join_next().await {
            r??;
        }
        Ok(chunks.into_iter().map(|(c, _)| c).collect())
    }

    /// The chunks a blob is made of, from the server (`GetChunkMapping`, or
    /// `SplitBlob` before RE API v2.13), and the function that split it.
    /// `None` when it doesn't split blobs or can't split this one.
    pub async fn split_blob(&self, d: &re::Digest) -> Result<Option<re::SplitBlobResponse>> {
        if !self.chunking.split {
            return Ok(None);
        }
        self.retrying(|| async {
            match tokio::time::timeout(self.timeout, self.split_blob_once(d)).await {
                Ok(r) => r,
                Err(_) => Err(no_answer(format!("splitting {}: no answer within {:?}", digest_str(d), self.timeout)).into()),
            }
        })
        .await
    }

    async fn split_blob_once(&self, d: &re::Digest) -> Result<Option<re::SplitBlobResponse>> {
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
        if let Some(c) = split.chunk_digests.iter().find(|c| !valid_digest(c)) {
            bail!("the chunks of {} include a malformed digest {:?}", digest_str(d), c.hash);
        }
        Ok(Some(split))
    }

    /// Tells the server that a blob is the concatenation of chunks it holds
    /// (`RegisterChunkMapping`, or `SpliceBlob` before RE API v2.13).
    pub async fn splice_blob(&self, d: &re::Digest, chunks: &[re::Digest]) -> Result<()> {
        self.retrying(|| async {
            match tokio::time::timeout(self.timeout, self.splice_blob_once(d, chunks)).await {
                Ok(r) => r,
                Err(_) => Err(no_answer(format!("splicing {}: no answer within {:?}", digest_str(d), self.timeout)).into()),
            }
        })
        .await
    }

    async fn splice_blob_once(&self, d: &re::Digest, chunks: &[re::Digest]) -> Result<()> {
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
        let _upload = self.uploads.acquire().await.context("uploads stopped")?;
        self.retrying(|| async {
            let request = self.req(re::BatchUpdateBlobsRequest {
                instance_name: self.instance.clone(),
                requests: requests.clone(),
                digest_function: re::digest_function::Value::Sha256 as i32,
            });
            let resp = self.timed(self.cas().batch_update_blobs(request)).await?.into_inner();
            for r in resp.responses {
                if let Some(st) = r.status
                    && st.code != 0
                {
                    let what = r.digest.as_ref().map(digest_str).unwrap_or_default();
                    return Err(tonic::Status::new(tonic::Code::from(st.code), format!("upload of {what} failed: {}", st.message)).into());
                }
            }
            Ok(())
        })
        .await
    }

    /// Uploads a blob through ByteStream, read from `src` as it is sent. A
    /// write that broke resumes where the server says it got to
    /// (`QueryWriteStatus`, as Bazel's), when uncompressed (a compressed
    /// stream can't be resumed: it starts over); getting further starts
    /// the count of retries over.
    async fn bytestream_write(&self, d: &re::Digest, src: &BlobSource) -> Result<()> {
        let _upload = self.uploads.acquire().await.context("uploads stopped")?;
        let compress = self.zstd && d.size_bytes as usize >= MIN_COMPRESSED;
        let kind = if compress { "compressed-blobs/zstd" } else { "blobs" };
        // One name for every attempt: what the server got is kept under it.
        let resource = format!("{}uploads/{}/{kind}/{}/{}", self.prefix(), upload_id(), d.hash, d.size_bytes);
        let (mut attempt, mut offset, mut furthest) = (0, 0, 0);
        loop {
            let e = match self.bytestream_write_once(d, src, &resource, compress, offset).await {
                Ok(()) => break,
                Err(e) => e,
            };
            if status_of(&e).is_some_and(from_server) {
                self.answer();
            }
            let Some(wait) = self.retry_after(&e, attempt) else { return Err(e) };
            let (complete, committed) = self.write_status(&resource).await;
            if complete {
                break;
            }
            offset = if compress { 0 } else { u64::try_from(committed).ok().filter(|&c| c <= d.size_bytes as u64).unwrap_or(0) };
            attempt = if offset > furthest { 0 } else { attempt + 1 };
            furthest = furthest.max(offset);
            tokio::time::sleep(wait).await;
        }
        self.answer();
        self.transfers.sent.fetch_add(d.size_bytes as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Whether the write `resource` is complete, and the bytes the server
    /// holds of it (none when it can't tell).
    async fn write_status(&self, resource: &str) -> (bool, i64) {
        let request = self.req(QueryWriteStatusRequest { resource_name: resource.to_string() });
        match self.timed(ByteStreamClient::new(self.channel.clone()).query_write_status(request)).await {
            Ok(r) => (r.get_ref().complete, r.get_ref().committed_size),
            Err(_) => (false, 0),
        }
    }

    /// One attempt of `bytestream_write`, from `offset`.
    async fn bytestream_write_once(&self, d: &re::Digest, src: &BlobSource, resource: &str, compress: bool, offset: u64) -> Result<()> {
        let reader: Box<dyn Read + Send> = match src {
            BlobSource::Bytes(b) => Box::new(std::io::Cursor::new(b.get(offset as usize..).unwrap_or_default().to_vec())),
            BlobSource::File(p) => {
                use std::io::Seek;
                let mut f = std::fs::File::open(p).with_context(|| format!("reading {}", p.display()))?;
                f.seek(std::io::SeekFrom::Start(offset))?;
                Box::new(f)
            }
            BlobSource::Remote(_) => return src.load().map(|_| ()),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let wire = Arc::new(AtomicU64::new(0));
        let producer = {
            let (wire, resource) = (wire.clone(), resource.to_string());
            let size = compress.then_some(d.size_bytes as u64);
            tokio::task::spawn_blocking(move || produce_writes(reader, size, resource, offset, tx, wire))
        };
        // The stream moves when the transport takes the next request. (When
        // it opens isn't known: the first request goes to the connection
        // before, and a server that took it and never answers looks the
        // same. Uploads are bounded, `MAX_UPLOADS`, not to wait for it.)
        let (progress, start) = (Arc::new(AtomicU64::new(0)), Instant::now());
        let requests = {
            let progress = progress.clone();
            tokio_stream::StreamExt::map(tokio_stream::wrappers::ReceiverStream::new(rx), move |r| {
                progress.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                r
            })
        };
        let mut bytestream = ByteStreamClient::new(self.channel.clone());
        let call = bytestream.write(self.req(requests));
        let result: Result<()> = tokio::select! {
            r = call => r.map(|_| ()).map_err(anyhow::Error::from),
            _ = self.stalled(&progress, start) => Err(no_answer(format!("stalled for {:?} (--remote_timeout)", self.timeout)).into()),
        };
        let produced = producer.await.context("ByteStream write");
        self.transfers.wire_sent.fetch_add(wire.load(Ordering::Relaxed), Ordering::Relaxed);
        // Reading the file failing explains a broken stream best.
        produced??;
        result.with_context(|| format!("ByteStream write {}", digest_str(d)))
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
            } else if !self.batched(d.size_bytes) {
                let (c, d) = (self.clone(), d.clone());
                tasks.spawn(async move { Ok(vec![(d.hash.clone(), c.bytestream_read(&d).await?)]) });
            } else {
                let entry = d.size_bytes + BATCH_ENTRY_OVERHEAD;
                if cur_bytes + entry > self.batch_room() {
                    batches.push(std::mem::take(&mut cur));
                    cur_bytes = 0;
                }
                cur_bytes += entry;
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

    /// Bytes a batch may hold: its blobs and their entries' overhead.
    fn batch_room(&self) -> i64 {
        self.max_batch_bytes - 1024
    }

    /// Whether a blob of `size` travels in batches (else through
    /// ByteStream).
    fn batched(&self, size: i64) -> bool {
        size + BATCH_ENTRY_OVERHEAD <= self.batch_room()
    }

    fn compressors(&self) -> Vec<i32> {
        if self.zstd { vec![re::compressor::Value::Zstd as i32] } else { vec![] }
    }

    /// The blob `d` as read: decompressed, and checked to be `d`. Memory
    /// follows what arrives, not the size `d` says: a server can name any.
    fn decode(&self, compressor: i32, data: Vec<u8>, d: &re::Digest) -> Result<Vec<u8>> {
        let size = d.size_bytes.max(0) as u64;
        self.transfers.wire_read.fetch_add(data.len() as u64, Ordering::Relaxed);
        let data = if compressor == re::compressor::Value::Zstd as i32 {
            let mut out = Vec::with_capacity(size.min(data.len() as u64 * 8).min(1 << 26) as usize);
            zstd::stream::read::Decoder::with_buffer(data.as_slice())
                .and_then(|z| z.take(size + 1).read_to_end(&mut out))
                .with_context(|| format!("decompressing {}", digest_str(d)))?;
            out
        } else {
            data
        };
        verify(d, &data)?;
        self.transfers.read.fetch_add(size, Ordering::Relaxed);
        Ok(data)
    }

    async fn batch_read(&self, digests: Vec<re::Digest>) -> Result<Vec<(String, Vec<u8>)>> {
        let _read = self.reads.acquire().await.context("reads stopped")?;
        let requested: HashSet<(String, i64)> = digests.iter().map(|d| (d.hash.clone(), d.size_bytes)).collect();
        let resp = self
            .retrying(|| async {
                let request = self.req(re::BatchReadBlobsRequest {
                    instance_name: self.instance.clone(),
                    digests: digests.clone(),
                    acceptable_compressors: self.compressors(),
                    digest_function: re::digest_function::Value::Sha256 as i32,
                });
                Ok(self.timed(self.cas().batch_read_blobs(request)).await?.into_inner())
            })
            .await?;
        // Decompressed and hashed off the runtime's workers.
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(resp.responses.len());
            for r in resp.responses {
                let d = r.digest.clone().unwrap_or_default();
                if let Some(st) = &r.status
                    && st.code != 0
                {
                    return Err(tonic::Status::new(tonic::Code::from(st.code), format!("read of {} failed: {}", digest_str(&d), st.message)).into());
                }
                // Only what was asked for.
                if !requested.contains(&(d.hash.clone(), d.size_bytes)) {
                    bail!("BatchReadBlobs returned {}, which wasn't requested", digest_str(&d));
                }
                out.push((d.hash.clone(), this.decode(r.compressor, r.data, &d)?));
            }
            Ok(out)
        })
        .await?
    }

    async fn bytestream_read(&self, d: &re::Digest) -> Result<Vec<u8>> {
        let _read = self.reads.acquire().await.context("reads stopped")?;
        let kind = if self.zstd { "compressed-blobs/zstd" } else { "blobs" };
        let resource = format!("{}{kind}/{}/{}", self.prefix(), d.hash, d.size_bytes);
        let size = d.size_bytes.max(0) as u64;
        let want = if self.zstd { compressed_bound(size) } else { size };
        let wire = self.read_stream(&resource, 0, want, false, self.zstd).await.with_context(|| format!("ByteStream read {}", digest_str(d)))?;
        let compressor = if self.zstd { re::compressor::Value::Zstd as i32 } else { 0 };
        let (this, d) = (self.clone(), d.clone());
        tokio::task::spawn_blocking(move || this.decode(compressor, wire, &d)).await?
    }

    /// Reads a blob into the file at `path` (created, or truncated) as it
    /// arrives: decompressed, hashed and written on a blocking thread, so
    /// that a large blob is never whole in memory. A stream that broke
    /// resumes where it got to, the writer going on, and getting further
    /// starts the count of retries over; compressed, it starts over. The
    /// file is the blob `d` once this returns; after an error, it's to be
    /// removed.
    pub async fn read_blob_to_file(&self, d: &re::Digest, path: &std::path::Path) -> Result<()> {
        let _read = self.reads.acquire().await.context("reads stopped")?;
        let kind = if self.zstd { "compressed-blobs/zstd" } else { "blobs" };
        let resource = format!("{}{kind}/{}/{}", self.prefix(), d.hash, d.size_bytes);
        let context = || format!("ByteStream read {}", digest_str(d));
        let mut sink: Option<BlobSink> = None;
        let (mut attempt, mut got) = (0, 0);
        loop {
            let s = match sink.as_mut() {
                Some(s) => s,
                None => {
                    got = 0;
                    sink.insert(BlobSink::create(path, d, self.zstd)?)
                }
            };
            let before = got;
            let e = match self.read_stream_to(&resource, got, &s.tx, &mut got).await {
                Ok(()) => break,
                Err(e) => e,
            };
            // The writer stopped (its error says why), or it all arrived.
            if s.tx.is_closed() || (!self.zstd && got == d.size_bytes.max(0) as u64) {
                break;
            }
            let Some(wait) = self.retry_after(&e, attempt) else {
                if let Some(s) = sink {
                    let _ = s.finish().await;
                }
                return Err(e).with_context(context);
            };
            if self.zstd
                && let Some(s) = sink.take()
            {
                let _ = s.finish().await;
            }
            attempt = if !self.zstd && got > before { 0 } else { attempt + 1 };
            tokio::time::sleep(wait).await;
        }
        sink.expect("a blob being written").finish().await.with_context(context)?;
        self.transfers.read.fetch_add(d.size_bytes.max(0) as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Sends `resource` from `offset` to `tx` as it arrives, each message
    /// within `--remote_timeout`, counting its bytes in `got`. Stops early
    /// when the writer has.
    async fn read_stream_to(&self, resource: &str, offset: u64, tx: &tokio::sync::mpsc::Sender<Vec<u8>>, got: &mut u64) -> Result<()> {
        let request = self.req(ReadRequest { resource_name: resource.to_string(), read_offset: offset as i64, read_limit: 0 });
        let mut stream = self.timed(ByteStreamClient::new(self.channel.clone()).read(request)).await?.into_inner();
        while let Some(chunk) = self.timed(stream.message()).await? {
            *got += chunk.data.len() as u64;
            self.transfers.wire_read.fetch_add(chunk.data.len() as u64, Ordering::Relaxed);
            if tx.send(chunk.data).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    /// Reads `want` bytes of `resource` from `offset` into memory, each
    /// message within `--remote_timeout`: asking for that many (`limited`),
    /// or to its end, and failing past them (a digest's size bounds what
    /// is kept, not the server). A stream that broke resumes where it got
    /// to, and getting further starts the count of retries over;
    /// `compressed` data (whose size `want` bounds) starts over instead.
    async fn read_stream(&self, resource: &str, offset: i64, want: u64, limited: bool, compressed: bool) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut attempt = 0;
        loop {
            if compressed {
                data.clear();
            }
            let got = data.len() as u64;
            let limit = if limited { (want - got) as i64 } else { 0 };
            let e = match self.read_stream_once(resource, offset + got as i64, limit, want, &mut data).await {
                Ok(()) => return Ok(data),
                Err(e) => e,
            };
            // It broke after the last byte.
            if !compressed && data.len() as u64 == want && status_of(&e).is_some() {
                return Ok(data);
            }
            let Some(wait) = self.retry_after(&e, attempt) else { return Err(e) };
            attempt = if !compressed && data.len() as u64 > got { 0 } else { attempt + 1 };
            tokio::time::sleep(wait).await;
        }
    }

    /// One attempt of `read_stream`, appending to `data`, up to `want`
    /// bytes in all.
    async fn read_stream_once(&self, resource: &str, offset: i64, limit: i64, want: u64, data: &mut Vec<u8>) -> Result<()> {
        let request = self.req(ReadRequest { resource_name: resource.to_string(), read_offset: offset, read_limit: limit });
        let mut stream = self.timed(ByteStreamClient::new(self.channel.clone()).read(request)).await?.into_inner();
        while let Some(chunk) = self.timed(stream.message()).await? {
            if (data.len() + chunk.data.len()) as u64 > want {
                bail!("more than the {want} bytes expected");
            }
            data.extend_from_slice(&chunk.data);
        }
        Ok(())
    }

    /// Reads `limit` bytes of a blob from `offset` (ByteStream's
    /// `read_offset` and `read_limit`), uncompressed: a part of a large blob
    /// without fetching all of it.
    pub async fn read_range(&self, d: &re::Digest, offset: i64, limit: i64) -> Result<Vec<u8>> {
        let resource = format!("{}blobs/{}/{}", self.prefix(), d.hash, d.size_bytes);
        let rest = (d.size_bytes - offset).max(0) as u64;
        let want = if limit > 0 { rest.min(limit as u64) } else { rest };
        let data = self
            .read_stream(&resource, offset, want, limit > 0, false)
            .await
            .with_context(|| format!("ByteStream read {} at {offset}", digest_str(d)))?;
        self.transfers.wire_read.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.transfers.read.fetch_add(data.len() as u64, Ordering::Relaxed);
        Ok(data)
    }

    pub async fn get_action_result(&self, action: &re::Digest) -> Result<Option<re::ActionResult>> {
        self.retrying(|| async {
            let request = self.req(re::GetActionResultRequest {
                instance_name: self.instance.clone(),
                action_digest: Some(action.clone()),
                inline_stdout: true,
                inline_stderr: true,
                inline_output_files: vec![],
                digest_function: re::digest_function::Value::Sha256 as i32,
            });
            match self.timed(ActionCacheClient::new(self.channel.clone()).get_action_result(request)).await {
                Ok(r) => Ok(Some(r.into_inner())),
                Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
                Err(s) => Err(s.into()),
            }
        })
        .await
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
        self.retrying(|| async {
            let request = self.req(re::UpdateActionResultRequest {
                instance_name: self.instance.clone(),
                action_digest: Some(action.clone()),
                action_result: Some(result.clone()),
                results_cache_policy: None,
                digest_function: re::digest_function::Value::Sha256 as i32,
            });
            self.timed(ActionCacheClient::new(self.channel.clone()).update_action_result(request)).await?;
            Ok(())
        })
        .await
    }

    /// Runs `Execute` and waits for the operation to finish, as Bazel does:
    /// a stream broken once the operation is named is followed again
    /// (`WaitExecution`); an operation the server lost, or that failed for a
    /// transient reason, executes again. Two budgets of `--remote_retries`,
    /// as Bazel's `executeBackoff` and `waitExecutionBackoff`: executions
    /// (every new `Execute` counts, so an operation the server keeps
    /// losing fails), and follow-ups since the operation's last update (a
    /// long execution whose streams a proxy keeps resetting is followed to
    /// its end). FAILED_PRECONDITION comes back as the response's status,
    /// however the server reports it.
    pub async fn execute(&self, action: &re::Digest, skip_cache_lookup: bool) -> Result<re::ExecuteResponse> {
        let request = re::ExecuteRequest {
            instance_name: self.instance.clone(),
            skip_cache_lookup,
            action_digest: Some(action.clone()),
            execution_policy: None,
            results_cache_policy: None,
            digest_function: re::digest_function::Value::Sha256 as i32,
            inline_stdout: true,
            inline_stderr: true,
            inline_output_files: vec![],
        };
        let mut operation: Option<String> = None;
        let (mut executions, mut follow_ups) = (0, 0);
        loop {
            let executions_client = ExecutionClient::new(self.channel.clone());
            let mut moved = false;
            let outcome = match &operation {
                None => self.follow(executions_client.clone().execute(self.req(request.clone())).await, &mut operation, &mut moved).await,
                Some(name) => {
                    let call = executions_client.clone().wait_execution(self.req(re::WaitExecutionRequest { name: name.clone() })).await;
                    self.follow(call, &mut operation, &mut moved).await
                }
            };
            let status = match outcome {
                // Not DEADLINE_EXCEEDED: the action timed out.
                Ok(resp)
                    if resp.status.as_ref().is_some_and(|s| s.code != tonic::Code::DeadlineExceeded as i32 && transient(tonic::Code::from(s.code))) =>
                {
                    let st = resp.status.unwrap_or_default();
                    operation = None;
                    tonic::Status::new(tonic::Code::from(st.code), format!("execution failed: {}", st.message))
                }
                Ok(resp) => return Ok(resp),
                Err(Ended::Operation(s)) => {
                    operation = None;
                    s
                }
                Err(Ended::Stream(s)) => s,
            };
            if from_server(&status) {
                self.answer();
            }
            // Followed again, the operation is gone (or can't be followed):
            // executed again.
            let lost = operation.is_some() && matches!(status.code(), tonic::Code::NotFound | tonic::Code::Unimplemented);
            if lost {
                operation = None;
            }
            // An update since it was last followed: it runs.
            if moved {
                follow_ups = 0;
            }
            let budget = if operation.is_some() { &mut follow_ups } else { &mut executions };
            if *budget >= self.retries || !(lost || transient(status.code())) {
                return Err(anyhow::Error::from(status).context(format!("executing {}", digest_str(action))));
            }
            tokio::time::sleep(backoff(*budget)).await;
            *budget += 1;
        }
    }

    /// Follows an operation's updates until it is done: its response, or
    /// what ended it. Its name, once known, goes in `name`; `moved` tells
    /// whether an update came.
    async fn follow(
        &self,
        call: Result<tonic::Response<tonic::Streaming<bazel_remote_apis::google::longrunning::Operation>>, tonic::Status>,
        name: &mut Option<String>,
        moved: &mut bool,
    ) -> std::result::Result<re::ExecuteResponse, Ended> {
        let mut stream = call.map_err(Ended::Stream)?.into_inner();
        while let Some(op) = stream.message().await.map_err(Ended::Stream)? {
            self.answer();
            *moved = true;
            if !op.name.is_empty() {
                *name = Some(op.name.clone());
            }
            if !op.done {
                continue;
            }
            return match op.result {
                Some(operation::Result::Response(any)) => re::ExecuteResponse::decode(any.value.as_slice())
                    .map_err(|e| Ended::Operation(tonic::Status::internal(format!("decoding the ExecuteResponse: {e}")))),
                // Missing inputs, as the response would say it.
                Some(operation::Result::Error(st)) if st.code == tonic::Code::FailedPrecondition as i32 => {
                    Ok(re::ExecuteResponse { status: Some(st), ..Default::default() })
                }
                Some(operation::Result::Error(st)) => {
                    Err(Ended::Operation(tonic::Status::new(tonic::Code::from(st.code), format!("execution failed: {}", st.message))))
                }
                None => Err(Ended::Operation(tonic::Status::internal("operation done without result"))),
            };
        }
        Err(Ended::Stream(tonic::Status::unavailable("the stream ended before the operation completed")))
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

/// The chunks of a blob to splice, with their offsets: its source read
/// once, a chunk at a time.
fn chunk_offsets(cdc: FastCdc, src: &BlobSource) -> Result<Vec<(re::Digest, u64)>> {
    let reader: Box<dyn Read> = match src {
        BlobSource::Bytes(b) => Box::new(b.as_slice()),
        BlobSource::File(p) => Box::new(std::fs::File::open(p).with_context(|| format!("reading {}", p.display()))?),
        BlobSource::Remote(_) => return src.load().map(|_| Vec::new()),
    };
    cdc.stream(reader).map(|c| c.map(|c| (digest_of(&c.data), c.offset)).context("chunking a blob")).collect()
}

/// The chunk `c` of the file `path` to splice, at `offset`: the file must
/// still hold it.
fn read_chunk(path: &std::path::Path, c: &re::Digest, offset: u64) -> Result<Vec<u8>> {
    use std::io::Seek;
    let context = || format!("reading {}", path.display());
    let mut f = std::fs::File::open(path).with_context(context)?;
    f.seek(std::io::SeekFrom::Start(offset)).with_context(context)?;
    let mut data = vec![0; c.size_bytes as usize];
    f.read_exact(&mut data).with_context(context)?;
    verify(c, &data).with_context(|| format!("{} changed since it was hashed", path.display()))?;
    Ok(data)
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

/// How an operation followed ended without a response.
enum Ended {
    /// Its stream broke: it may still run.
    Stream(tonic::Status),
    /// It is done, failed.
    Operation(tonic::Status),
}

/// Uploads blobs: small ones batched, large ones through ByteStream, up to
/// `UPLOADS` batches and streams at once.
struct Uploader<'a> {
    client: &'a Client,
    batch: Vec<re::batch_update_blobs_request::Request>,
    batch_bytes: i64,
    /// Bytes uploaded.
    sent: u64,
    /// The blobs it uploads for every call needing them (see
    /// `Client::inflight`), until they are in the CAS.
    claims: HashMap<String, watch::Sender<Option<bool>>>,
    /// Batches and streams being sent: the hashes of the blobs each sends.
    running: tokio::task::JoinSet<Result<Vec<String>>>,
}

impl<'a> Uploader<'a> {
    fn new(client: &'a Client, claims: HashMap<String, watch::Sender<Option<bool>>>) -> Self {
        Self { client, batch: Vec::new(), batch_bytes: 0, sent: 0, claims, running: tokio::task::JoinSet::new() }
    }

    /// A claimed blob is in the CAS.
    fn done(&mut self, hash: &str) {
        if let Some(tx) = self.claims.remove(hash) {
            self.client.inflight.lock().unwrap().remove(hash);
            let _ = tx.send(Some(true));
        }
    }

    /// Uploads a blob: a large one read as it is sent, a small one batched.
    async fn add(&mut self, d: &re::Digest, src: &BlobSource) -> Result<()> {
        if !self.client.batched(d.size_bytes) {
            return self.stream(d, src.clone()).await;
        }
        self.add_bytes(d, src.load()?).await
    }

    /// Sends a large blob through ByteStream, alongside the other uploads.
    async fn stream(&mut self, d: &re::Digest, src: BlobSource) -> Result<()> {
        self.sent += d.size_bytes as u64;
        let (client, d) = (self.client.clone(), d.clone());
        self.spawn(async move { client.bytestream_write(&d, &src).await.map(|()| vec![d.hash]) }).await
    }

    /// Starts an upload once fewer than `UPLOADS` are running.
    async fn spawn(&mut self, upload: impl Future<Output = Result<Vec<String>>> + Send + 'static) -> Result<()> {
        while self.running.len() >= UPLOADS {
            self.join_one().await?;
        }
        self.running.spawn(upload);
        Ok(())
    }

    /// Waits for an upload to finish, its blobs then in the CAS.
    async fn join_one(&mut self) -> Result<()> {
        if let Some(r) = self.running.join_next().await {
            for h in r?? {
                self.done(&h);
            }
        }
        Ok(())
    }

    async fn add_bytes(&mut self, d: &re::Digest, data: Vec<u8>) -> Result<()> {
        if !self.client.batched(d.size_bytes) {
            return self.stream(d, BlobSource::Bytes(data)).await;
        }
        self.sent += data.len() as u64;
        let (data, compressor) = match self.client.batch_zstd.then(|| compressed(&data)).flatten() {
            Some(c) => (c, re::compressor::Value::Zstd as i32),
            None => (data, re::compressor::Value::Identity as i32),
        };
        let wire = data.len() as i64;
        let entry = wire + BATCH_ENTRY_OVERHEAD;
        if self.batch_bytes + entry > self.client.batch_room() {
            self.send_batch().await?;
        }
        self.batch_bytes += entry;
        let transfers = &self.client.transfers;
        transfers.sent.fetch_add(d.size_bytes as u64, Ordering::Relaxed);
        transfers.wire_sent.fetch_add(wire as u64, Ordering::Relaxed);
        self.batch.push(re::batch_update_blobs_request::Request { digest: Some(d.clone()), data, compressor });
        Ok(())
    }

    /// Sends the batch being filled, alongside the other uploads.
    async fn send_batch(&mut self) -> Result<()> {
        self.batch_bytes = 0;
        let batch = std::mem::take(&mut self.batch);
        if batch.is_empty() {
            return Ok(());
        }
        let hashes: Vec<String> = batch.iter().filter_map(|r| r.digest.as_ref().map(|d| d.hash.clone())).collect();
        let client = self.client.clone();
        self.spawn(async move { client.flush_batch(batch).await.map(|()| hashes) }).await
    }

    /// Sends the batch being filled and waits for every upload.
    async fn flush(&mut self) -> Result<()> {
        self.send_batch().await?;
        while !self.running.is_empty() {
            self.join_one().await?;
        }
        Ok(())
    }
}

/// A blob being written to a file as it arrives (see `write_blob_file`).
struct BlobSink {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    writer: tokio::task::JoinHandle<Result<()>>,
}

impl BlobSink {
    /// Creates (or truncates) the file before the writer starts: a read
    /// stopped meanwhile, whose caller removes the file, leaves none behind.
    fn create(path: &std::path::Path, d: &re::Digest, zstd: bool) -> Result<Self> {
        let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let (path, d) = (path.to_path_buf(), d.clone());
        Ok(Self { tx, writer: tokio::task::spawn_blocking(move || write_blob_file(file, &path, &d, zstd, rx)) })
    }

    /// The blob, once everything was sent: written, or why not.
    async fn finish(self) -> Result<()> {
        drop(self.tx);
        self.writer.await.context("writing a blob")?
    }
}

/// The most a blob of `size` bytes takes compressed with zstd (its
/// `ZSTD_compressBound`, give or take): what a compressed read may bring.
fn compressed_bound(size: u64) -> u64 {
    size + size / 128 + (64 << 10)
}

/// Writes a blob arriving in chunks to `file` (at `path`): decompressed
/// when `zstd`, hashed as it's written, and checked to be `d` once
/// complete. More bytes than `d` says stop it (a decompression bomb).
fn write_blob_file(file: std::fs::File, path: &std::path::Path, d: &re::Digest, zstd: bool, mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Result<()> {
    use std::io::Write;
    struct Hashing<W> {
        inner: W,
        hasher: Sha256,
        len: u64,
        limit: u64,
    }
    impl<W: Write> Write for Hashing<W> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.len += buf.len() as u64;
            if self.len > self.limit {
                return Err(std::io::Error::other("more bytes than its digest says"));
            }
            self.hasher.update(buf);
            self.inner.write_all(buf)?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
    let mut sink = Hashing { inner: std::io::BufWriter::new(file), hasher: Sha256::new(), len: 0, limit: d.size_bytes.max(0) as u64 };
    let context = || format!("writing {} to {}", digest_str(d), path.display());
    if zstd {
        let mut decoder = zstd::stream::write::Decoder::new(&mut sink).with_context(context)?;
        while let Some(chunk) = rx.blocking_recv() {
            decoder.write_all(&chunk).with_context(context)?;
        }
        decoder.flush().with_context(context)?;
    } else {
        while let Some(chunk) = rx.blocking_recv() {
            sink.write_all(&chunk).with_context(context)?;
        }
    }
    sink.flush().with_context(context)?;
    if sink.len != d.size_bytes as u64 {
        bail!("blob {}: received {} bytes", digest_str(d), sink.len);
    }
    let hash = hex::encode(sink.hasher.finalize());
    if hash != d.hash {
        bail!("blob {}: received data hashing to {hash}", digest_str(d));
    }
    Ok(())
}

/// Blobs it claimed and didn't upload (a failure, a cancelled call): the
/// calls waiting for them upload them.
impl Drop for Uploader<'_> {
    fn drop(&mut self) {
        if self.claims.is_empty() {
            return;
        }
        let mut inflight = self.client.inflight.lock().unwrap();
        for (hash, tx) in self.claims.drain() {
            inflight.remove(&hash);
            let _ = tx.send(Some(false));
        }
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

    /// Feeds `chunks` to `write_blob_file` as a stream would.
    fn write_chunks(path: &std::path::Path, d: &re::Digest, zstd: bool, chunks: Vec<Vec<u8>>) -> Result<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let feeder = std::thread::spawn(move || {
            for c in chunks {
                if tx.blocking_send(c).is_err() {
                    break;
                }
            }
        });
        let r = write_blob_file(std::fs::File::create(path).unwrap(), path, d, zstd, rx);
        feeder.join().unwrap();
        r
    }

    #[test]
    fn blobs_written_as_they_arrive() {
        let dir = test_dirs::base().join(format!("rbe-blob-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blob");
        let data: Vec<u8> = (0..300_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        let d = digest_of(&data);
        let split = |b: &[u8]| b.chunks(65_536).map(<[u8]>::to_vec).collect::<Vec<_>>();
        write_chunks(&path, &d, false, split(&data)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
        let packed = zstd::bulk::compress(&data, 3).unwrap();
        write_chunks(&path, &d, true, split(&packed)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
        // Other contents, a truncated stream, more than the digest says.
        let mut other = data.clone();
        other[1000] ^= 1;
        assert!(write_chunks(&path, &d, false, split(&other)).is_err());
        assert!(write_chunks(&path, &d, false, split(&data[..data.len() - 1])).is_err());
        let small = re::Digest { hash: d.hash.clone(), size_bytes: 1000 };
        assert!(format!("{:#}", write_chunks(&path, &small, true, split(&packed)).unwrap_err()).contains("more bytes"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    fn options(endpoint: &str) -> ConnectOptions {
        ConnectOptions {
            endpoint: endpoint.into(),
            instance_name: String::new(),
            client_cert_pem: None,
            client_key_pem: None,
            ca_pem: None,
            authority_override: None,
            headers: Vec::new(),
        }
    }

    /// A server that accepts connections and never says anything (as a
    /// hung one does: the kernel still accepts them). Its port.
    fn silent_server() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().collect();
            drop(held);
        });
        port
    }

    #[tokio::test]
    async fn tls_handshakes_are_bounded() {
        let opts = options(&format!("grpcs://127.0.0.1:{}", silent_server()));
        let start = Instant::now();
        let connected = tokio::time::timeout(Duration::from_secs(10), connect_within(&opts, Duration::from_millis(500))).await;
        assert!(connected.expect("the TLS handshake wasn't bounded").is_err());
        assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    }

    #[tokio::test]
    async fn servers_that_never_answered_are_not_called_again() {
        let mut c = Client::connect(options(&format!("grpc://127.0.0.1:{}", silent_server())))
            .await
            .unwrap()
            .with_retries(5, Duration::from_millis(300));
        let start = Instant::now();
        let err = c.capabilities().await.unwrap_err();
        assert_eq!(status_of(&err).map(|s| s.code()), Some(tonic::Code::DeadlineExceeded), "{err:#}");
        // One --remote_timeout, not six and their backoffs (5 s).
        assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
    }

    type Operations =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<bazel_remote_apis::google::longrunning::Operation, tonic::Status>> + Send>>;

    /// An Execution service whose streams break `breaks` times before the
    /// operation completes: after naming it (`Execute`), then after an
    /// update of it if `updates` (`WaitExecution`).
    #[derive(Clone)]
    struct Breaking {
        breaks: Arc<AtomicU64>,
        updates: bool,
        calls: Arc<AtomicU64>,
    }

    impl Breaking {
        fn stream(&self, follow_up: bool) -> Operations {
            use bazel_remote_apis::google::longrunning::Operation;
            self.calls.fetch_add(1, Ordering::SeqCst);
            let update = Operation { name: "operations/long".into(), ..Default::default() };
            let broke = Err(tonic::Status::unavailable("a proxy reset the stream"));
            if self.breaks.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
                let sent = if follow_up && !self.updates { vec![broke] } else { vec![Ok(update), broke] };
                return Box::pin(tokio_stream::iter(sent));
            }
            let response = re::ExecuteResponse { result: Some(re::ActionResult::default()), ..Default::default() };
            let any = pb::Any { type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".into(), value: response.encode_to_vec() };
            let done = Operation { done: true, result: Some(operation::Result::Response(any)), ..update.clone() };
            Box::pin(tokio_stream::iter(vec![Ok(update), Ok(done)]))
        }
    }

    #[tonic::async_trait]
    impl re::execution_server::Execution for Breaking {
        type ExecuteStream = Operations;
        async fn execute(&self, _: tonic::Request<re::ExecuteRequest>) -> Result<tonic::Response<Operations>, tonic::Status> {
            Ok(tonic::Response::new(self.stream(false)))
        }
        type WaitExecutionStream = Operations;
        async fn wait_execution(&self, _: tonic::Request<re::WaitExecutionRequest>) -> Result<tonic::Response<Operations>, tonic::Status> {
            Ok(tonic::Response::new(self.stream(true)))
        }
    }

    /// Executes against `svc` with `--remote_retries=2`: the outcome, and
    /// the calls made.
    async fn execute_against(svc: Breaking) -> (Result<re::ExecuteResponse>, u64) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(re::execution_server::ExecutionServer::new(svc.clone()))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        let channel = connect_channel(&options(&format!("grpc://{addr}"))).await.unwrap();
        let c = Client::from_channel(channel, "").with_retries(2, Duration::from_secs(10));
        let r = c.execute(&digest_of(b"action"), false).await;
        (r, svc.calls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn executions_making_progress_are_followed_however_often_streams_break() {
        // A long execution whose streams a proxy keeps resetting: each
        // follow-up got an update, so the retries start over.
        let svc = Breaking { breaks: Arc::new(AtomicU64::new(6)), updates: true, calls: Arc::default() };
        let (r, calls) = execute_against(svc).await;
        assert!(r.unwrap().result.is_some());
        assert_eq!(calls, 7);
        // Without updates, the breaks count: Execute, then two follow-ups.
        let svc = Breaking { breaks: Arc::new(AtomicU64::new(6)), updates: false, calls: Arc::default() };
        let (r, calls) = execute_against(svc).await;
        assert_eq!(status_of(&r.unwrap_err()).map(|s| s.code()), Some(tonic::Code::Unavailable));
        assert_eq!(calls, 3);
    }

    /// Execute names the operation and sends an update before its stream
    /// breaks; following it, WaitExecution says it's gone (`lost`).
    #[derive(Clone)]
    struct Losing {
        lost: tonic::Code,
        executes: Arc<AtomicU64>,
        waits: Arc<AtomicU64>,
    }

    #[tonic::async_trait]
    impl re::execution_server::Execution for Losing {
        type ExecuteStream = Operations;
        async fn execute(&self, _: tonic::Request<re::ExecuteRequest>) -> Result<tonic::Response<Operations>, tonic::Status> {
            use bazel_remote_apis::google::longrunning::Operation;
            let n = self.executes.fetch_add(1, Ordering::SeqCst);
            let op = Operation { name: format!("operations/{n}"), ..Default::default() };
            let sent = vec![Ok(op.clone()), Ok(op), Err(tonic::Status::unavailable("a proxy reset the stream"))];
            Ok(tonic::Response::new(Box::pin(tokio_stream::iter(sent))))
        }
        type WaitExecutionStream = Operations;
        async fn wait_execution(&self, _: tonic::Request<re::WaitExecutionRequest>) -> Result<tonic::Response<Operations>, tonic::Status> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            Err(tonic::Status::new(self.lost, "no such operation"))
        }
    }

    #[tokio::test]
    async fn executions_the_server_keeps_losing_fail() {
        for lost in [tonic::Code::NotFound, tonic::Code::Unimplemented] {
            let svc = Losing { lost, executes: Arc::default(), waits: Arc::default() };
            let endpoint = serve(tonic::transport::Server::builder().add_service(re::execution_server::ExecutionServer::new(svc.clone()))).await;
            let c = Client::from_channel(connect_channel(&options(&endpoint)).await.unwrap(), "").with_retries(2, Duration::from_secs(10));
            // Each new execution counts, however far the last one got.
            let r = tokio::time::timeout(Duration::from_secs(30), c.execute(&digest_of(b"action"), false)).await.expect("executed forever");
            assert_eq!(status_of(&r.unwrap_err()).map(|s| s.code()), Some(lost));
            assert_eq!((svc.executes.load(Ordering::SeqCst), svc.waits.load(Ordering::SeqCst)), (3, 3));
        }
    }

    /// Serves `router` on a port of its own: its endpoint.
    async fn serve(router: tonic::transport::server::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(router.serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)));
        format!("grpc://{addr}")
    }

    /// A CAS (with ByteStream and the capabilities) misbehaving as told.
    #[derive(Clone, Default)]
    struct TestCas(Arc<CasState>);

    #[derive(Default)]
    struct CasState {
        blobs: Mutex<HashMap<String, Vec<u8>>>,
        /// FindMissingBlobs lists it too, unasked.
        unasked: Mutex<Option<re::Digest>>,
        /// The largest BatchUpdateBlobs request, encoded.
        largest_batch: AtomicU64,
        /// The next reads break after sending this many bytes, so many
        /// times; their offsets.
        read_breaks: Mutex<(u32, usize)>,
        read_offsets: Mutex<Vec<i64>>,
        /// Reads send this many bytes, whatever the blob (counted in
        /// `streamed`).
        endless: Mutex<Option<usize>>,
        streamed: Arc<AtomicU64>,
        /// Reads hold their first message this long.
        read_hold: Mutex<Duration>,
        /// The next writes break after a message, so many times, keeping
        /// what they got under their name; their offsets.
        write_breaks: Mutex<u32>,
        partial: Mutex<HashMap<String, Vec<u8>>>,
        write_offsets: Mutex<Vec<i64>>,
        /// Writes wait this long between messages.
        write_pace: Mutex<Duration>,
        writing: AtomicU64,
        most_writing: AtomicU64,
        /// zstd advertised (and served for reads).
        zstd: std::sync::atomic::AtomicBool,
        /// GetActionResult leaves this many calls unanswered first.
        unanswered: Mutex<u32>,
    }

    type Responses<T> = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<T, tonic::Status>> + Send>>;

    impl TestCas {
        fn put(&self, data: Vec<u8>) -> re::Digest {
            let d = digest_of(&data);
            self.0.blobs.lock().unwrap().insert(d.hash.clone(), data);
            d
        }

        fn has(&self, d: &re::Digest) -> bool {
            self.0.blobs.lock().unwrap().get(&d.hash).is_some_and(|b| digest_of(b) == *d)
        }

        async fn serve(&self) -> String {
            let s = self.clone();
            serve(
                tonic::transport::Server::builder()
                    .add_service(re::content_addressable_storage_server::ContentAddressableStorageServer::new(s.clone()))
                    .add_service(re::capabilities_server::CapabilitiesServer::new(s.clone()))
                    .add_service(re::action_cache_server::ActionCacheServer::new(s.clone()))
                    .add_service(bazel_remote_apis::google::bytestream::byte_stream_server::ByteStreamServer::new(s)),
            )
            .await
        }

        /// A client of it, knowing its capabilities.
        async fn client(&self, retries: u32, timeout: Duration) -> Client {
            let endpoint = self.serve().await;
            let mut c = Client::connect(options(&endpoint)).await.unwrap().with_retries(retries, timeout);
            c.capabilities().await.unwrap();
            c
        }
    }

    #[tonic::async_trait]
    impl re::capabilities_server::Capabilities for TestCas {
        async fn get_capabilities(&self, _: tonic::Request<re::GetCapabilitiesRequest>) -> Result<tonic::Response<re::ServerCapabilities>, tonic::Status> {
            let zstd = if self.0.zstd.load(Ordering::SeqCst) { vec![re::compressor::Value::Zstd as i32] } else { vec![] };
            Ok(tonic::Response::new(re::ServerCapabilities {
                cache_capabilities: Some(re::CacheCapabilities {
                    max_batch_total_size_bytes: 4 << 20,
                    supported_compressors: zstd,
                    ..Default::default()
                }),
                ..Default::default()
            }))
        }
    }

    #[tonic::async_trait]
    impl re::action_cache_server::ActionCache for TestCas {
        async fn get_action_result(&self, _: tonic::Request<re::GetActionResultRequest>) -> Result<tonic::Response<re::ActionResult>, tonic::Status> {
            let unanswered = {
                let mut n = self.0.unanswered.lock().unwrap();
                let left = *n > 0;
                *n = n.saturating_sub(1);
                left
            };
            if unanswered {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            Err(tonic::Status::not_found("not cached"))
        }
        async fn update_action_result(&self, _: tonic::Request<re::UpdateActionResultRequest>) -> Result<tonic::Response<re::ActionResult>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
    }

    #[tonic::async_trait]
    impl re::content_addressable_storage_server::ContentAddressableStorage for TestCas {
        async fn find_missing_blobs(&self, r: tonic::Request<re::FindMissingBlobsRequest>) -> Result<tonic::Response<re::FindMissingBlobsResponse>, tonic::Status> {
            let mut missing: Vec<re::Digest> = r.into_inner().blob_digests.into_iter().filter(|d| !self.has(d)).collect();
            missing.extend(self.0.unasked.lock().unwrap().clone());
            Ok(tonic::Response::new(re::FindMissingBlobsResponse { missing_blob_digests: missing }))
        }
        async fn batch_update_blobs(&self, r: tonic::Request<re::BatchUpdateBlobsRequest>) -> Result<tonic::Response<re::BatchUpdateBlobsResponse>, tonic::Status> {
            self.0.largest_batch.fetch_max(r.get_ref().encoded_len() as u64, Ordering::SeqCst);
            let responses = r
                .into_inner()
                .requests
                .into_iter()
                .map(|q| {
                    let d = self.put(q.data);
                    re::batch_update_blobs_response::Response { digest: Some(d), status: None }
                })
                .collect();
            Ok(tonic::Response::new(re::BatchUpdateBlobsResponse { responses }))
        }
        async fn batch_read_blobs(&self, r: tonic::Request<re::BatchReadBlobsRequest>) -> Result<tonic::Response<re::BatchReadBlobsResponse>, tonic::Status> {
            let blobs = self.0.blobs.lock().unwrap();
            let responses = r
                .into_inner()
                .digests
                .into_iter()
                .map(|d| re::batch_read_blobs_response::Response { data: blobs.get(&d.hash).cloned().unwrap_or_default(), digest: Some(d), ..Default::default() })
                .collect();
            Ok(tonic::Response::new(re::BatchReadBlobsResponse { responses }))
        }
        type GetTreeStream = Responses<re::GetTreeResponse>;
        async fn get_tree(&self, _: tonic::Request<re::GetTreeRequest>) -> Result<tonic::Response<Self::GetTreeStream>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
        async fn split_blob(&self, _: tonic::Request<re::SplitBlobRequest>) -> Result<tonic::Response<re::SplitBlobResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
        async fn splice_blob(&self, _: tonic::Request<re::SpliceBlobRequest>) -> Result<tonic::Response<re::SpliceBlobResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
        type GetChunkMappingStream = Responses<re::GetChunkMappingResponse>;
        async fn get_chunk_mapping(&self, _: tonic::Request<re::GetChunkMappingRequest>) -> Result<tonic::Response<Self::GetChunkMappingStream>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
        async fn register_chunk_mapping(
            &self,
            _: tonic::Request<tonic::Streaming<re::RegisterChunkMappingRequest>>,
        ) -> Result<tonic::Response<re::RegisterChunkMappingResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented(""))
        }
    }

    use bazel_remote_apis::google::bytestream as bs;

    #[tonic::async_trait]
    impl bs::byte_stream_server::ByteStream for TestCas {
        type ReadStream = Responses<bs::ReadResponse>;
        async fn read(&self, r: tonic::Request<bs::ReadRequest>) -> Result<tonic::Response<Self::ReadStream>, tonic::Status> {
            let r = r.into_inner();
            self.0.read_offsets.lock().unwrap().push(r.read_offset);
            if let Some(n) = *self.0.endless.lock().unwrap() {
                let streamed = self.0.streamed.clone();
                let chunks = (0..n / 65_536).map(move |_| {
                    streamed.fetch_add(65_536, Ordering::SeqCst);
                    Ok(bs::ReadResponse { data: vec![7; 65_536] })
                });
                return Ok(tonic::Response::new(Box::pin(tokio_stream::iter(chunks))));
            }
            let parts: Vec<&str> = r.resource_name.split('/').collect();
            let hash = parts[parts.len() - 2];
            let blob = self.0.blobs.lock().unwrap().get(hash).cloned().ok_or_else(|| tonic::Status::not_found("no such blob"))?;
            let data = if parts.contains(&"compressed-blobs") { zstd::bulk::compress(&blob, 1).unwrap() } else { blob[r.read_offset as usize..].to_vec() };
            let mut sent: Vec<Result<bs::ReadResponse, tonic::Status>> = data.chunks(65_536).map(|c| Ok(bs::ReadResponse { data: c.to_vec() })).collect();
            {
                let mut breaks = self.0.read_breaks.lock().unwrap();
                if breaks.0 > 0 {
                    breaks.0 -= 1;
                    sent.truncate(breaks.1 / 65_536);
                    sent.push(Err(tonic::Status::unavailable("the stream broke")));
                }
            }
            let hold = *self.0.read_hold.lock().unwrap();
            let held = tokio_stream::StreamExt::then(tokio_stream::iter(sent.into_iter().enumerate()), move |(i, m)| async move {
                if i == 0 {
                    tokio::time::sleep(hold).await;
                }
                m
            });
            Ok(tonic::Response::new(Box::pin(held)))
        }

        async fn write(&self, r: tonic::Request<tonic::Streaming<bs::WriteRequest>>) -> Result<tonic::Response<bs::WriteResponse>, tonic::Status> {
            struct Writing<'a>(&'a CasState);
            impl Drop for Writing<'_> {
                fn drop(&mut self) {
                    self.0.writing.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let now = self.0.writing.fetch_add(1, Ordering::SeqCst) + 1;
            self.0.most_writing.fetch_max(now, Ordering::SeqCst);
            let _writing = Writing(&self.0);
            let pace = *self.0.write_pace.lock().unwrap();
            let mut stream = r.into_inner();
            let (mut resource, mut data) = (String::new(), Vec::new());
            while let Some(m) = stream.message().await? {
                if resource.is_empty() {
                    resource = m.resource_name.clone();
                    self.0.write_offsets.lock().unwrap().push(m.write_offset);
                    data = self.0.partial.lock().unwrap().remove(&resource).unwrap_or_default();
                    if m.write_offset != data.len() as i64 {
                        data.clear();
                        if m.write_offset != 0 {
                            return Err(tonic::Status::invalid_argument("bad write_offset"));
                        }
                    }
                }
                data.extend(m.data);
                tokio::time::sleep(pace).await;
                if m.finish_write {
                    break;
                }
                let mut breaks = self.0.write_breaks.lock().unwrap();
                if *breaks > 0 {
                    *breaks -= 1;
                    self.0.partial.lock().unwrap().insert(resource, data);
                    return Err(tonic::Status::unavailable("the stream broke"));
                }
            }
            let parts: Vec<&str> = resource.split('/').collect();
            let blob = if parts.contains(&"compressed-blobs") { zstd::bulk::decompress(&data, 1 << 30).unwrap() } else { data };
            self.put(blob);
            Ok(tonic::Response::new(bs::WriteResponse { committed_size: 0 }))
        }

        async fn query_write_status(&self, r: tonic::Request<bs::QueryWriteStatusRequest>) -> Result<tonic::Response<bs::QueryWriteStatusResponse>, tonic::Status> {
            let committed = self.0.partial.lock().unwrap().get(&r.get_ref().resource_name).map(|p| p.len() as i64).ok_or_else(|| tonic::Status::not_found(""))?;
            Ok(tonic::Response::new(bs::QueryWriteStatusResponse { committed_size: committed, complete: false }))
        }
    }

    /// A TCP proxy in front of `upstream` dropping its first connection at
    /// the first request (once the HTTP/2 handshake went through): a reset,
    /// as a server restarting or a load balancer gives. Its endpoint.
    async fn resetting_proxy(upstream: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let upstream: std::net::SocketAddr = upstream.trim_start_matches("grpc://").parse().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut first = true;
            while let Ok((mut client, _)) = listener.accept().await {
                let mut server = tokio::net::TcpStream::connect(upstream).await.unwrap();
                if !std::mem::take(&mut first) {
                    tokio::spawn(async move { tokio::io::copy_bidirectional(&mut client, &mut server).await });
                    continue;
                }
                tokio::spawn(async move {
                    let (mut cr, mut cw) = client.into_split();
                    let (mut sr, mut sw) = server.into_split();
                    let down = tokio::spawn(async move { tokio::io::copy(&mut sr, &mut cw).await });
                    let mut preface = [0u8; 24];
                    if cr.read_exact(&mut preface).await.is_err() || sw.write_all(&preface).await.is_err() {
                        return;
                    }
                    // HTTP/2 frames up to the first HEADERS one (a request).
                    loop {
                        let mut h = [0u8; 9];
                        if cr.read_exact(&mut h).await.is_err() || h[3] == 1 {
                            break;
                        }
                        let mut payload = vec![0u8; ((h[0] as usize) << 16) | ((h[1] as usize) << 8) | h[2] as usize];
                        if cr.read_exact(&mut payload).await.is_err() || sw.write_all(&h).await.is_err() || sw.write_all(&payload).await.is_err() {
                            break;
                        }
                    }
                    down.abort();
                });
            }
        });
        format!("grpc://{addr}")
    }

    #[tokio::test]
    async fn connections_broken_before_any_answer_are_made_again() {
        let cas = TestCas::default();
        let proxy = resetting_proxy(&cas.serve().await).await;
        let mut c = Client::connect(options(&proxy)).await.unwrap().with_retries(5, Duration::from_secs(10));
        c.capabilities().await.unwrap();
    }

    #[tokio::test]
    async fn huge_sizes_arent_allocated() {
        let c = client();
        let data = b"hello, world".repeat(100);
        let lie = re::Digest { hash: digest_of(&data).hash, size_bytes: 1 << 46 };
        let err = c.decode(re::compressor::Value::Zstd as i32, zstd::bulk::compress(&data, 3).unwrap(), &lie).unwrap_err();
        assert!(format!("{err:#}").contains("received 1200 bytes"), "{err:#}");
    }

    #[tokio::test]
    async fn reads_stop_at_the_size_their_digest_says() {
        let cas = TestCas::default();
        *cas.0.endless.lock().unwrap() = Some(256 << 20);
        let c = cas.client(0, Duration::from_secs(10)).await;
        let d = re::Digest { hash: digest_of(b"x").hash, size_bytes: 4 << 20 };
        let err = c.read_blob(&d).await.unwrap_err();
        assert!(format!("{err:#}").contains("more than the"), "{err:#}");
        // What the server sent ahead of the client, at most its windows.
        assert!(cas.0.streamed.load(Ordering::SeqCst) < 64 << 20, "{}", cas.0.streamed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unasked_missing_digests_are_left_out() {
        let cas = TestCas::default();
        *cas.0.unasked.lock().unwrap() = Some(digest_of(b"never asked"));
        let c = cas.client(0, Duration::from_secs(10)).await;
        let mut set = BlobSet::default();
        let d = set.insert_message(&re::Command { arguments: vec!["true".into()], ..Default::default() });
        assert_eq!(tokio::spawn(async move { c.upload_blobs(&set).await.unwrap().0 }).await.expect("the upload panicked"), 1);
        assert!(cas.has(&d));
    }

    #[tokio::test]
    async fn batches_of_many_small_blobs_stay_within_the_limit() {
        let cas = TestCas::default();
        let c = cas.client(0, Duration::from_secs(30)).await;
        let mut set = BlobSet::default();
        let blobs: Vec<Vec<u8>> = (0..40_000u32).map(|i| format!("{i:0100}").into_bytes()).collect();
        for b in &blobs {
            set.insert(digest_of(b), BlobSource::Bytes(b.clone()));
        }
        assert_eq!(c.upload_blobs(&set).await.unwrap().0, 40_000);
        let largest = cas.0.largest_batch.load(Ordering::SeqCst);
        assert!(largest <= c.max_batch_bytes as u64, "{largest}");
        // Read back: the answers stay within the client's limit too.
        let digests: Vec<re::Digest> = blobs.iter().map(|b| digest_of(b)).collect();
        assert_eq!(c.read_blobs(&digests).await.unwrap().len(), 40_000);
    }

    #[tokio::test]
    async fn uploads_are_bounded_across_calls() {
        let cas = TestCas::default();
        *cas.0.write_pace.lock().unwrap() = Duration::from_millis(50);
        let mut c = cas.client(0, Duration::from_secs(30)).await;
        // Blobs of 100 KiB go through ByteStream.
        c.max_batch_bytes = 64 << 10;
        let mut calls = tokio::task::JoinSet::new();
        for call in 0..6u8 {
            let mut set = BlobSet::default();
            for i in 0..8u8 {
                let data = vec![call * 8 + i; 100 << 10];
                set.insert(digest_of(&data), BlobSource::Bytes(data));
            }
            let c = c.clone();
            calls.spawn(async move { c.upload_blobs(&set).await.unwrap() });
        }
        while let Some(r) = calls.join_next().await {
            assert_eq!(r.unwrap().0, 8);
        }
        let most = cas.0.most_writing.load(Ordering::SeqCst);
        assert!(most <= MAX_UPLOADS as u64, "{most} writes at once");
    }

    /// A blob of `size` bytes, not compressing to nothing.
    fn noise(size: usize) -> Vec<u8> {
        let mut x = 0x9e3779b97f4a7c15u64;
        (0..size)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn reads_resume_where_they_broke() {
        let dir = tempfile::tempdir().unwrap();
        let cas = TestCas::default();
        let d = cas.put(noise(5 << 20));
        let c = cas.client(2, Duration::from_secs(10)).await;
        // Each attempt gets 1 MiB further: more breaks than retries.
        *cas.0.read_breaks.lock().unwrap() = (3, 1 << 20);
        assert_eq!(digest_of(&c.read_blob(&d).await.unwrap()), d);
        assert_eq!(std::mem::take(&mut *cas.0.read_offsets.lock().unwrap()), [0, 1 << 20, 2 << 20, 3 << 20]);
        *cas.0.read_breaks.lock().unwrap() = (3, 1 << 20);
        let path = dir.path().join("blob");
        c.read_blob_to_file(&d, &path).await.unwrap();
        assert_eq!(digest_of(&std::fs::read(&path).unwrap()), d);
        assert_eq!(std::mem::take(&mut *cas.0.read_offsets.lock().unwrap()), [0, 1 << 20, 2 << 20, 3 << 20]);
        // Compressed, a read starts over; the breaks count.
        cas.0.zstd.store(true, Ordering::SeqCst);
        let c = cas.client(2, Duration::from_secs(10)).await;
        *cas.0.read_breaks.lock().unwrap() = (2, 1 << 20);
        c.read_blob_to_file(&d, &path).await.unwrap();
        assert_eq!(digest_of(&std::fs::read(&path).unwrap()), d);
        assert_eq!(std::mem::take(&mut *cas.0.read_offsets.lock().unwrap()), [0, 0, 0]);
        *cas.0.read_breaks.lock().unwrap() = (3, 1 << 20);
        assert!(c.read_blob(&d).await.is_err());
    }

    #[tokio::test]
    async fn writes_resume_where_the_server_got_to() {
        let cas = TestCas::default();
        let c = cas.client(2, Duration::from_secs(10)).await;
        let data = noise(5 << 20);
        let d = digest_of(&data);
        // Each attempt gets a message further: more breaks than retries.
        *cas.0.write_breaks.lock().unwrap() = 3;
        c.bytestream_write(&d, &BlobSource::Bytes(data)).await.unwrap();
        assert!(cas.has(&d));
        assert_eq!(*cas.0.write_offsets.lock().unwrap(), [0, 1 << 20, 2 << 20, 3 << 20]);
    }

    #[test]
    fn stopped_reads_leave_no_file() {
        // One blocking thread, kept busy: the writer of the blob read
        // starts only once the read was stopped and the file removed.
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(1).enable_all().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        rt.block_on(async {
            let cas = TestCas::default();
            let d = cas.put(noise(5 << 20));
            *cas.0.read_hold.lock().unwrap() = Duration::from_secs(30);
            let c = cas.client(0, Duration::from_secs(60)).await;
            let (release, busy) = std::sync::mpsc::channel::<()>();
            let busy = tokio::task::spawn_blocking(move || busy.recv());
            assert!(tokio::time::timeout(Duration::from_millis(500), c.read_blob_to_file(&d, &path)).await.is_err());
            // As its caller does.
            let _ = std::fs::remove_file(&path);
            release.send(()).unwrap();
            busy.await.unwrap().unwrap();
            // Queued after the writer.
            tokio::task::spawn_blocking(|| ()).await.unwrap();
        });
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn any_answer_counts() {
        let cas = TestCas::default();
        let endpoint = cas.serve().await;
        let c = Client::connect(options(&endpoint)).await.unwrap().with_retries(2, Duration::from_millis(300));
        // A write is the server's first answer: a call it leaves
        // unanswered next is made again.
        let data = b"blob".to_vec();
        c.bytestream_write(&digest_of(&data), &BlobSource::Bytes(data)).await.unwrap();
        *cas.0.unanswered.lock().unwrap() = 1;
        assert_eq!(c.get_action_result(&digest_of(b"action")).await.unwrap(), None);
    }

    #[test]
    fn endpoint_hosts() {
        assert_eq!(endpoint_host("[::1]:8980"), "::1");
        assert_eq!(endpoint_host("rbe.example.com:443/x"), "rbe.example.com");
        assert_eq!(endpoint_host("127.0.0.1"), "127.0.0.1");
    }
}
