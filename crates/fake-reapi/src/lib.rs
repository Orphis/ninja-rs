//! An in-process fake REAPI v2 server for tests: CAS, ActionCache,
//! Capabilities, ByteStream and an Execution service that runs actions as
//! local subprocesses in temporary exec roots (like a single local worker).
//!
//! It is deliberately simple (no compression, no output directories) but
//! follows the protocol closely enough to exercise a real client.
//!
//! It is also a Build Event Service: it records the build events it
//! receives (decoded) and acks each one.
//!
//! Split and spliced blobs are off by default; [`State::set_chunking`]
//! turns them on, as in RE API v2.12 (`SplitBlob` / `SpliceBlob`) or v2.13
//! (`GetChunkMapping` / `RegisterChunkMapping`), with FastCDC 2020 and
//! REAPI's recommended parameters.

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
    pub blobs_uploaded: u64,
    pub bytestream_writes: u64,
    /// Blob bytes received (batches and ByteStream).
    pub bytes_uploaded: u64,
    /// Blob bytes sent (batches and ByteStream).
    pub bytes_read: u64,
    /// Blobs split, and blobs spliced.
    pub splits: u64,
    pub splices: u64,
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
    /// Build events received, in order, and the lifecycle events.
    pub build_events: Mutex<Vec<bep::BuildEvent>>,
    pub lifecycle_events: Mutex<Vec<bes::PublishLifecycleEventRequest>>,
    /// Whether a build event stream ended with `ComponentStreamFinished`.
    pub bes_streams_finished: Mutex<u32>,
    /// The RE API minor version whose split/splice RPCs are served, if any.
    chunking: Mutex<Option<u32>>,
    base: PathBuf,
    counter: AtomicU64,
}

impl State {
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
    pub fn clear_cas(&self) {
        self.cas.lock().unwrap().clear();
    }
    /// Serves split and spliced blobs as RE API 2.`minor` does (12 or 13),
    /// or not at all. Clients see it in the capabilities they fetch next.
    pub fn set_chunking(&self, minor: Option<u32>) {
        *self.chunking.lock().unwrap() = minor;
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
        let cas = self.0.cas.lock().unwrap();
        let missing = r.into_inner().blob_digests.into_iter().filter(|d| d.size_bytes != 0 && !cas.contains_key(&d.hash)).collect();
        Ok(Response::new(re::FindMissingBlobsResponse { missing_blob_digests: missing }))
    }

    async fn batch_update_blobs(&self, r: Request<re::BatchUpdateBlobsRequest>) -> Result<Response<re::BatchUpdateBlobsResponse>, Status> {
        let mut responses = Vec::new();
        for req in r.into_inner().requests {
            let d = req.digest.unwrap_or_default();
            let st = if digest_of(&req.data) == d {
                let mut stats = self.0.stats.lock().unwrap();
                stats.blobs_uploaded += 1;
                stats.bytes_uploaded += req.data.len() as u64;
                self.0.cas.lock().unwrap().insert(d.hash.clone(), req.data);
                status(0, "")
            } else {
                status(3, "digest mismatch")
            };
            responses.push(re::batch_update_blobs_response::Response { digest: Some(d), status: Some(st) });
        }
        Ok(Response::new(re::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(&self, r: Request<re::BatchReadBlobsRequest>) -> Result<Response<re::BatchReadBlobsResponse>, Status> {
        let mut responses = Vec::new();
        for d in r.into_inner().digests {
            let (data, st) = match self.0.get(&d) {
                Some(data) => {
                    self.0.stats.lock().unwrap().bytes_read += data.len() as u64;
                    (data, status(0, ""))
                }
                None => (vec![], status(5, "not found")),
            };
            responses.push(re::batch_read_blobs_response::Response { digest: Some(d), data, compressor: 0, status: Some(st) });
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
            .map(|(i, part)| {
                Ok(re::GetChunkMappingResponse { chunk_digests: part.to_vec(), chunking_function: if i == 0 { FAST_CDC } else { 0 } })
            })
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
        let d = r.into_inner().action_digest.unwrap_or_default();
        match self.0.ac.lock().unwrap().get(&d.hash) {
            Some(ar) => {
                self.0.stats.lock().unwrap().action_cache_hits += 1;
                Ok(Response::new(ar.clone()))
            }
            None => Err(Status::not_found("not in action cache")),
        }
    }
    async fn update_action_result(&self, r: Request<re::UpdateActionResultRequest>) -> Result<Response<re::ActionResult>, Status> {
        let r = r.into_inner();
        let ar = r.action_result.unwrap_or_default();
        self.0.ac.lock().unwrap().insert(r.action_digest.unwrap_or_default().hash, ar.clone());
        Ok(Response::new(ar))
    }
}

#[tonic::async_trait]
impl re::capabilities_server::Capabilities for Svc {
    async fn get_capabilities(&self, _r: Request<re::GetCapabilitiesRequest>) -> Result<Response<re::ServerCapabilities>, Status> {
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

fn parse_resource(name: &str) -> Option<re::Digest> {
    let parts: Vec<&str> = name.split('/').collect();
    let i = parts.iter().position(|p| *p == "blobs")?;
    let hash = parts.get(i + 1)?.to_string();
    let size = parts.get(i + 2)?.parse().ok()?;
    Some(re::Digest { hash, size_bytes: size })
}

#[tonic::async_trait]
impl bs::byte_stream_server::ByteStream for Svc {
    type ReadStream = BoxStream<bs::ReadResponse>;
    async fn read(&self, r: Request<bs::ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let r = r.into_inner();
        let d = parse_resource(&r.resource_name).ok_or_else(|| Status::invalid_argument("bad resource name"))?;
        let data = self.0.get(&d).ok_or_else(|| Status::not_found("blob not found"))?;
        let start = (r.read_offset as usize).min(data.len());
        self.0.stats.lock().unwrap().bytes_read += (data.len() - start) as u64;
        let chunks: Vec<Result<bs::ReadResponse, Status>> =
            data[start..].chunks(64 * 1024).map(|c| Ok(bs::ReadResponse { data: c.to_vec() })).collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
    }

    async fn write(&self, r: Request<Streaming<bs::WriteRequest>>) -> Result<Response<bs::WriteResponse>, Status> {
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
        let d = parse_resource(&resource).ok_or_else(|| Status::invalid_argument("bad resource name"))?;
        if digest_of(&data) != d {
            return Err(Status::invalid_argument("digest mismatch"));
        }
        let len = data.len() as i64;
        self.0.cas.lock().unwrap().insert(d.hash, data);
        let mut stats = self.0.stats.lock().unwrap();
        stats.bytestream_writes += 1;
        stats.bytes_uploaded += len as u64;
        Ok(Response::new(bs::WriteResponse { committed_size: len }))
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
                let d = self.0.put(data);
                result.output_files.push(re::OutputFile { path: o.clone(), digest: Some(d), ..Default::default() });
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
        let op = Operation {
            name: format!("operations/{}", d.hash),
            metadata: None,
            done: true,
            result: Some(operation::Result::Response(Any {
                type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".into(),
                value: resp.encode_to_vec(),
            })),
        };
        Ok(Response::new(Box::pin(tokio_stream::iter(vec![Ok(op)]))))
    }

    type WaitExecutionStream = BoxStream<Operation>;
    async fn wait_execution(&self, _r: Request<re::WaitExecutionRequest>) -> Result<Response<Self::WaitExecutionStream>, Status> {
        Err(Status::unimplemented("WaitExecution"))
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
                let ack = bes::PublishBuildToolEventStreamResponse {
                    stream_id: ordered.stream_id.clone(),
                    sequence_number: ordered.sequence_number,
                };
                if tx.send(Ok(ack)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }
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
        std::fs::create_dir_all(work_base)?;
        let state = Arc::new(State { base: work_base.to_path_buf(), ..Default::default() });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let addr = listener.local_addr()?;
        let svc = Svc(state.clone());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
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
        Ok(Self { endpoint: format!("grpc://{addr}"), state, shutdown: Some(tx) })
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
