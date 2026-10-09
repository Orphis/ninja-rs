//! Streams build events to a Build Event Service, as Bazel does with
//! `--bes_backend`.
//!
//! - **Lifecycle events** (optional, like `--bes_lifecycle_events`):
//!   `BuildEnqueued` and `InvocationAttemptStarted` before the stream,
//!   `InvocationAttemptFinished` and `BuildFinished` after it, through
//!   `PublishLifecycleEvent`.
//! - **Build tool stream:** one `PublishBuildToolEventStream` call carrying
//!   each event as an `OrderedBuildEvent` with consecutive sequence numbers,
//!   ending with `ComponentStreamFinished`. The server acks sequence numbers;
//!   events stay buffered until acked and are resent, in order, after a
//!   reconnect.
//!
//! Events are queued without blocking and sent by a background task, so a
//! slow or unavailable backend never slows the build down.

use std::collections::VecDeque;
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
}

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
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(PublishBuildEventClient::new(channel), config, rx));
        Self { tx, task }
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

/// One build tool stream request. Keywords and the lifecycle check go with
/// the first event only, as Bazel does.
pub fn tool_request(config: &BesConfig, seq: i64, event: Event, at: SystemTime) -> bes::PublishBuildToolEventStreamRequest {
    bes::PublishBuildToolEventStreamRequest {
        ordered_build_event: Some(bes::OrderedBuildEvent {
            stream_id: Some(stream_id(config, BuildComponent::Tool, true)),
            sequence_number: seq,
            event: Some(bes::BuildEvent { event_time: Some(timestamp(at)), event: Some(event) }),
        }),
        notification_keywords: if seq == 1 { config.keywords.clone() } else { Vec::new() },
        project_id: config.project_id.clone(),
        check_preceding_lifecycle_events_present: seq == 1 && config.lifecycle_events,
    }
}

pub fn pack(event: &bep::BuildEvent) -> Event {
    Event::BazelEvent(prost_types::Any { type_url: BAZEL_EVENT_TYPE_URL.into(), value: event.encode_to_vec() })
}

async fn with_retries<T, F, Fut>(config: &BesConfig, what: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, tonic::Status>>,
{
    let mut backoff = config.initial_backoff;
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < config.max_retries && retryable(&e) => {
                attempt += 1;
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
            Err(e) => bail!("{what}: {e}"),
        }
    }
}

/// Wraps a message (or a request stream) with the configured headers.
/// Headers that aren't valid gRPC metadata are left out.
fn request<T>(config: &BesConfig, message: T) -> tonic::Request<T> {
    let mut r = tonic::Request::new(message);
    for (k, v) in &config.headers {
        if let (Ok(k), Ok(v)) = (
            tonic::metadata::AsciiMetadataKey::from_bytes(k.to_ascii_lowercase().as_bytes()),
            tonic::metadata::AsciiMetadataValue::try_from(v.as_str()),
        ) {
            r.metadata_mut().insert(k, v);
        }
    }
    r
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

async fn run(
    mut client: PublishBuildEventClient<Channel>,
    config: BesConfig,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) -> Result<UploadSummary> {
    if config.lifecycle_events {
        publish_lifecycle(&client, &config, lifecycle_requests(&config, None, SystemTime::now())).await?;
    }
    let (summary, outcome) = stream_events(&mut client, &config, &mut rx).await?;
    if config.lifecycle_events {
        publish_lifecycle(&client, &config, lifecycle_requests(&config, Some(outcome), SystemTime::now())).await?;
    }
    Ok(summary)
}

async fn publish_lifecycle(
    client: &PublishBuildEventClient<Channel>,
    config: &BesConfig,
    requests: Vec<bes::PublishLifecycleEventRequest>,
) -> Result<()> {
    for r in requests {
        with_retries(config, "BES lifecycle event", || {
            let (r, mut c) = (r.clone(), client.clone());
            async move { c.publish_lifecycle_event(request(config, r)).await }
        })
        .await?;
    }
    Ok(())
}

fn seq_of(r: &bes::PublishBuildToolEventStreamRequest) -> i64 {
    r.ordered_build_event.as_ref().map_or(0, |e| e.sequence_number)
}

/// Sends every queued event through `PublishBuildToolEventStream` until the
/// producer finishes and everything is acked, reconnecting as needed.
async fn stream_events(
    client: &mut PublishBuildEventClient<Channel>,
    config: &BesConfig,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
) -> Result<(UploadSummary, BuildOutcome)> {
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
        let call = client.publish_build_tool_event_stream(request(config, UnboundedReceiverStream::new(req_rx)));
        tokio::pin!(call);
        let mut responses: Option<tonic::Streaming<bes::PublishBuildToolEventStreamResponse>> = None;
        let err = loop {
            if let Some(t) = &tx {
                let start = sent_upto;
                for r in pending.iter().filter(|r| seq_of(r) > start) {
                    let _ = t.send(r.clone());
                    sent_upto = seq_of(r);
                }
                // The final event is queued once there's an outcome: half-close.
                if outcome.is_some() {
                    tx = None;
                }
            }
            tokio::select! {
                r = &mut call, if responses.is_none() => match r {
                    Ok(resp) => responses = Some(resp.into_inner()),
                    Err(e) => break e,
                },
                cmd = rx.recv(), if outcome.is_none() => {
                    accept(cmd.unwrap_or_else(abandoned), config, &mut pending, &mut next_seq, &mut outcome, &mut summary);
                }
                ack = next_ack(&mut responses), if responses.is_some() => match ack {
                    Some(Ok(ack)) => {
                        let before = pending.len();
                        while pending.front().is_some_and(|r| seq_of(r) <= ack.sequence_number) {
                            pending.pop_front();
                        }
                        if pending.len() < before {
                            failures = 0;
                            backoff = config.initial_backoff;
                        }
                        if let (Some(o), true) = (outcome, pending.is_empty()) {
                            return Ok((summary, o));
                        }
                    }
                    Some(Err(e)) => break e,
                    None => match (outcome, pending.is_empty()) {
                        (Some(o), true) => return Ok((summary, o)),
                        _ => break tonic::Status::unavailable("stream closed before every event was acked"),
                    },
                },
            }
        };
        failures += 1;
        if failures > config.max_retries || !retryable(&err) {
            bail!("BES upload failed after {failures} attempts: {err}");
        }
        summary.reconnects += 1;
        tokio::time::sleep(backoff).await;
        backoff *= 2;
    }
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
