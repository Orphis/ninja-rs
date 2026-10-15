//! The uploader against an in-process fake BES backend.

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
        let break_after = if stream_index == 0 { self.break_first_after } else { None };
        let (tx, rx) = tokio::sync::mpsc::channel(1024);
        let rec = self.rec.clone();
        tokio::spawn(async move {
            let mut n = 0;
            while let Some(Ok(r)) = incoming.next().await {
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
async fn lifecycle_events_hold_the_stream_back_briefly() {
    // A lifecycle service that never answers: the stream goes on after a
    // deadline; the upload went through, without them.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let fake = Fake { lifecycle_gate: Some(gate.clone()), ..Default::default() };
    let mut c = config();
    c.rpc_timeout = Duration::from_millis(300);
    c.max_retries = 1;
    let up = BesUploader::start(serve(fake.clone()).await, c);
    for i in 0..3 {
        up.send(progress(i, "x"));
    }
    let summary = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert!(summary.lifecycle_error.is_some_and(|e| e.contains("BuildEnqueued") && e.contains("no answer")));
    let rec = fake.rec.lock().unwrap();
    assert_eq!(seqs(&rec.streams[0]), [1, 2, 3, 4]);
    assert!(rec.lifecycle.is_empty());
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
async fn the_build_ends_in_lifecycle_events_whenever_the_stream_went_through() {
    // The events before the stream failed: those after it are sent, so that
    // a lifecycle UI doesn't keep the invocation open.
    let fake = Fake { lifecycle_failures: Some((tonic::Code::FailedPrecondition, 1)), ..Default::default() };
    let up = BesUploader::start(serve(fake.clone()).await, config());
    up.send(progress(0, "x"));
    up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap();
    assert_eq!(fake.rec.lock().unwrap().calls, ["failed", "stream", "invocation finished", "build finished"]);
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

#[tokio::test]
async fn invalid_headers_fail_the_upload() {
    let mut c = config();
    c.headers = vec![("x-api-key".into(), "line\nbreak".into())];
    let up = BesUploader::start(serve(Fake::default()).await, c);
    let err = up.finish(BuildOutcome { success: true, exit_code: 0 }).await.unwrap_err();
    assert!(err.to_string().contains("invalid value for header `x-api-key`"), "{err}");
}

/// A backend that accepts connections and never says anything (as a hung
/// one does: the kernel still accepts them).
async fn silent_backend() -> tonic::transport::Channel {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().collect();
        drop(held);
    });
    tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap()
}

#[tokio::test]
async fn a_silent_backend_costs_one_deadline() {
    for lifecycle_events in [true, false] {
        let mut c = config();
        c.lifecycle_events = lifecycle_events;
        c.rpc_timeout = Duration::from_millis(300);
        let up = BesUploader::start(silent_backend().await, c);
        up.send(progress(0, "x"));
        let start = std::time::Instant::now();
        // However long the build waits for the upload (--bes_timeout=0).
        let finished = tokio::time::timeout(Duration::from_secs(10), up.finish(BuildOutcome { success: true, exit_code: 0 })).await;
        let err = finished.expect("the upload waited for the backend forever").unwrap_err();
        assert!(err.to_string().contains("no answer"), "{err}");
        // Not retried: it never answered anything.
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
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
