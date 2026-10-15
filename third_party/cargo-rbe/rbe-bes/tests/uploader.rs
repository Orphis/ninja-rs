//! The uploader against an in-process fake BES backend.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost::Message;
use rbe_bes::proto::build_event_stream as bep;
use rbe_bes::proto::google::devtools::build::v1 as bes;
use rbe_bes::uploader::{BAZEL_EVENT_TYPE_URL, BesConfig, BesUploader, BuildOutcome};
use bes::build_event::Event;
use bes::publish_build_event_server::{PublishBuildEvent, PublishBuildEventServer};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

#[derive(Default)]
struct Recorded {
    lifecycle: Vec<bes::PublishLifecycleEventRequest>,
    /// Requests received, per stream (one entry per `PublishBuildToolEventStream` call).
    streams: Vec<Vec<bes::PublishBuildToolEventStreamRequest>>,
    /// The calls answered, in order: lifecycle events, and streams opened.
    calls: Vec<String>,
    /// Streams closed for being idle (see `Fake::idle_close`).
    idle_closed: usize,
}

#[derive(Clone, Default)]
struct Fake {
    rec: Arc<Mutex<Recorded>>,
    /// On the first stream only: fail after receiving this many events,
    /// acking all but the last one received.
    break_first_after: Option<usize>,
    /// Refuse every stream with this code.
    refuse: Option<tonic::Code>,
    /// Lifecycle events are answered once this has permits.
    lifecycle_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Lifecycle events are answered after this long.
    lifecycle_delay: Duration,
    /// The first lifecycle events, so many of them, fail with this code.
    lifecycle_failures: Option<(tonic::Code, usize)>,
    /// The lifecycle events from the `n`th call on (from 0) are never
    /// answered (`None`), or fail with this code.
    lifecycle_from: Option<(usize, Option<tonic::Code>)>,
    /// Lifecycle calls received, answered or not.
    lifecycle_received: Arc<AtomicUsize>,
    /// Streams are never answered.
    stream_hang: bool,
    /// A stream is closed once it went this long without an event after
    /// its last ack (a proxy closing idle streams).
    idle_close: Option<Duration>,
}

fn kind(e: &Event) -> &'static str {
    match e {
        Event::BuildEnqueued(_) => "enqueued",
        Event::InvocationAttemptStarted(_) => "started",
        Event::InvocationAttemptFinished(_) => "invocation finished",
        Event::BuildFinished(_) => "build finished",
        _ => "other",
    }
}

#[tonic::async_trait]
impl PublishBuildEvent for Fake {
    async fn publish_lifecycle_event(&self, req: Request<bes::PublishLifecycleEventRequest>) -> Result<Response<()>, Status> {
        let n = self.lifecycle_received.fetch_add(1, Ordering::SeqCst);
        match self.lifecycle_from {
            Some((from, None)) if n >= from => std::future::pending().await,
            Some((from, Some(code))) if n >= from => return Err(Status::new(code, "lifecycle events failing")),
            _ => {}
        }
        if let Some(gate) = &self.lifecycle_gate {
            let _permit = gate.acquire().await.unwrap();
        }
        tokio::time::sleep(self.lifecycle_delay).await;
        let mut rec = self.rec.lock().unwrap();
        if let Some((code, n)) = self.lifecycle_failures
            && rec.calls.iter().filter(|c| *c == "failed").count() < n
        {
            rec.calls.push("failed".into());
            return Err(Status::new(code, "no lifecycle events here"));
        }
        let req = req.into_inner();
        let what = kind(req.build_event.as_ref().unwrap().event.as_ref().unwrap().event.as_ref().unwrap());
        rec.calls.push(what.into());
        rec.lifecycle.push(req);
        Ok(Response::new(()))
    }

    type PublishBuildToolEventStreamStream = ReceiverStream<Result<bes::PublishBuildToolEventStreamResponse, Status>>;

    async fn publish_build_tool_event_stream(
        &self,
        req: Request<Streaming<bes::PublishBuildToolEventStreamRequest>>,
    ) -> Result<Response<Self::PublishBuildToolEventStreamStream>, Status> {
        if let Some(code) = self.refuse {
            return Err(Status::new(code, "refused"));
        }
        let mut incoming = req.into_inner();
        let stream_index = {
            let mut rec = self.rec.lock().unwrap();
            rec.calls.push("stream".into());
            rec.streams.push(Vec::new());
            rec.streams.len() - 1
        };
        if self.stream_hang {
            std::future::pending::<()>().await;
        }
        let break_after = if stream_index == 0 { self.break_first_after } else { None };
        let (tx, rx) = tokio::sync::mpsc::channel(1024);
        let rec = self.rec.clone();
        let idle_close = self.idle_close;
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                let next = match (idle_close, n) {
                    (Some(idle), 1..) => match tokio::time::timeout(idle, incoming.next()).await {
                        Ok(next) => next,
                        Err(_) => {
                            rec.lock().unwrap().idle_closed += 1;
                            let _ = tx.send(Err(Status::unavailable("idle stream closed"))).await;
                            return;
                        }
                    },
                    _ => incoming.next().await,
                };
                let Some(Ok(r)) = next else { return };
                n += 1;
                rec.lock().unwrap().streams[stream_index].push(r.clone());
                if break_after == Some(n) {
                    let _ = tx.send(Err(Status::unavailable("simulated disconnect"))).await;
                    return;
                }
                let e = r.ordered_build_event.unwrap();
                let ack = bes::PublishBuildToolEventStreamResponse { stream_id: e.stream_id, sequence_number: e.sequence_number };
                if tx.send(Ok(ack)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn serve(fake: Fake) -> tonic::transport::Channel {
    let addr = serve_at(fake).await;
    tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap()
}

async fn serve_at(fake: Fake) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(PublishBuildEventServer::new(fake))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    addr
}

fn config() -> BesConfig {
    let mut c = BesConfig::new("proj", "build-1", "inv-1");
    c.keywords = vec!["command_name=build".into()];
    c.initial_backoff = Duration::from_millis(10);
    c
}

fn progress(n: i32, text: &str) -> bep::BuildEvent {
    bep::BuildEvent {
        id: Some(bep::BuildEventId {
            id: Some(bep::build_event_id::Id::Progress(bep::build_event_id::ProgressId { opaque_count: n })),
        }),
        payload: Some(bep::build_event::Payload::Progress(bep::Progress { stdout: String::new(), stderr: text.into() })),
        ..Default::default()
    }
}

fn seqs(stream: &[bes::PublishBuildToolEventStreamRequest]) -> Vec<i64> {
    stream.iter().map(|r| r.ordered_build_event.as_ref().unwrap().sequence_number).collect()
}

fn event(r: &bes::PublishBuildToolEventStreamRequest) -> &Event {
    r.ordered_build_event.as_ref().unwrap().event.as_ref().unwrap().event.as_ref().unwrap()
}

#[tokio::test]
async fn streams_events_in_order_between_lifecycle_events() {
    let fake = Fake::default();
    let up = BesUploader::start(serve(fake.clone()).await, config());
    for i in 0..3 {
        up.send(progress(i, &format!("line {i}")));
    }
    let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert_eq!(summary.events, 4);
    assert_eq!(summary.reconnects, 0);

    let rec = fake.rec.lock().unwrap();
    assert_eq!(rec.streams.len(), 1);
    let s = &rec.streams[0];
    assert_eq!(seqs(s), vec![1, 2, 3, 4]);
    for (i, r) in s.iter().enumerate() {
        assert_eq!(r.project_id, "proj");
        let id = r.ordered_build_event.as_ref().unwrap().stream_id.as_ref().unwrap();
        assert_eq!((id.build_id.as_str(), id.invocation_id.as_str()), ("build-1", "inv-1"));
        assert_eq!(id.component, bes::stream_id::BuildComponent::Tool as i32);
        // Keywords only on the first event. The lifecycle events before the
        // stream may have failed: the server mustn't check for them.
        assert_eq!(r.notification_keywords.is_empty(), i > 0);
        assert!(!r.check_preceding_lifecycle_events_present);
    }
    for (i, r) in s[..3].iter().enumerate() {
        let Event::BazelEvent(any) = event(r) else { panic!("not a BEP event") };
        assert_eq!(any.type_url, BAZEL_EVENT_TYPE_URL);
        assert_eq!(bep::BuildEvent::decode(any.value.as_slice()).unwrap(), progress(i as i32, &format!("line {i}")));
    }
    assert!(matches!(event(&s[3]), Event::ComponentStreamFinished(f) if f.r#type == 1));

    let kinds: Vec<(&str, i64)> = rec
        .lifecycle
        .iter()
        .map(|r| {
            let e = r.build_event.as_ref().unwrap();
            (kind(e.event.as_ref().unwrap().event.as_ref().unwrap()), e.sequence_number)
        })
        .collect();
    assert_eq!(kinds, vec![("enqueued", 1), ("started", 1), ("invocation finished", 2), ("build finished", 2)]);
    // Build-level events aren't tied to the invocation.
    assert!(rec.lifecycle[0].build_event.as_ref().unwrap().stream_id.as_ref().unwrap().invocation_id.is_empty());
    let Event::BuildFinished(f) = rec.lifecycle[3].build_event.as_ref().unwrap().event.as_ref().unwrap().event.as_ref().unwrap() else {
        unreachable!()
    };
    let status = f.status.as_ref().unwrap();
    assert_eq!(status.result, bes::build_status::Result::CommandSucceeded as i32);
    assert_eq!(status.build_tool_exit_code, Some(0));
}

#[tokio::test]
async fn resends_unacked_events_after_a_disconnect() {
    let fake = Fake { break_first_after: Some(3), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    for i in 0..5 {
        up.send(progress(i, "x"));
    }
    let summary = up.finish(BuildOutcome { success: false, exit_code: 1 }).await.unwrap();
    assert_eq!(summary.reconnects, 1);

    let rec = fake.rec.lock().unwrap();
    assert_eq!(rec.streams.len(), 2);
    // Events 1 and 2 were acked; 3 wasn't, so the second stream starts there.
    assert_eq!(seqs(&rec.streams[0]), vec![1, 2, 3]);
    assert_eq!(seqs(&rec.streams[1]), vec![3, 4, 5, 6]);
    let Event::BuildFinished(f) = rec.lifecycle[3].build_event.as_ref().unwrap().event.as_ref().unwrap().event.as_ref().unwrap() else {
        unreachable!()
    };
    assert_eq!(f.status.as_ref().unwrap().result, bes::build_status::Result::CommandFailed as i32);
}

#[tokio::test]
async fn permanent_errors_are_not_retried() {
    let fake = Fake { refuse: Some(tonic::Code::PermissionDenied), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    up.send(progress(0, "x"));
    let err = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap_err();
    assert!(err.to_string().contains("refused"), "{err}");
    // Lifecycle events before the stream went through; none after it.
    assert_eq!(fake.rec.lock().unwrap().lifecycle.len(), 2);
}

#[tokio::test]
async fn gives_up_after_the_retry_budget() {
    let fake = Fake { refuse: Some(tonic::Code::Unavailable), ..Default::default() };
    let mut c = config();
    c.max_retries = 2;
    let up = BesUploader::start(serve(fake.clone()).await, c);
    let err = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap_err();
    assert!(err.to_string().contains("after 3 attempts"), "{err}");
}

/// A backend that only ever said it can't serve now (a proxy without a
/// healthy backend) gets one more try, not the whole budget: the end of
/// every build would wait for the backoffs. Said without saying "failed"
/// again (the caller says the upload failed).
#[tokio::test]
async fn backends_only_declining_get_one_more_try() {
    for code in [tonic::Code::Unavailable, tonic::Code::ResourceExhausted] {
        let fake = Fake { refuse: Some(code), lifecycle_from: Some((0, Some(code))), ..Default::default() };
        let up = BesUploader::start(serve(fake.clone()).await, config());
        up.send(progress(0, "x"));
        let err = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap_err();
        assert!(err.to_string().starts_with("after 2 attempts: "), "{code:?}: {err}");
        assert_eq!(fake.lifecycle_received.load(Ordering::SeqCst), 2, "{code:?}");
    }
}

#[tokio::test]
async fn without_lifecycle_events_only_the_stream_is_sent() {
    let fake = Fake::default();
    let mut c = config();
    c.lifecycle_events = false;
    let up = BesUploader::start(serve(fake.clone()).await, c);
    up.send(progress(0, "x"));
    up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    let rec = fake.rec.lock().unwrap();
    assert!(rec.lifecycle.is_empty());
    assert!(!rec.streams[0][0].check_preceding_lifecycle_events_present);
}

#[tokio::test]
async fn lifecycle_start_events_go_before_the_stream() {
    // Slow, but within the deadline: the stream waits for them.
    let fake = Fake { lifecycle_delay: Duration::from_millis(200), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    up.send(progress(0, "x"));
    up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert_eq!(fake.rec.lock().unwrap().calls, ["enqueued", "started", "stream", "invocation finished", "build finished"]);
}

#[tokio::test]
async fn unanswered_lifecycle_events_hold_the_stream_back_once() {
    // A lifecycle service that never answers: the stream goes on after one
    // deadline (for both events before it), the upload goes through, and
    // the events after it aren't sent (the service doesn't know of the
    // build), with the retries the stream gets or not.
    let fake = Fake { lifecycle_from: Some((0, None)), ..Default::default() };
    let mut c = config();
    c.rpc_timeout = Duration::from_secs(1);
    let up = BesUploader::start(serve(fake.clone()).await, c);
    for i in 0..3 {
        up.send(progress(i, "x"));
    }
    let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert!(summary.lifecycle_error.as_ref().is_some_and(|e| e.starts_with("BuildEnqueued: ") && e.contains("no answer")), "{:?}", summary.lifecycle_error);
    assert_eq!(seqs(&fake.rec.lock().unwrap().streams[0]), [1, 2, 3, 4]);
    assert_eq!(fake.lifecycle_received.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lifecycle_events_after_the_stream_arent_retried() {
    // Never answered (within one deadline for both), or failing: once.
    for failing in [None, Some(tonic::Code::Unavailable)] {
        let fake = Fake { lifecycle_from: Some((2, failing)), ..Default::default() };
        let mut c = config();
        c.rpc_timeout = Duration::from_secs(2);
        let up = BesUploader::start(serve(fake.clone()).await, c);
        up.send(progress(0, "x"));
        let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
        assert!(summary.lifecycle_error.is_some_and(|e| e.contains("InvocationAttemptFinished")), "{failing:?}");
        assert_eq!(fake.lifecycle_received.load(Ordering::SeqCst), 3, "{failing:?}");
    }
}

#[tokio::test]
async fn the_end_of_an_upload_says_what_it_waits_for() {
    // The lifecycle events after the stream are still going: every build
    // event was acked.
    let fake = Fake { lifecycle_from: Some((2, None)), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    let acked = up.acked();
    up.send(progress(0, "x"));
    assert!(!acked.get());
    let closing = async {
        while fake.lifecycle_received.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        r = up.finish(BuildOutcome { success: true, exit_code: 0 }) => panic!("finished while a lifecycle event hung: {r:?}"),
        () = closing => {}
    }
    assert!(acked.get());
}

#[tokio::test]
async fn failing_lifecycle_events_dont_fail_the_upload() {
    // A BES without lifecycle events: every event is acked all the same.
    let fake = Fake { lifecycle_failures: Some((tonic::Code::Unimplemented, usize::MAX)), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    up.send(progress(0, "x"));
    let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert!(summary.lifecycle_error.is_some_and(|e| e.contains("no lifecycle events here")));
    assert_eq!(seqs(&fake.rec.lock().unwrap().streams[0]), [1, 2]);
}

#[tokio::test]
async fn lifecycle_events_after_the_stream_need_those_before_it() {
    // The events before the stream failed: the service doesn't know of the
    // build, and isn't told it ended.
    let fake = Fake { lifecycle_failures: Some((tonic::Code::FailedPrecondition, 1)), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    up.send(progress(0, "x"));
    let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert!(summary.lifecycle_error.is_some_and(|e| e.contains("BuildEnqueued")));
    assert_eq!(fake.rec.lock().unwrap().calls, ["failed", "stream"]);
}

#[tokio::test]
async fn streams_closed_while_idle_are_opened_again_with_the_next_event() {
    // A proxy closing idle streams, through a build step saying nothing:
    // no stream is opened until there's an event, and closing is no
    // failure.
    let fake = Fake { idle_close: Some(Duration::from_millis(50)), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    for i in 0..6 {
        up.send(progress(i, "x"));
        let closed = async {
            while fake.rec.lock().unwrap().idle_closed <= i as usize {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(30), closed).await.expect("the stream wasn't closed");
        // Silence: a stream opened now would be closed too, and count.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.rec.lock().unwrap().streams.len(), i as usize + 1, "a stream opened without events");
    }
    let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert_eq!(summary.reconnects, 0);
    let rec = fake.rec.lock().unwrap();
    let sent: Vec<i64> = rec.streams.iter().flat_map(|s| seqs(s)).collect();
    assert_eq!(sent, (1..=7).collect::<Vec<i64>>());
}

/// A TCP proxy in front of `upstream` dropping its first connection at
/// the first request (once the HTTP/2 handshake went through): a reset,
/// as a server restarting or a load balancer gives.
async fn resetting_proxy(upstream: std::net::SocketAddr) -> tonic::transport::Channel {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap()
}

#[tokio::test]
async fn connections_broken_before_any_answer_are_made_again() {
    for lifecycle_events in [true, false] {
        let fake = Fake::default();
        let mut c = config();
        c.lifecycle_events = lifecycle_events;
        let up = BesUploader::start(resetting_proxy(serve_at(fake.clone()).await).await, c);
        up.send(progress(0, "x"));
        let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
        assert_eq!(summary.lifecycle_error, None);
        let rec = fake.rec.lock().unwrap();
        assert_eq!(seqs(rec.streams.last().unwrap()), [1, 2]);
        assert_eq!(rec.lifecycle.len(), if lifecycle_events { 4 } else { 0 });
    }
}

/// A connection broken without a word, once the last connection was
/// refused for the client's certificate (as `rbe_client::connect_watched`
/// sees it: h2 may lose the connection's own error): that refusal, not
/// made again.
#[tokio::test]
async fn connections_broken_once_a_certificate_was_refused_are_refusals() {
    let fake = Fake::default();
    let mut c = config();
    c.lifecycle_events = false;
    let (_seeing, seen) = tokio::sync::watch::channel(rbe_client::Seen::Refused("the client's certificate".into()));
    let channel = resetting_proxy(serve_at(fake.clone()).await).await;
    let up = BesUploader::connecting_watched(async { Ok(channel) }, Some(seen), c);
    up.send(progress(0, "x"));
    let err = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap_err();
    assert_eq!(rbe_client::fault::certificate(err.as_ref()), Some(rbe_client::fault::Certificate::Client), "{err:#}");
    assert!(err.to_string().starts_with("after 1 attempt "), "{err}");
}

#[tokio::test]
async fn invalid_headers_fail_the_upload() {
    let mut c = config();
    c.headers = vec![("x-api-key".into(), "line\nbreak".into())];
    let up = BesUploader::start(serve(Fake::default()).await, c);
    let err = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap_err();
    assert!(err.to_string().contains("invalid value for header `x-api-key`"), "{err}");
}

#[tokio::test]
async fn a_silent_backend_costs_one_deadline() {
    for lifecycle_events in [true, false] {
        // A backend taking calls and never answering (as a hung one does).
        let fake = Fake { lifecycle_from: Some((0, None)), stream_hang: true, ..Default::default() };
        let mut c = config();
        c.lifecycle_events = lifecycle_events;
        c.rpc_timeout = Duration::from_millis(300);
        let up = BesUploader::start(serve(fake.clone()).await, c);
        up.send(progress(0, "x"));
        // However long the build waits for the upload (--bes_timeout=0).
        let finished = tokio::time::timeout(Duration::from_secs(60), up.finish(BuildOutcome { success: true, exit_code: 0 })).await;
        let err = finished.expect("the upload waited for the backend forever").unwrap_err();
        assert!(err.to_string().contains("no answer"), "{err}");
        // Not called again: it never answered anything.
        assert_eq!(fake.lifecycle_received.load(Ordering::SeqCst), usize::from(lifecycle_events));
        assert_eq!(fake.rec.lock().unwrap().streams.len(), 1);
    }
}

#[tokio::test]
async fn connects_in_the_background() {
    let fake = Fake::default();
    let channel = serve(fake.clone()).await;
    let connect = async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(channel)
    };
    let up = BesUploader::connecting(connect, config());
    // Events sent before the connection are kept for it.
    up.send(progress(0, "x"));
    up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert_eq!(seqs(&fake.rec.lock().unwrap().streams[0]), vec![1, 2]);

    // A connection that fails ends the upload: events go nowhere.
    let up = BesUploader::connecting(async { anyhow::bail!("connecting to grpc://nowhere: refused") }, config());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !up.is_closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the upload didn't end");
    let err = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap_err();
    assert!(err.to_string().contains("refused"), "{err}");
}
