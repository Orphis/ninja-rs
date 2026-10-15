//! Streams build events to a Build Event Service, as Bazel does with
//! `--bes_backend`.
//!
//! - **Lifecycle events** (optional, like `--bes_lifecycle_events`):
//!   `BuildEnqueued` and `InvocationAttemptStarted` before the stream, as
//!   Bazel sends them (within `START_TIMEOUT`: a slow lifecycle service
//!   doesn't hold the events back long), `InvocationAttemptFinished` and
//!   `BuildFinished` once it went through, through `PublishLifecycleEvent`.
//!   They don't fail the upload: what failed is in
//!   `UploadSummary::lifecycle_error`.
//! - **Build tool stream:** one `PublishBuildToolEventStream` call carrying
//!   each event as an `OrderedBuildEvent` with consecutive sequence numbers,
//!   ending with `ComponentStreamFinished`. The server acks sequence numbers;
//!   events stay buffered until acked and are resent, in order, after a
//!   reconnect.
//!
//! Events are queued without blocking and sent by a background task, which
//! connects first, so a slow or unavailable backend never slows the build
//! down. Every call has a deadline (`BesConfig::rpc_timeout`), and a
//! backend that never answered anything isn't called again (but for a
//! connection that broke, twice): the upload fails instead of waiting for
//! it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use prost::Message;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::transport::Channel;

use crate::proto::build_event_stream as bep;
use crate::proto::google::devtools::build::v1 as bes;
use bes::build_event::{self as be, Event};
use bes::publish_build_event_client::PublishBuildEventClient;
use bes::stream_id::BuildComponent;

/// Type URL of a BEP event packed in `BuildEvent.bazel_event`.
pub const BAZEL_EVENT_TYPE_URL: &str = "type.googleapis.com/build_event_stream.BuildEvent";

#[derive(Clone, Debug)]
pub struct BesConfig {
    /// BES "project"; Bazel's `--bes_instance_name`.
    pub project_id: String,
    /// Bazel's build request id: groups related invocations.
    pub build_id: String,
    pub invocation_id: String,
    /// Sent with the first event of the stream (and lifecycle events).
    pub keywords: Vec<String>,
    pub lifecycle_events: bool,
    /// Consecutive failed attempts (without an ack in between) before giving up.
    pub max_retries: u32,
    /// Delay before the first retry; doubles on each consecutive failure.
    pub initial_backoff: Duration,
    /// Extra gRPC headers on every call (e.g. an API key).
    pub headers: Vec<(String, String)>,
    /// Traces the upload on stderr (stream, acks, lifecycle events).
    pub trace: bool,
    /// How long the backend may take to answer: a lifecycle event, or the
    /// stream's next ack while events wait for one.
    pub rpc_timeout: Duration,
}

impl BesConfig {
    pub fn new(project_id: &str, build_id: &str, invocation_id: &str) -> Self {
        Self {
            project_id: project_id.into(),
            build_id: build_id.into(),
            invocation_id: invocation_id.into(),
            keywords: Vec::new(),
            lifecycle_events: true,
            max_retries: 4,
            initial_backoff: Duration::from_secs(1),
            headers: Vec::new(),
            trace: false,
            rpc_timeout: Duration::from_secs(15),
        }
    }
}

/// How the build ended, for the lifecycle events.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuildOutcome {
    pub success: bool,
    pub exit_code: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UploadSummary {
    /// Build tool events sent, including the final `ComponentStreamFinished`.
    pub events: u64,
    pub reconnects: u32,
    /// Why lifecycle events didn't all go through (the first failure).
    pub lifecycle_error: Option<String>,
}

/// How long the lifecycle events before the stream may take: the stream
/// waits for them.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Calls a backend that never answered anything gets again when the
/// connection broke (reset, refused): one restarting, or a load balancer's
/// hiccup.
const FIRST_CONTACT_RETRIES: u32 = 2;

enum Cmd {
    Event(Box<bep::BuildEvent>, SystemTime),
    Finish(BuildOutcome),
}

pub struct BesUploader {
    tx: mpsc::UnboundedSender<Cmd>,
    task: tokio::task::JoinHandle<Result<UploadSummary>>,
}

impl BesUploader {
    /// Starts the background task on the current Tokio runtime.
    pub fn start(channel: Channel, config: BesConfig) -> Self {
        Self::connecting(async { Ok(channel) }, config)
    }

    /// Starts the background task, which connects first (`connect`):
    /// events are queued meanwhile. A connection that fails ends the
    /// upload, with its error.
    pub fn connecting<F>(connect: F, config: BesConfig) -> Self
    where
        F: std::future::Future<Output = Result<Channel>> + Send + 'static,
    {
        TRACE.store(config.trace, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut rx = rx;
            let headers = metadata(&config.headers)?;
            let channel = connect.await?;
            trace(format_args!("connected"));
            Upload { client: PublishBuildEventClient::new(channel), config, headers, answered: AtomicBool::new(false) }.run(&mut rx).await
        });
        Self { tx, task }
    }

    /// Whether the upload ended (it failed): events sent go nowhere.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Queues a BEP event. Never blocks; a no-op once the upload has failed.
    pub fn send(&self, event: bep::BuildEvent) {
        let _ = self.tx.send(Cmd::Event(Box::new(event), SystemTime::now()));
    }

    /// Ends the stream and waits until every event is acked and the
    /// lifecycle events are sent.
    pub async fn finish(self, outcome: BuildOutcome) -> Result<UploadSummary> {
        let _ = self.tx.send(Cmd::Finish(outcome));
        drop(self.tx);
        self.task.await.context("BES upload task panicked")?
    }
}

fn timestamp(t: SystemTime) -> prost_types::Timestamp {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    prost_types::Timestamp { seconds: d.as_secs() as i64, nanos: d.subsec_nanos() as i32 }
}

fn stream_id(config: &BesConfig, component: BuildComponent, invocation: bool) -> bes::StreamId {
    bes::StreamId {
        build_id: config.build_id.clone(),
        invocation_id: if invocation { config.invocation_id.clone() } else { String::new() },
        component: component as i32,
    }
}

/// The `PublishLifecycleEvent` requests, in order, before and after the stream.
pub fn lifecycle_requests(
    config: &BesConfig,
    outcome: Option<BuildOutcome>,
    at: SystemTime,
) -> Vec<bes::PublishLifecycleEventRequest> {
    let req = |invocation: bool, seq: i64, event: Event| bes::PublishLifecycleEventRequest {
        service_level: bes::publish_lifecycle_event_request::ServiceLevel::Interactive as i32,
        build_event: Some(bes::OrderedBuildEvent {
            stream_id: Some(stream_id(config, BuildComponent::Controller, invocation)),
            sequence_number: seq,
            event: Some(bes::BuildEvent { event_time: Some(timestamp(at)), event: Some(event) }),
        }),
        stream_timeout: None,
        notification_keywords: config.keywords.clone(),
        project_id: config.project_id.clone(),
        check_preceding_lifecycle_events_present: false,
    };
    match outcome {
        None => vec![
            req(false, 1, Event::BuildEnqueued(be::BuildEnqueued { details: None })),
            req(true, 1, Event::InvocationAttemptStarted(be::InvocationAttemptStarted { attempt_number: 1, details: None })),
        ],
        Some(o) => {
            let status = bes::BuildStatus {
                result: if o.success {
                    bes::build_status::Result::CommandSucceeded
                } else {
                    bes::build_status::Result::CommandFailed
                } as i32,
                final_invocation_id: config.invocation_id.clone(),
                build_tool_exit_code: Some(o.exit_code),
                error_message: String::new(),
                details: None,
            };
            vec![
                req(true, 2, Event::InvocationAttemptFinished(be::InvocationAttemptFinished {
                    invocation_status: Some(status.clone()),
                    details: None,
                })),
                req(false, 2, Event::BuildFinished(be::BuildFinished { status: Some(status), details: None })),
            ]
        }
    }
}

/// One build tool stream request. Keywords go with the first event only,
/// as Bazel does. The lifecycle events before the stream may have failed
/// (see `Upload::run`): the server mustn't require them first.
pub fn tool_request(config: &BesConfig, seq: i64, event: Event, at: SystemTime) -> bes::PublishBuildToolEventStreamRequest {
    bes::PublishBuildToolEventStreamRequest {
        ordered_build_event: Some(bes::OrderedBuildEvent {
            stream_id: Some(stream_id(config, BuildComponent::Tool, true)),
            sequence_number: seq,
            event: Some(bes::BuildEvent { event_time: Some(timestamp(at)), event: Some(event) }),
        }),
        notification_keywords: if seq == 1 { config.keywords.clone() } else { Vec::new() },
        project_id: config.project_id.clone(),
        check_preceding_lifecycle_events_present: false,
    }
}

pub fn pack(event: &bep::BuildEvent) -> Event {
    Event::BazelEvent(prost_types::Any { type_url: BAZEL_EVENT_TYPE_URL.into(), value: event.encode_to_vec() })
}

/// Extra gRPC headers as metadata.
type Headers = Vec<(tonic::metadata::AsciiMetadataKey, tonic::metadata::AsciiMetadataValue)>;

/// `BesConfig::headers` as metadata: a name or value that isn't valid
/// fails the upload, as an invalid `--remote_header` fails remote calls.
fn metadata(headers: &[(String, String)]) -> Result<Headers> {
    headers
        .iter()
        .map(|(k, v)| {
            let key = tonic::metadata::AsciiMetadataKey::from_bytes(k.to_ascii_lowercase().as_bytes()).with_context(|| format!("invalid header name `{k}`"))?;
            let value = v.parse().with_context(|| format!("invalid value for header `{k}`"))?;
            Ok((key, value))
        })
        .collect()
}

/// What a call left unanswered fails with (see `no_answer`): its status's
/// source, which tells it apart from a broken connection's.
#[derive(Debug)]
struct NoAnswer;

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no answer")
    }
}

impl std::error::Error for NoAnswer {}

/// Whether `s` stands for a connection that broke (reset, refused, closed:
/// it carries the transport's error), not for an answer, a call left
/// unanswered or a connection attempt that timed out.
fn broken_connection(s: &tonic::Status) -> bool {
    let mut cause = std::error::Error::source(s);
    if cause.is_none() {
        return false;
    }
    while let Some(e) = cause {
        let text = e.to_string().to_ascii_lowercase();
        if e.is::<NoAnswer>() || text.contains("timed out") || text.contains("timeout") || text.contains("certificate") {
            return false;
        }
        cause = e.source();
    }
    true
}

/// Notes that the backend answered, when `status` is its own: one standing
/// for a broken connection has the transport's error as its source, one
/// for a call left unanswered `NoAnswer`. Until the backend answered
/// something, failures aren't retried (but for a connection that broke): it
/// is gone, or isn't a BES.
fn note_answer(answered: &AtomicBool, status: &tonic::Status) {
    if std::error::Error::source(status).is_none() {
        answered.store(true, Ordering::Relaxed);
    }
}

fn retryable(s: &tonic::Status) -> bool {
    !matches!(
        s.code(),
        tonic::Code::InvalidArgument
            | tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
            | tonic::Code::NotFound
            | tonic::Code::AlreadyExists
            | tonic::Code::FailedPrecondition
            | tonic::Code::Unimplemented
    )
}

/// The background task: the backend's client, and what its calls share.
struct Upload {
    client: PublishBuildEventClient<Channel>,
    config: BesConfig,
    headers: Headers,
    /// Whether the backend answered anything yet (see `note_answer`).
    answered: AtomicBool,
}

impl Upload {
    async fn run(&self, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<UploadSummary> {
        let config = &self.config;
        let mut lifecycle_error = None;
        if config.lifecycle_events {
            // Before the stream, as Bazel sends them, but within
            // START_TIMEOUT and without retries: the build's events don't
            // wait long for a slow or failing lifecycle service.
            let requests = lifecycle_requests(config, None, SystemTime::now());
            if let Err(e) = self.publish_lifecycle(requests, 0, config.rpc_timeout.min(START_TIMEOUT)).await {
                trace(format_args!("{e:#}"));
                lifecycle_error = Some(format!("{e:#}"));
            }
        }
        let (mut summary, outcome) = self.stream_events(rx).await?;
        if config.lifecycle_events {
            // Whenever the stream went through: a lifecycle UI closes the
            // invocation, whatever became of the events before it.
            let requests = lifecycle_requests(config, Some(outcome), SystemTime::now());
            if let Err(e) = self.publish_lifecycle(requests, config.max_retries, config.rpc_timeout).await {
                trace(format_args!("{e:#}"));
                lifecycle_error.get_or_insert(format!("{e:#}"));
            }
        }
        summary.lifecycle_error = lifecycle_error;
        Ok(summary)
    }

    /// Makes a call until it succeeds, fails for good, or failed `retries`
    /// times. Each call may take `deadline`.
    async fn with_retries<T, F, Fut>(&self, what: &str, retries: u32, deadline: Duration, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, tonic::Status>>,
    {
        let mut backoff = self.config.initial_backoff;
        let mut attempt = 0;
        loop {
            let e = match tokio::time::timeout(deadline, f()).await {
                Ok(Ok(v)) => {
                    self.answered.store(true, Ordering::Relaxed);
                    return Ok(v);
                }
                Ok(Err(e)) => {
                    note_answer(&self.answered, &e);
                    e
                }
                Err(_) => no_answer(deadline),
            };
            if attempt >= self.budget(&e, retries) || !retryable(&e) {
                bail!("{what}: {e}");
            }
            attempt += 1;
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
    }

    /// How many times a call failing with `e` is made again, of `retries`:
    /// none until the backend answered something, but twice for a
    /// connection that broke (a call left unanswered would cost a deadline
    /// each time).
    fn budget(&self, e: &tonic::Status, retries: u32) -> u32 {
        if self.answered.load(Ordering::Relaxed) {
            retries
        } else if broken_connection(e) {
            FIRST_CONTACT_RETRIES.min(self.config.max_retries)
        } else {
            0
        }
    }

    /// Sends lifecycle events, in order, until one fails.
    async fn publish_lifecycle(&self, requests: Vec<bes::PublishLifecycleEventRequest>, retries: u32, deadline: Duration) -> Result<()> {
        for r in requests {
            let what = r.build_event.as_ref().and_then(|e| e.event.as_ref()).and_then(|e| e.event.as_ref()).map(|e| match e {
                Event::BuildEnqueued(_) => "BuildEnqueued",
                Event::InvocationAttemptStarted(_) => "InvocationAttemptStarted",
                Event::InvocationAttemptFinished(_) => "InvocationAttemptFinished",
                Event::BuildFinished(_) => "BuildFinished",
                _ => "other",
            });
            self.with_retries(&format!("BES lifecycle event {}", what.unwrap_or_default()), retries, deadline, || {
                let (r, mut c) = (r.clone(), self.client.clone());
                async move { c.publish_lifecycle_event(self.request(r)).await }
            })
            .await?;
            trace(format_args!("lifecycle {} sent", what.unwrap_or_default()));
        }
        Ok(())
    }

    /// Wraps a message (or a request stream) with the configured headers.
    fn request<T>(&self, message: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(message);
        for (k, v) in &self.headers {
            r.metadata_mut().insert(k.clone(), v.clone());
        }
        r
    }

    /// Sends every queued event through `PublishBuildToolEventStream` until
    /// the producer finishes and everything is acked, reconnecting as
    /// needed (also when events wait for an ack longer than `rpc_timeout`).
    async fn stream_events(&self, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<(UploadSummary, BuildOutcome)> {
        let config = &self.config;
        let answered = &self.answered;
        let mut client = self.client.clone();
        let mut pending: VecDeque<bes::PublishBuildToolEventStreamRequest> = VecDeque::new();
        let mut next_seq = 1i64;
        let mut outcome: Option<BuildOutcome> = None;
        let mut summary = UploadSummary::default();
        let mut failures = 0u32;
        let mut backoff = config.initial_backoff;
        // The producer went away without `finish`: end the stream as failed.
        let abandoned = || Cmd::Finish(BuildOutcome { success: false, exit_code: 1 });

        // Open the stream once there's an event: some servers only answer then.
        let first = rx.recv().await.unwrap_or_else(abandoned);
        accept(first, config, &mut pending, &mut next_seq, &mut outcome, &mut summary);
        loop {
            // Everything not yet acked is resent, in order, on a new stream.
            let (tx, req_rx) = mpsc::unbounded_channel();
            let mut tx = Some(tx);
            let mut sent_upto = 0i64;
            trace(format_args!("opening the stream ({} events pending)", pending.len()));
            let call = client.publish_build_tool_event_stream(self.request(UnboundedReceiverStream::new(req_rx)));
            tokio::pin!(call);
            let mut responses: Option<tonic::Streaming<bes::PublishBuildToolEventStreamResponse>> = None;
            // Since when events have waited for the backend's next answer.
            let mut waiting = tokio::time::Instant::now();
            let err = loop {
                if let Some(t) = &tx {
                    let start = sent_upto;
                    for r in pending.iter().filter(|r| seq_of(r) > start) {
                        let _ = t.send(r.clone());
                        sent_upto = seq_of(r);
                    }
                    // The final event is queued once there's an outcome: half-close.
                    if outcome.is_some() {
                        trace(format_args!("half-closing after event {sent_upto}"));
                        tx = None;
                    }
                }
                tokio::select! {
                    r = &mut call, if responses.is_none() => match r {
                        Ok(resp) => {
                            trace(format_args!("stream open"));
                            answered.store(true, Ordering::Relaxed);
                            waiting = tokio::time::Instant::now();
                            responses = Some(resp.into_inner());
                        }
                        Err(e) => {
                            note_answer(answered, &e);
                            break e;
                        }
                    },
                    cmd = rx.recv(), if outcome.is_none() => {
                        if pending.is_empty() {
                            waiting = tokio::time::Instant::now();
                        }
                        accept(cmd.unwrap_or_else(abandoned), config, &mut pending, &mut next_seq, &mut outcome, &mut summary);
                    }
                    ack = next_ack(&mut responses), if responses.is_some() => match ack {
                        Some(Ok(ack)) => {
                            answered.store(true, Ordering::Relaxed);
                            waiting = tokio::time::Instant::now();
                            let before = pending.len();
                            while pending.front().is_some_and(|r| seq_of(r) <= ack.sequence_number) {
                                pending.pop_front();
                            }
                            if pending.len() < before {
                                failures = 0;
                                backoff = config.initial_backoff;
                            }
                            if let (Some(o), true) = (outcome, pending.is_empty()) {
                                trace(format_args!("all {} events acked", ack.sequence_number));
                                return Ok((summary, o));
                            }
                        }
                        Some(Err(e)) => {
                            note_answer(answered, &e);
                            break e;
                        }
                        None => match (outcome, pending.is_empty()) {
                            (Some(o), true) => {
                                trace(format_args!("stream closed by the server, all acked"));
                                return Ok((summary, o));
                            }
                            _ => break tonic::Status::unavailable("stream closed before every event was acked"),
                        },
                    },
                    () = tokio::time::sleep_until(waiting + config.rpc_timeout), if !pending.is_empty() => break no_answer(config.rpc_timeout),
                }
            };
            failures += 1;
            trace(format_args!("stream failed ({} pending): {err}", pending.len()));
            if failures > self.budget(&err, config.max_retries) || !retryable(&err) {
                let never = if answered.load(Ordering::Relaxed) { "" } else { " (the backend never answered)" };
                bail!("BES upload failed after {failures} attempts{never}: {err}");
            }
            summary.reconnects += 1;
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
    }
}

/// A call the backend didn't answer within `deadline`.
fn no_answer(deadline: Duration) -> tonic::Status {
    let mut s = tonic::Status::deadline_exceeded(format!("no answer within {deadline:?}"));
    s.set_source(std::sync::Arc::new(NoAnswer));
    s
}

/// Whether `trace` prints: `BesConfig::trace` of the last uploader started.
static TRACE: AtomicBool = AtomicBool::new(false);

/// Traces the upload on stderr, with `BesConfig::trace`.
fn trace(what: std::fmt::Arguments<'_>) {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    if TRACE.load(Ordering::Relaxed) {
        eprintln!("[bes {:>8.3}s] {what}", start.elapsed().as_secs_f64());
    }
}

fn seq_of(r: &bes::PublishBuildToolEventStreamRequest) -> i64 {
    r.ordered_build_event.as_ref().map_or(0, |e| e.sequence_number)
}

async fn next_ack(
    responses: &mut Option<tonic::Streaming<bes::PublishBuildToolEventStreamResponse>>,
) -> Option<Result<bes::PublishBuildToolEventStreamResponse, tonic::Status>> {
    responses.as_mut()?.next().await
}

fn accept(
    cmd: Cmd,
    config: &BesConfig,
    pending: &mut VecDeque<bes::PublishBuildToolEventStreamRequest>,
    next_seq: &mut i64,
    outcome: &mut Option<BuildOutcome>,
    summary: &mut UploadSummary,
) {
    let (event, at) = match cmd {
        Cmd::Event(e, at) => (pack(&e), at),
        Cmd::Finish(o) => {
            *outcome = Some(o);
            (
                Event::ComponentStreamFinished(be::BuildComponentStreamFinished {
                    r#type: be::build_component_stream_finished::FinishType::Finished as i32,
                }),
                SystemTime::now(),
            )
        }
    };
    pending.push_back(tool_request(config, *next_seq, event, at));
    *next_seq += 1;
    summary.events += 1;
}
