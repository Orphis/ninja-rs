//! An in-process fake REAPI v2 server for tests: CAS, ActionCache,
//! Capabilities, ByteStream and an Execution service that runs actions as
//! local subprocesses in temporary exec roots (like a single local worker).
//!
//! It is deliberately simple (no output directories) but follows the
//! protocol closely enough to exercise a real client.
//!
//! It is also a Build Event Service: it records the build events it
//! receives (decoded) and acks each one.
//!
//! Split and spliced blobs are off by default; [`State::set_chunking`]
//! turns them on, as in RE API v2.12 (`SplitBlob` / `SpliceBlob`) or v2.13
//! (`GetChunkMapping` / `RegisterChunkMapping`), with FastCDC 2020 and
//! REAPI's recommended parameters.
//!
//! zstd compression is off by default too; [`State::set_compression`]
//! advertises it for ByteStream, batch reads and batch updates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use bazel_remote_apis::build::bazel::semver::SemVer;
use bazel_remote_apis::google::bytestream as bs;
use bazel_remote_apis::google::longrunning::{Operation, operation};
use bazel_remote_apis::google::protobuf::Any;
use bazel_remote_apis::google::rpc::Status as RpcStatus;
use prost::Message;
use rbe_bes::proto::build_event_stream as bep;
use rbe_bes::proto::google::devtools::build::v1 as bes;
use sha2::{Digest as _, Sha256};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

const FAST_CDC: i32 = re::chunking_function::Value::FastCdc2020 as i32;

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub executions: u64,
    pub action_cache_hits: u64,
    /// Results stored by clients (UpdateActionResult).
    pub action_cache_updates: u64,
    pub blobs_uploaded: u64,
    pub bytestream_writes: u64,
    /// Blob bytes received (batches and ByteStream).
    pub bytes_uploaded: u64,
    /// Blob bytes sent (batches and ByteStream).
    pub bytes_read: u64,
    /// ByteStream reads of part of a blob (`read_offset`, `read_limit`).
    pub ranged_reads: u64,
    /// Blobs split, and blobs spliced.
    pub splits: u64,
    pub splices: u64,
    /// Blobs read and written compressed (batches and ByteStream).
    pub compressed_reads: u64,
    pub compressed_writes: u64,
    /// Calls failed on purpose (`State::fail_next`), and operations
    /// followed again (`WaitExecution`).
    pub injected_failures: u64,
    pub wait_executions: u64,
    /// Most reads (batches and ByteStream) served at once.
    pub max_concurrent_reads: u64,
}

#[derive(Default)]
pub struct State {
    cas: Mutex<HashMap<String, Vec<u8>>>,
    ac: Mutex<HashMap<String, re::ActionResult>>,
    pub stats: Mutex<Stats>,
    /// Actions as executed: (argv, working directory).
    pub executed: Mutex<Vec<(Vec<String>, String)>>,
    /// The `RequestMetadata` of each `Execute` call.
    pub request_metadata: Mutex<Vec<re::RequestMetadata>>,
    /// The `x-` headers of each `Execute` call (`--remote_header`).
    pub headers: Mutex<Vec<(String, String)>>,
    /// Build events received, in order, and the lifecycle events.
    pub build_events: Mutex<Vec<bep::BuildEvent>>,
    pub lifecycle_events: Mutex<Vec<bes::PublishLifecycleEventRequest>>,
    /// Whether a build event stream ended with `ComponentStreamFinished`.
    pub bes_streams_finished: Mutex<u32>,
    /// The RE API minor version whose split/splice RPCs are served, if any.
    chunking: Mutex<Option<u32>>,
    /// zstd advertised and served, and for batch updates (unless
    /// `set_batch_compression(false)`).
    compression: std::sync::atomic::AtomicBool,
    no_batch_compression: std::sync::atomic::AtomicBool,
    /// Empties the CAS when the next Execute arrives (an eviction during a
    /// build).
    evict_on_execute: std::sync::atomic::AtomicBool,
    /// Execute fails with INTERNAL (a broken remote).
    fail_executions: std::sync::atomic::AtomicBool,
    /// Milliseconds Execute waits first (a slow remote).
    execute_delay_ms: AtomicU64,
    /// Milliseconds a ByteStream write waits before it reads the blob,
    /// and a read before it answers.
    write_delay_ms: AtomicU64,
    read_delay_ms: AtomicU64,
    reads_in_flight: AtomicU64,
    /// Calls to fail with UNAVAILABLE, by method (see `fail_next`).
    failures: Mutex<HashMap<&'static str, u32>>,
    /// Executions whose stream breaks once the operation is named (see
    /// `break_next_executions`), and the operations they leave to follow.
    break_executions: Mutex<u64>,
    operations: Mutex<HashMap<String, Operation>>,
    /// Rewrites every result returned, cached or executed (a misbehaving
    /// server, or a poisoned cache).
    tamper: Mutex<Option<Tamper>>,
    base: PathBuf,
    counter: AtomicU64,
}

/// See `State::set_tamper`.
pub type Tamper = Box<dyn Fn(&mut re::ActionResult) + Send + Sync>;

impl State {
    /// Rewrites every action result the server returns (from the action
    /// cache or an execution) with `f`; `None` stops.
    pub fn set_tamper(&self, f: Option<Tamper>) {
        *self.tamper.lock().unwrap() = f;
    }
    fn tampered(&self, mut r: re::ActionResult) -> re::ActionResult {
        if let Some(f) = self.tamper.lock().unwrap().as_ref() {
            f(&mut r);
        }
        r
    }
    /// Replaces a blob's bytes with others of the same size (corruption).
    pub fn corrupt_blob(&self, hash: &str) {
        if let Some(data) = self.cas.lock().unwrap().get_mut(hash) {
            for b in data.iter_mut() {
                *b = !*b;
            }
        }
    }
    pub fn put(&self, data: Vec<u8>) -> re::Digest {
        let d = digest_of(&data);
        self.cas.lock().unwrap().insert(d.hash.clone(), data);
        d
    }
    pub fn get(&self, d: &re::Digest) -> Option<Vec<u8>> {
        if d.size_bytes == 0 {
            return Some(Vec::new());
        }
        self.cas.lock().unwrap().get(&d.hash).cloned()
    }
    pub fn clear_action_cache(&self) {
        self.ac.lock().unwrap().clear();
    }
    /// Evicts every blob when the next Execute request arrives, after the
    /// client's checks of what the CAS holds.
    pub fn evict_on_next_execute(&self) {
        self.evict_on_execute.store(true, Ordering::SeqCst);
    }
    /// Makes every Execute wait first, as a slow or busy remote would.
    pub fn set_execute_delay(&self, delay: std::time::Duration) {
        self.execute_delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    }
    /// Makes the next `times` calls of `method` (`FindMissingBlobs`,
    /// `BatchUpdateBlobs`, `BatchReadBlobs`, `GetActionResult`,
    /// `UpdateActionResult`, `GetCapabilities`, `Read`, `Write`, `Execute`)
    /// fail with UNAVAILABLE, as a transient failure would.
    pub fn fail_next(&self, method: &'static str, times: u32) {
        *self.failures.lock().unwrap().entry(method).or_default() += times;
    }
    fn injected(&self, method: &'static str) -> Result<(), Status> {
        let mut failures = self.failures.lock().unwrap();
        match failures.get_mut(method) {
            Some(n) if *n > 0 => {
                *n -= 1;
                self.stats.lock().unwrap().injected_failures += 1;
                Err(Status::unavailable(format!("injected failure of {method}")))
            }
            _ => Ok(()),
        }
    }
    /// The next `times` executions run, but their stream breaks after the
    /// operation's name: the client must follow it (`WaitExecution`).
    pub fn break_next_executions(&self, times: u64) {
        *self.break_executions.lock().unwrap() += times;
    }
    /// Makes every ByteStream write wait first, so that uploads overlap.
    pub fn set_write_delay(&self, delay: std::time::Duration) {
        self.write_delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    }
    /// Makes every read (batches and ByteStream) wait first, so that
    /// concurrent ones overlap (see `Stats::max_concurrent_reads`).
    pub fn set_read_delay(&self, delay: std::time::Duration) {
        self.read_delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
    }
    /// A read being served, until it's dropped.
    async fn reading(self: &Arc<Self>) -> Reading {
        let now = self.reads_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut stats = self.stats.lock().unwrap();
            stats.max_concurrent_reads = stats.max_concurrent_reads.max(now);
        }
        let delay = self.read_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        Reading(self.clone())
    }
    /// Makes every Execute fail, as a broken remote service would.
    pub fn set_fail_executions(&self, on: bool) {
        self.fail_executions.store(on, Ordering::SeqCst);
    }
    pub fn clear_cas(&self) {
        self.cas.lock().unwrap().clear();
    }
    /// Serves split and spliced blobs as RE API 2.`minor` does (12 or 13),
    /// or not at all. Clients see it in the capabilities they fetch next.
    pub fn set_chunking(&self, minor: Option<u32>) {
        *self.chunking.lock().unwrap() = minor;
    }
    /// Advertises and serves zstd (ByteStream, batch reads and updates).
    pub fn set_compression(&self, on: bool) {
        self.compression.store(on, Ordering::SeqCst);
    }
    /// Whether batch updates may be compressed too, when compression is on
    /// (EngFlow's don't).
    pub fn set_batch_compression(&self, on: bool) {
        self.no_batch_compression.store(!on, Ordering::SeqCst);
    }
    fn compressing(&self) -> bool {
        self.compression.load(Ordering::SeqCst)
    }
    fn compressing_batches(&self) -> bool {
        self.compressing() && !self.no_batch_compression.load(Ordering::SeqCst)
    }
    fn chunking_api(&self, minor: u32) -> Result<(), Status> {
        if *self.chunking.lock().unwrap() == Some(minor) { Ok(()) } else { Err(Status::unimplemented("not with this API version")) }
    }
    /// Splits a blob it holds with FastCDC 2020 (512 KiB on average),
    /// storing the chunks.
    fn split(&self, d: Option<re::Digest>) -> Result<Vec<re::Digest>, Status> {
        let d = d.unwrap_or_default();
        let data = self.get(&d).ok_or_else(|| Status::not_found("blob not found"))?;
        let avg = 512 * 1024;
        let chunks = fastcdc::v2020::FastCDC::with_level(&data, avg / 4, avg, avg * 4, fastcdc::v2020::Normalization::Level2)
            .map(|c| self.put(data[c.offset..c.offset + c.length].to_vec()))
            .collect();
        self.stats.lock().unwrap().splits += 1;
        Ok(chunks)
    }
    /// Stores a blob made of chunks it holds, after checking its digest.
    fn splice(&self, d: Option<re::Digest>, chunks: &[re::Digest]) -> Result<re::Digest, Status> {
        let d = d.ok_or_else(|| Status::invalid_argument("no blob digest"))?;
        let mut data = Vec::new();
        for c in chunks {
            data.extend(self.get(c).ok_or_else(|| Status::not_found(format!("chunk {} not found", c.hash)))?);
        }
        if digest_of(&data) != d {
            return Err(Status::invalid_argument("the chunks don't make up the blob"));
        }
        self.put(data);
        self.stats.lock().unwrap().splices += 1;
        Ok(d)
    }
}

struct Reading(Arc<State>);

impl Drop for Reading {
    fn drop(&mut self) {
        self.0.reads_in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn digest_of(data: &[u8]) -> re::Digest {
    re::Digest { hash: hex::encode(Sha256::digest(data)), size_bytes: data.len() as i64 }
}

fn status(code: i32, message: impl Into<String>) -> RpcStatus {
    RpcStatus { code, message: message.into(), details: vec![] }
}

#[derive(Clone)]
struct Svc(Arc<State>);

#[tonic::async_trait]
impl re::content_addressable_storage_server::ContentAddressableStorage for Svc {
    async fn find_missing_blobs(&self, r: Request<re::FindMissingBlobsRequest>) -> Result<Response<re::FindMissingBlobsResponse>, Status> {
        self.0.injected("FindMissingBlobs")?;
        let cas = self.0.cas.lock().unwrap();
        let missing = r.into_inner().blob_digests.into_iter().filter(|d| d.size_bytes != 0 && !cas.contains_key(&d.hash)).collect();
        Ok(Response::new(re::FindMissingBlobsResponse { missing_blob_digests: missing }))
    }

    async fn batch_update_blobs(&self, r: Request<re::BatchUpdateBlobsRequest>) -> Result<Response<re::BatchUpdateBlobsResponse>, Status> {
        self.0.injected("BatchUpdateBlobs")?;
        let mut responses = Vec::new();
        let zstd = re::compressor::Value::Zstd as i32;
        for req in r.into_inner().requests {
            let d = req.digest.unwrap_or_default();
            let wire = req.data.len() as u64;
            let data = match req.compressor {
                0 => Ok(req.data),
                c if c == zstd && self.0.compressing_batches() => zstd::bulk::decompress(&req.data, d.size_bytes as usize).map_err(|e| e.to_string()),
                c => Err(format!("compressor {c} not supported")),
            };
            let st = match data {
                Ok(data) if digest_of(&data) == d => {
                    let mut stats = self.0.stats.lock().unwrap();
                    stats.blobs_uploaded += 1;
                    stats.bytes_uploaded += wire;
                    stats.compressed_writes += (req.compressor == zstd) as u64;
                    self.0.cas.lock().unwrap().insert(d.hash.clone(), data);
                    status(0, "")
                }
                Ok(_) => status(3, "digest mismatch"),
                Err(e) => status(3, e),
            };
            responses.push(re::batch_update_blobs_response::Response { digest: Some(d), status: Some(st) });
        }
        Ok(Response::new(re::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(&self, r: Request<re::BatchReadBlobsRequest>) -> Result<Response<re::BatchReadBlobsResponse>, Status> {
        self.0.injected("BatchReadBlobs")?;
        let _reading = self.0.reading().await;
        let mut responses = Vec::new();
        let r = r.into_inner();
        let zstd = re::compressor::Value::Zstd as i32;
        let compress = self.0.compressing() && r.acceptable_compressors.contains(&zstd);
        for d in r.digests {
            let (data, compressor, st) = match self.0.get(&d) {
                Some(data) => {
                    let (data, compressor) = if compress { (zstd::bulk::compress(&data, 1).unwrap(), zstd) } else { (data, 0) };
                    let mut stats = self.0.stats.lock().unwrap();
                    stats.bytes_read += data.len() as u64;
                    stats.compressed_reads += compress as u64;
                    (data, compressor, status(0, ""))
                }
                None => (vec![], 0, status(5, "not found")),
            };
            responses.push(re::batch_read_blobs_response::Response { digest: Some(d), data, compressor, status: Some(st) });
        }
        Ok(Response::new(re::BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = BoxStream<re::GetTreeResponse>;
    async fn get_tree(&self, _r: Request<re::GetTreeRequest>) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("GetTree"))
    }
    async fn split_blob(&self, r: Request<re::SplitBlobRequest>) -> Result<Response<re::SplitBlobResponse>, Status> {
        self.0.chunking_api(12)?;
        let chunk_digests = self.0.split(r.into_inner().blob_digest)?;
        Ok(Response::new(re::SplitBlobResponse { chunk_digests, chunking_function: FAST_CDC }))
    }
    type GetChunkMappingStream = BoxStream<re::GetChunkMappingResponse>;
    async fn get_chunk_mapping(&self, r: Request<re::GetChunkMappingRequest>) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        self.0.chunking_api(13)?;
        let chunks = self.0.split(r.into_inner().blob_digest)?;
        // Streamed a few digests at a time, the function in the first
        // message only.
        let responses: Vec<Result<re::GetChunkMappingResponse, Status>> = chunks
            .chunks(3)
            .enumerate()
            .map(|(i, part)| Ok(re::GetChunkMappingResponse { chunk_digests: part.to_vec(), chunking_function: if i == 0 { FAST_CDC } else { 0 } }))
            .collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(responses))))
    }
    async fn splice_blob(&self, r: Request<re::SpliceBlobRequest>) -> Result<Response<re::SpliceBlobResponse>, Status> {
        self.0.chunking_api(12)?;
        let r = r.into_inner();
        let d = self.0.splice(r.blob_digest, &r.chunk_digests)?;
        Ok(Response::new(re::SpliceBlobResponse { blob_digest: Some(d) }))
    }
    async fn register_chunk_mapping(
        &self,
        r: Request<Streaming<re::RegisterChunkMappingRequest>>,
    ) -> Result<Response<re::RegisterChunkMappingResponse>, Status> {
        self.0.chunking_api(13)?;
        let mut stream = r.into_inner();
        let (mut blob, mut chunks) = (None, Vec::new());
        while let Some(m) = stream.next().await {
            let m = m?;
            if blob.is_none() {
                blob = m.blob_digest;
            }
            chunks.extend(m.chunk_digests);
        }
        let d = self.0.splice(blob, &chunks)?;
        Ok(Response::new(re::RegisterChunkMappingResponse { blob_digest: Some(d) }))
    }
}

#[tonic::async_trait]
impl re::action_cache_server::ActionCache for Svc {
    async fn get_action_result(&self, r: Request<re::GetActionResultRequest>) -> Result<Response<re::ActionResult>, Status> {
        self.0.injected("GetActionResult")?;
        let d = r.into_inner().action_digest.unwrap_or_default();
        match self.0.ac.lock().unwrap().get(&d.hash) {
            Some(ar) => {
                self.0.stats.lock().unwrap().action_cache_hits += 1;
                Ok(Response::new(self.0.tampered(ar.clone())))
            }
            None => Err(Status::not_found("not in action cache")),
        }
    }
    async fn update_action_result(&self, r: Request<re::UpdateActionResultRequest>) -> Result<Response<re::ActionResult>, Status> {
        self.0.injected("UpdateActionResult")?;
        let r = r.into_inner();
        let ar = r.action_result.unwrap_or_default();
        self.0.ac.lock().unwrap().insert(r.action_digest.unwrap_or_default().hash, ar.clone());
        self.0.stats.lock().unwrap().action_cache_updates += 1;
        Ok(Response::new(ar))
    }
}

#[tonic::async_trait]
impl re::capabilities_server::Capabilities for Svc {
    async fn get_capabilities(&self, _r: Request<re::GetCapabilitiesRequest>) -> Result<Response<re::ServerCapabilities>, Status> {
        self.0.injected("GetCapabilities")?;
        let sha = re::digest_function::Value::Sha256 as i32;
        let chunking = *self.0.chunking.lock().unwrap();
        Ok(Response::new(re::ServerCapabilities {
            cache_capabilities: Some(re::CacheCapabilities {
                digest_functions: vec![sha],
                action_cache_update_capabilities: Some(re::ActionCacheUpdateCapabilities { update_enabled: true }),
                max_batch_total_size_bytes: 4 * 1024 * 1024,
                split_blob_support: chunking.is_some(),
                splice_blob_support: chunking.is_some(),
                fast_cdc_2020_params: chunking.map(|_| re::FastCdc2020Params { avg_chunk_size_bytes: 512 * 1024, seed: 0 }),
                supported_compressors: if self.0.compressing() { vec![re::compressor::Value::Zstd as i32] } else { vec![] },
                supported_batch_update_compressors: if self.0.compressing_batches() { vec![re::compressor::Value::Zstd as i32] } else { vec![] },
                ..Default::default()
            }),
            execution_capabilities: Some(re::ExecutionCapabilities {
                digest_function: sha,
                exec_enabled: true,
                digest_functions: vec![sha],
                ..Default::default()
            }),
            low_api_version: Some(SemVer { major: 2, minor: 0, patch: 0, prerelease: String::new() }),
            high_api_version: Some(SemVer { major: 2, minor: chunking.unwrap_or(3) as i32, patch: 0, prerelease: String::new() }),
            ..Default::default()
        }))
    }
}

/// The blob a ByteStream resource names, and whether it's compressed
/// (`compressed-blobs/zstd/<hash>/<size>`).
fn parse_resource(name: &str) -> Option<(re::Digest, bool)> {
    let parts: Vec<&str> = name.split('/').collect();
    let (i, compressed) = match parts.iter().position(|p| *p == "blobs") {
        Some(i) => (i, false),
        None => (parts.iter().position(|p| *p == "compressed-blobs").filter(|&i| parts.get(i + 1) == Some(&"zstd"))? + 1, true),
    };
    let hash = parts.get(i + 1)?.to_string();
    let size = parts.get(i + 2)?.parse().ok()?;
    Some((re::Digest { hash, size_bytes: size }, compressed))
}

#[tonic::async_trait]
impl bs::byte_stream_server::ByteStream for Svc {
    type ReadStream = BoxStream<bs::ReadResponse>;
    async fn read(&self, r: Request<bs::ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        self.0.injected("Read")?;
        let _reading = self.0.reading().await;
        let r = r.into_inner();
        let (d, compressed) = parse_resource(&r.resource_name).ok_or_else(|| Status::invalid_argument("bad resource name"))?;
        let data = self.0.get(&d).ok_or_else(|| Status::not_found("blob not found"))?;
        if compressed {
            if !self.0.compressing() || r.read_offset != 0 || r.read_limit != 0 {
                return Err(Status::invalid_argument("compressed reads: zstd, whole blobs only"));
            }
            let data = zstd::bulk::compress(&data, 1).map_err(|e| Status::internal(e.to_string()))?;
            let mut stats = self.0.stats.lock().unwrap();
            stats.bytes_read += data.len() as u64;
            stats.compressed_reads += 1;
            let chunks: Vec<Result<bs::ReadResponse, Status>> = data.chunks(64 * 1024).map(|c| Ok(bs::ReadResponse { data: c.to_vec() })).collect();
            return Ok(Response::new(Box::pin(tokio_stream::iter(chunks))));
        }
        if r.read_offset < 0 || r.read_limit < 0 || r.read_offset as usize > data.len() {
            return Err(Status::out_of_range("bad read_offset or read_limit"));
        }
        let start = r.read_offset as usize;
        let end = if r.read_limit > 0 { (start + r.read_limit as usize).min(data.len()) } else { data.len() };
        {
            let mut stats = self.0.stats.lock().unwrap();
            stats.bytes_read += (end - start) as u64;
            if r.read_offset > 0 || r.read_limit > 0 {
                stats.ranged_reads += 1;
            }
        }
        let chunks: Vec<Result<bs::ReadResponse, Status>> = data[start..end].chunks(64 * 1024).map(|c| Ok(bs::ReadResponse { data: c.to_vec() })).collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
    }

    async fn write(&self, r: Request<Streaming<bs::WriteRequest>>) -> Result<Response<bs::WriteResponse>, Status> {
        self.0.injected("Write")?;
        let delay = self.0.write_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let mut stream = r.into_inner();
        let mut resource = String::new();
        let mut data = Vec::new();
        while let Some(msg) = stream.next().await {
            let msg = msg?;
            if resource.is_empty() {
                resource = msg.resource_name.clone();
            }
            data.extend_from_slice(&msg.data);
            if msg.finish_write {
                break;
            }
        }
        let (d, compressed) = parse_resource(&resource).ok_or_else(|| Status::invalid_argument("bad resource name"))?;
        let wire = data.len() as i64;
        let data = if compressed {
            if !self.0.compressing() {
                return Err(Status::invalid_argument("compressed writes aren't supported"));
            }
            zstd::bulk::decompress(&data, d.size_bytes as usize).map_err(|e| Status::invalid_argument(e.to_string()))?
        } else {
            data
        };
        if digest_of(&data) != d {
            return Err(Status::invalid_argument("digest mismatch"));
        }
        self.0.cas.lock().unwrap().insert(d.hash, data);
        let mut stats = self.0.stats.lock().unwrap();
        stats.bytestream_writes += 1;
        stats.bytes_uploaded += wire as u64;
        stats.compressed_writes += compressed as u64;
        // The bytes received; -1 would do for a compressed write.
        Ok(Response::new(bs::WriteResponse { committed_size: wire }))
    }

    async fn query_write_status(&self, _r: Request<bs::QueryWriteStatusRequest>) -> Result<Response<bs::QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("QueryWriteStatus"))
    }
}

impl Svc {
    fn materialize(&self, dir: &re::Directory, path: &Path) -> Result<(), String> {
        std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
        for f in &dir.files {
            let d = f.digest.clone().unwrap_or_default();
            let data = self.0.get(&d).ok_or_else(|| format!("missing blob {} for {}", d.hash, f.name))?;
            let p = path.join(&f.name);
            std::fs::write(&p, data).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            if f.is_executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            }
        }
        for sub in &dir.directories {
            let d = sub.digest.clone().unwrap_or_default();
            let data = self.0.get(&d).ok_or_else(|| format!("missing directory {}", d.hash))?;
            let sd = re::Directory::decode(data.as_slice()).map_err(|e| e.to_string())?;
            self.materialize(&sd, &path.join(&sub.name))?;
        }
        Ok(())
    }

    fn run_action(&self, action_digest: &re::Digest) -> Result<re::ActionResult, RpcStatus> {
        let missing = |what: &str| status(9, format!("missing {what}"));
        let action = re::Action::decode(self.0.get(action_digest).ok_or_else(|| missing("action"))?.as_slice()).map_err(|e| status(3, e.to_string()))?;
        let cmd_d = action.command_digest.clone().unwrap_or_default();
        let command = re::Command::decode(self.0.get(&cmd_d).ok_or_else(|| missing("command"))?.as_slice()).map_err(|e| status(3, e.to_string()))?;
        let root_d = action.input_root_digest.clone().unwrap_or_default();
        let root_dir = re::Directory::decode(self.0.get(&root_d).ok_or_else(|| missing("input root"))?.as_slice()).map_err(|e| status(3, e.to_string()))?;
        let n = self.0.counter.fetch_add(1, Ordering::SeqCst);
        let root = self.0.base.join(format!("exec-{n}"));
        let _ = std::fs::remove_dir_all(&root);
        self.materialize(&root_dir, &root).map_err(|e| status(9, e))?;
        let cwd = root.join(&command.working_directory);
        std::fs::create_dir_all(&cwd).map_err(|e| status(13, e.to_string()))?;
        #[allow(deprecated)]
        let outputs: Vec<String> = if command.output_paths.is_empty() { command.output_files.clone() } else { command.output_paths.clone() };
        for o in &outputs {
            if let Some(parent) = cwd.join(o).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let argv0 = &command.arguments[0];
        let program = if argv0.contains('/') || argv0.contains('\\') { cwd.join(argv0) } else { PathBuf::from(argv0) };
        self.0.executed.lock().unwrap().push((command.arguments.clone(), command.working_directory.clone()));
        let out = std::process::Command::new(&program)
            .args(&command.arguments[1..])
            .current_dir(&cwd)
            .envs(command.environment_variables.iter().map(|e| (e.name.clone(), e.value.clone())))
            .stdin(std::process::Stdio::null())
            .output();
        let out = match out {
            Ok(o) => o,
            Err(e) => return Err(status(13, format!("spawning {}: {e}", program.display()))),
        };
        self.0.stats.lock().unwrap().executions += 1;
        let mut result = re::ActionResult { exit_code: out.status.code().unwrap_or(1), ..Default::default() };
        result.stdout_digest = Some(self.0.put(out.stdout.clone()));
        result.stdout_raw = out.stdout;
        result.stderr_digest = Some(self.0.put(out.stderr.clone()));
        result.stderr_raw = out.stderr;
        for o in &outputs {
            let p = cwd.join(o);
            if let Ok(data) = std::fs::read(&p) {
                #[cfg(unix)]
                let is_executable = {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::metadata(&p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
                };
                #[cfg(not(unix))]
                let is_executable = false;
                let d = self.0.put(data);
                result.output_files.push(re::OutputFile { path: o.clone(), digest: Some(d), is_executable, ..Default::default() });
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        Ok(result)
    }
}

#[tonic::async_trait]
impl re::execution_server::Execution for Svc {
    type ExecuteStream = BoxStream<Operation>;
    async fn execute(&self, r: Request<re::ExecuteRequest>) -> Result<Response<Self::ExecuteStream>, Status> {
        if let Some(m) = r.metadata().get_bin("build.bazel.remote.execution.v2.requestmetadata-bin")
            && let Ok(bytes) = m.to_bytes()
            && let Ok(meta) = re::RequestMetadata::decode(bytes)
        {
            self.0.request_metadata.lock().unwrap().push(meta);
        }
        for kv in r.metadata().iter() {
            if let tonic::metadata::KeyAndValueRef::Ascii(k, v) = kv
                && k.as_str().starts_with("x-")
            {
                self.0.headers.lock().unwrap().push((k.as_str().to_string(), v.to_str().unwrap_or_default().to_string()));
            }
        }
        if self.0.evict_on_execute.swap(false, Ordering::SeqCst) {
            self.0.clear_cas();
        }
        if self.0.fail_executions.load(Ordering::SeqCst) {
            return Err(Status::internal("injected failure"));
        }
        self.0.injected("Execute")?;
        let delay = self.0.execute_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let r = r.into_inner();
        let d = r.action_digest.unwrap_or_default();
        let cached = if r.skip_cache_lookup { None } else { self.0.ac.lock().unwrap().get(&d.hash).cloned() };
        let resp = match cached {
            Some(ar) => re::ExecuteResponse { result: Some(ar), cached_result: true, status: Some(status(0, "")), ..Default::default() },
            None => {
                let me = self.clone();
                let d2 = d.clone();
                match tokio::task::spawn_blocking(move || me.run_action(&d2)).await.map_err(|e| Status::internal(e.to_string()))? {
                    Ok(ar) => {
                        if ar.exit_code == 0 {
                            self.0.ac.lock().unwrap().insert(d.hash.clone(), ar.clone());
                        }
                        re::ExecuteResponse { result: Some(ar), status: Some(status(0, "")), ..Default::default() }
                    }
                    Err(st) => re::ExecuteResponse { status: Some(st), ..Default::default() },
                }
            }
        };
        let resp = re::ExecuteResponse { result: resp.result.map(|r| self.0.tampered(r)), ..resp };
        let name = format!("operations/{}-{}", d.hash, self.0.counter.fetch_add(1, Ordering::SeqCst));
        let op = Operation {
            name: name.clone(),
            metadata: None,
            done: true,
            result: Some(operation::Result::Response(Any {
                type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".into(),
                value: resp.encode_to_vec(),
            })),
        };
        // A broken stream: the operation named, then an error; it is done
        // for whoever follows it.
        let broken = {
            let mut left = self.0.break_executions.lock().unwrap();
            let broken = *left > 0;
            *left = left.saturating_sub(1);
            broken
        };
        if broken {
            self.0.operations.lock().unwrap().insert(name.clone(), op);
            let named = Operation { name, metadata: None, done: false, result: None };
            return Ok(Response::new(Box::pin(tokio_stream::iter(vec![Ok(named), Err(Status::unavailable("injected: the stream broke"))]))));
        }
        Ok(Response::new(Box::pin(tokio_stream::iter(vec![Ok(op)]))))
    }

    type WaitExecutionStream = BoxStream<Operation>;
    async fn wait_execution(&self, r: Request<re::WaitExecutionRequest>) -> Result<Response<Self::WaitExecutionStream>, Status> {
        let name = r.into_inner().name;
        let op = self.0.operations.lock().unwrap().remove(&name).ok_or_else(|| Status::not_found(format!("no operation {name}")))?;
        self.0.stats.lock().unwrap().wait_executions += 1;
        Ok(Response::new(Box::pin(tokio_stream::iter(vec![Ok(op)]))))
    }
}

#[tonic::async_trait]
impl bes::publish_build_event_server::PublishBuildEvent for Svc {
    async fn publish_lifecycle_event(&self, request: Request<bes::PublishLifecycleEventRequest>) -> Result<Response<()>, Status> {
        self.0.lifecycle_events.lock().unwrap().push(request.into_inner());
        Ok(Response::new(()))
    }

    type PublishBuildToolEventStreamStream = BoxStream<bes::PublishBuildToolEventStreamResponse>;

    async fn publish_build_tool_event_stream(
        &self,
        request: Request<Streaming<bes::PublishBuildToolEventStreamRequest>>,
    ) -> Result<Response<Self::PublishBuildToolEventStreamStream>, Status> {
        let mut input = request.into_inner();
        let state = self.0.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(Ok(req)) = input.next().await {
                let Some(ordered) = req.ordered_build_event else { continue };
                match ordered.event.as_ref().and_then(|e| e.event.as_ref()) {
                    Some(bes::build_event::Event::BazelEvent(any)) => {
                        if let Ok(e) = bep::BuildEvent::decode(any.value.as_slice()) {
                            state.build_events.lock().unwrap().push(e);
                        }
                    }
                    Some(bes::build_event::Event::ComponentStreamFinished(_)) => *state.bes_streams_finished.lock().unwrap() += 1,
                    _ => {}
                }
                let ack = bes::PublishBuildToolEventStreamResponse { stream_id: ordered.stream_id.clone(), sequence_number: ordered.sequence_number };
                if tx.send(Ok(ack)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }
}

/// TLS for the fake server: its certificate and key, and the CA client
/// certificates must chain to (mTLS: clients without one are refused). PEM.
#[derive(Clone)]
pub struct ServerTls {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub client_ca_pem: Option<Vec<u8>>,
}

/// A running fake server.
pub struct FakeServer {
    pub endpoint: String,
    pub state: Arc<State>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl FakeServer {
    /// Starts the server on 127.0.0.1 with an ephemeral port. Actions run
    /// under `work_base`.
    pub async fn start(work_base: &Path) -> std::io::Result<Self> {
        Self::start_on(work_base, 0).await
    }

    /// Like `start`, on a given port (0 for an ephemeral one).
    pub async fn start_on(work_base: &Path, port: u16) -> std::io::Result<Self> {
        Self::start_with(work_base, port, None).await
    }

    /// Like `start_on`, with TLS (the endpoint is then
    /// `grpcs://localhost:<port>`).
    pub async fn start_with(work_base: &Path, port: u16, tls: Option<ServerTls>) -> std::io::Result<Self> {
        std::fs::create_dir_all(work_base)?;
        let state = Arc::new(State { base: work_base.to_path_buf(), ..Default::default() });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let addr = listener.local_addr()?;
        let svc = Svc(state.clone());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        // Without TCP_NODELAY (tonic doesn't set it for an incoming stream),
        // window updates wait behind Nagle's algorithm and delayed ACKs:
        // one large upload crawls at a few MB/s. Adaptive windows, as the
        // client's.
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener).map(|s| {
            s.inspect(|s| {
                let _ = s.set_nodelay(true);
            })
        });
        let mut builder = tonic::transport::Server::builder().http2_adaptive_window(Some(true));
        let endpoint = match &tls {
            Some(t) => {
                use tonic::transport::{Certificate, Identity, ServerTlsConfig};
                let mut cfg = ServerTlsConfig::new().identity(Identity::from_pem(&t.cert_pem, &t.key_pem));
                if let Some(ca) = &t.client_ca_pem {
                    cfg = cfg.client_ca_root(Certificate::from_pem(ca));
                }
                builder = builder.tls_config(cfg).map_err(std::io::Error::other)?;
                format!("grpcs://localhost:{}", addr.port())
            }
            None => format!("grpc://{addr}"),
        };
        tokio::spawn(async move {
            let _ = builder
                .add_service(re::content_addressable_storage_server::ContentAddressableStorageServer::new(svc.clone()))
                .add_service(re::action_cache_server::ActionCacheServer::new(svc.clone()))
                .add_service(re::capabilities_server::CapabilitiesServer::new(svc.clone()))
                .add_service(re::execution_server::ExecutionServer::new(svc.clone()))
                .add_service(bs::byte_stream_server::ByteStreamServer::new(svc.clone()))
                .add_service(bes::publish_build_event_server::PublishBuildEventServer::new(svc))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(Self { endpoint, state, shutdown: Some(tx) })
    }

    pub fn stats(&self) -> Stats {
        self.state.stats.lock().unwrap().clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
