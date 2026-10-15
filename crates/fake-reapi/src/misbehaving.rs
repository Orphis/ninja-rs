//! Servers misbehaving as no REAPI server does, but as what stands in front
//! of one may: a tunnel whose far end is down, a load balancer restarting,
//! a proxy without a healthy backend, a server draining, a TLS-terminating
//! proxy whose backend is down (`Misbehaving::closing_tls`). For tests of
//! what clients make of them.

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
        Self::serving(move |listener, count| serve(listener, how, count))
    }

    /// A TLS server (`tls`'s certificate) closing each connection once its
    /// handshake is done, saying nothing: a TLS-terminating proxy whose
    /// backend is down; with `tls.client_ca_pem`, asking for a client
    /// certificate (taking any, or none), a server refusing it whose alert
    /// was lost (Windows resets the connection before the client reads it).
    pub fn closing_tls(tls: &crate::ServerTls) -> Self {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().expect("TLS versions");
        let builder = match &tls.client_ca_pem {
            Some(ca) => {
                let mut roots = rustls::RootCertStore::empty();
                roots.add_parsable_certificates(CertificateDer::pem_slice_iter(ca).map(|c| c.expect("a PEM CA")));
                let verifier =
                    rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider).allow_unauthenticated().build().expect("a verifier");
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };
        let chain = CertificateDer::pem_slice_iter(&tls.cert_pem).collect::<Result<Vec<_>, _>>().expect("a PEM certificate");
        let mut config = builder.with_single_cert(chain, PrivateKeyDer::from_pem_slice(&tls.key_pem).expect("a PEM key")).expect("a certificate and its key");
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        Self::serving(move |listener, count| async move {
            loop {
                let Some(c) = accept(&listener).await else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(tls) = acceptor.accept(c).await {
                        drop(tls.into_inner().0);
                    }
                });
            }
        })
    }

    /// A server on a port and a runtime of its own, `serve` taking its
    /// connections, and counting what it takes.
    fn serving<F, Fut>(serve: F) -> Self
    where
        F: FnOnce(tokio::net::TcpListener, Arc<AtomicU64>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()>,
    {
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
                    _ = serve(listener, counted) => {}
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

/// The next connection `listener` takes. None when that failed (a
/// connection reset before it was taken, descriptors running out a while):
/// the next may do.
async fn accept(listener: &tokio::net::TcpListener) -> Option<tokio::net::TcpStream> {
    match listener.accept().await {
        Ok((c, _)) => Some(c),
        Err(_) => {
            tokio::time::sleep(Duration::from_millis(10)).await;
            None
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
    loop {
        let Some(c) = accept(&listener).await else { continue };
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
