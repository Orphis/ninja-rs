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
}

#[derive(Clone, Default)]
struct Fake {
    rec: Arc<Mutex<Recorded>>,
    /// On the first stream only: fail after receiving this many events,
    /// acking all but the last one received.
    break_first_after: Option<usize>,
    /// Refuse every stream with this code.
    refuse: Option<tonic::Code>,
}

#[tonic::async_trait]
impl PublishBuildEvent for Fake {
    async fn publish_lifecycle_event(&self, req: Request<bes::PublishLifecycleEventRequest>) -> Result<Response<()>, Status> {
        self.rec.lock().unwrap().lifecycle.push(req.into_inner());
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(PublishBuildEventServer::new(fake))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap()
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
        // Keywords and the lifecycle check only on the first event.
        assert_eq!(r.notification_keywords.is_empty(), i > 0);
        assert_eq!(r.check_preceding_lifecycle_events_present, i == 0);
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
            let kind = match e.event.as_ref().unwrap().event.as_ref().unwrap() {
                Event::BuildEnqueued(_) => "enqueued",
                Event::InvocationAttemptStarted(_) => "started",
                Event::InvocationAttemptFinished(_) => "invocation finished",
                Event::BuildFinished(_) => "build finished",
                _ => "other",
            };
            (kind, e.sequence_number)
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
