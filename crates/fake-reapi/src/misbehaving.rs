//! Servers misbehaving as no REAPI server does, but as what stands in front
//! of one may: a tunnel whose far end is down, a load balancer restarting,
//! a proxy without a healthy backend, a server draining. For tests of what
//! clients make of them.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How a `Misbehaving` server misbehaves.
#[derive(Clone, Copy, Debug)]
pub enum Misbehavior {
    /// Closes each connection it accepts at once, resetting it (`reset`)
    /// or not, saying nothing: the far end of a tunnel down.
    Closing { reset: bool },
    /// An HTTP/2 server answering each request with this HTTP status and an
    /// HTML page: a proxy without a healthy backend.
    Page(u16),
    /// An HTTP/2 server closing each connection at once (GOAWAY): a server
    /// draining.
    GoAway,
    /// A TCP proxy to `upstream` resetting the first `bad` connections it
    /// accepts: a load balancer restarting.
    Restarting { upstream: SocketAddr, bad: u64 },
}

/// A misbehaving server, on a runtime of its own, until it's dropped.
pub struct Misbehaving {
    pub addr: SocketAddr,
    /// The connections it accepted (an HTTP/2 server: the requests).
    pub count: Arc<AtomicU64>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Misbehaving {
    pub fn start(how: Misbehavior) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding a port");
        let addr = listener.local_addr().expect("its address");
        listener.set_nonblocking(true).expect("a non-blocking listener");
        let count = Arc::new(AtomicU64::new(0));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let counted = count.clone();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("a listener");
                tokio::select! {
                    _ = serve(listener, how, counted) => {}
                    _ = stopped => {}
                }
            });
        });
        Self { addr, count, stop: Some(stop), thread: Some(thread) }
    }

    /// Its endpoint with `scheme` (`grpc`, `grpcs`).
    pub fn endpoint(&self, scheme: &str) -> String {
        format!("{scheme}://{}", self.addr)
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }
}

impl Drop for Misbehaving {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Closes `c` resetting it: what's unread or unsent is dropped.
fn reset(c: tokio::net::TcpStream) {
    #[allow(deprecated)]
    let _ = c.set_linger(Some(Duration::ZERO));
    drop(c);
}

async fn serve(listener: tokio::net::TcpListener, how: Misbehavior, count: Arc<AtomicU64>) {
    let mut accepted = 0;
    while let Ok((c, _)) = listener.accept().await {
        accepted += 1;
        match how {
            Misbehavior::Closing { reset: true } => {
                count.fetch_add(1, Ordering::SeqCst);
                reset(c);
            }
            Misbehavior::Closing { reset: false } => {
                count.fetch_add(1, Ordering::SeqCst);
                drop(c);
            }
            Misbehavior::Restarting { bad, .. } if accepted <= bad => {
                count.fetch_add(1, Ordering::SeqCst);
                reset(c);
            }
            Misbehavior::Restarting { upstream, .. } => {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut c = c;
                    if let Ok(mut s) = tokio::net::TcpStream::connect(upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut c, &mut s).await;
                    }
                });
            }
            Misbehavior::Page(_) | Misbehavior::GoAway => {
                tokio::spawn(answer(c, how, count.clone()));
            }
        }
    }
}

/// Answers the requests of an HTTP/2 connection as `how` says.
async fn answer(c: tokio::net::TcpStream, how: Misbehavior, count: Arc<AtomicU64>) {
    let Ok(mut conn) = h2::server::handshake(c).await else { return };
    if let Misbehavior::GoAway = how {
        conn.abrupt_shutdown(h2::Reason::NO_ERROR);
    }
    while let Some(Ok((_, mut respond))) = conn.accept().await {
        count.fetch_add(1, Ordering::SeqCst);
        match how {
            Misbehavior::Page(status) => {
                let page = tonic::codegen::http::Response::builder().status(status).header("content-type", "text/html").body(()).expect("a response");
                if let Ok(mut body) = respond.send_response(page, false) {
                    let _ = body.send_data(tonic::codegen::Bytes::from_static(b"<html><body>no healthy upstream</body></html>"), true);
                }
            }
            _ => respond.send_reset(h2::Reason::REFUSED_STREAM),
        }
    }
}
