//! What the client adds to tonic's transport: connections (TCP, with TLS of
//! their own) telling a server that refused the client's certificate from
//! one that isn't there (`Connector`, `Contact`, `Link`), and HTTP errors
//! that aren't gRPC's (a proxy's error page) read as the gRPC status they
//! stand for (`HttpErrors`).
//!
//! A server refusing the client's certificate says so with an alert, but
//! one may close the connection right after it, which Windows then resets
//! before the client read it; with TLS 1.3, that is once the client's
//! handshake is over, at its first call. So what tells is what came before:
//! a connection broken after the server asked for the client's certificate,
//! and before anything came from the service (its HTTP/2 SETTINGS come at
//! once otherwise), was refused for it. One broken after a handshake where
//! no certificate was asked for (a TLS-terminating proxy whose backend is
//! down), or before anything came, is to no service at all.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{ResolvesClientCert, WebPkiServerVerifier, verify_server_cert_signed_by_trust_anchor};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, DigitallySignedStruct, OtherError, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tonic::body::Body;
use tonic::codegen::{Service, http};
use tonic::transport::Uri;

use crate::fault::{self, CaRefused, Certificate, ClientCertificateRefused};

/// What one connection came to, as far as it tells a server refusing the
/// client's certificate from none (see the module's comment): whether the
/// server asked for the client's certificate (TLS), and whether anything
/// came from the service since the handshake.
pub(crate) struct Contact {
    tls: bool,
    asked: AtomicBool,
    answered: AtomicBool,
    /// Whether it broke (an error, or the end).
    broke: AtomicBool,
}

impl Contact {
    fn new(tls: bool) -> Arc<Self> {
        Arc::new(Self { tls, asked: AtomicBool::new(false), answered: AtomicBool::new(false), broke: AtomicBool::new(false) })
    }

    /// Whether the connection, broken now, was refused for the client's
    /// certificate: over TLS, the server asked for it, and nothing came
    /// since.
    fn refused(&self) -> bool {
        self.tls && self.asked.load(Ordering::Relaxed) && !self.answered.load(Ordering::Relaxed)
    }

    /// Whether the connection, broken now, was to no service at all, as
    /// through a tunnel whose far end is down: on plaintext (nothing tells
    /// then), or over TLS before anything came from the service and without
    /// the client's certificate asked for.
    fn unheard(&self) -> bool {
        !self.tls || (!self.asked.load(Ordering::Relaxed) && !self.answered.load(Ordering::Relaxed))
    }
}

/// What a channel's connections came to, the last that said anything (see
/// `crate::connect_watched`): for whoever waits for the first answer, and
/// to judge a call that failed for its connection broken, without saying
/// why (see `Link::refused`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Nothing yet.
    Nothing,
    /// A connection broke before anything came, without the client's
    /// certificate asked for: as one to no service at all would.
    Unheard,
    /// The service sent something over TLS: it took the client's
    /// certificate, if it asked for one.
    Answered,
    /// The server refused the client's certificate: in words (see
    /// `fault::describe`).
    Refused(String),
}

/// What a client knows of its connections, to judge one that broke before
/// the service answered anything (see `unheard`), and tell whoever waits
/// for it what they came to (`Seen`).
pub(crate) struct Link {
    tls: bool,
    /// The connection made last.
    last: Mutex<Arc<Contact>>,
    seen: watch::Sender<Seen>,
}

impl Link {
    pub(crate) fn new(tls: bool) -> Arc<Self> {
        Arc::new(Self { tls, last: Mutex::new(Contact::new(tls)), seen: watch::Sender::new(Seen::Nothing) })
    }

    /// A TLS link whose last connection broke once the server had asked
    /// for the client's certificate, before anything came from it.
    #[cfg(test)]
    pub(crate) fn refusing() -> Arc<Self> {
        let link = Self::new(true);
        let last = link.last.lock().unwrap().clone();
        last.asked.store(true, Ordering::Relaxed);
        judge(io::ErrorKind::ConnectionReset.into(), &last, Some(&link));
        link
    }

    /// Whether it's plaintext.
    pub(crate) fn plaintext(&self) -> bool {
        !self.tls
    }

    /// Whether the connection made last, broken before the service
    /// answered anything, was to no service at all (see `Contact::unheard`).
    pub(crate) fn unheard(&self) -> bool {
        self.last.lock().unwrap().unheard()
    }

    /// Whether the connection made last broke refused for the client's
    /// certificate (see `Contact::refused`): what a call on it failing for
    /// a connection broken was, though its error doesn't say so (h2 gives
    /// a stream opened as the connection went "a broken pipe"). Were the
    /// call on an older connection, the server refuses the certificate now
    /// all the same.
    pub(crate) fn refused(&self) -> bool {
        let last = self.last.lock().unwrap();
        last.broke.load(Ordering::Relaxed) && last.refused()
    }

    pub(crate) fn watch(&self) -> watch::Receiver<Seen> {
        self.seen.subscribe()
    }

    /// Notes what a connection came to: a refusal or an answer, a
    /// connection unheard as long as nothing said more.
    fn saw(&self, now: Seen) {
        self.seen.send_if_modified(|seen| {
            let news = match (&*seen, &now) {
                (Seen::Answered | Seen::Refused(_), Seen::Unheard) => false,
                (was, now) => was != now,
            };
            if news {
                *seen = now.clone();
            }
            news
        });
    }
}

/// Checks the CAs of `ConnectOptions::ca_pem` that verified a server's
/// certificate (DER; several, when cross-signed ones are given), after
/// rustls's own checks: an error refuses the server's certificate. None
/// checks nothing.
#[derive(Clone, Default)]
pub struct CaCheck(pub Option<Arc<CheckCas>>);

/// What `CaCheck` calls.
pub type CheckCas = dyn Fn(&[&[u8]]) -> Result<(), String> + Send + Sync;

impl CaCheck {
    pub fn new(check: impl Fn(&[&[u8]]) -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(check)))
    }
}

impl std::fmt::Debug for CaCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "CaCheck(Some)" } else { "CaCheck(None)" })
    }
}

/// The TLS of a channel's connections: rustls's, with the client's
/// certificate resolved for each connection (to note it was asked for:
/// `Asked`), ALPN `h2`, and the server's certificate checked against the
/// CA given (`ca_pem`, as Bazel's `--tls_certificate`: a server pinned to a
/// private CA can't be impersonated with a publicly issued certificate),
/// or else the public roots.
pub(crate) struct Tls {
    config: Arc<rustls::ClientConfig>,
    key: Option<Arc<CertifiedKey>>,
    name: ServerName<'static>,
}

impl Tls {
    /// TLS checking the server's certificate against `ca_pem` (PEM) or
    /// the public roots, for `name`, sending `identity` (certificate chain
    /// and key, PEM) when asked. What's wrong with them is said in words,
    /// not as a transport error: it isn't a fault of the connection's.
    pub(crate) fn new(ca_pem: Option<&[u8]>, identity: Option<(&[u8], &[u8])>, name: &str, check: CaCheck) -> anyhow::Result<Self> {
        let provider = rustls::crypto::CryptoProvider::get_default().cloned().unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
        let mut roots = RootCertStore::empty();
        let mut alone = Vec::new();
        match ca_pem {
            Some(pem) => {
                let cas = CertificateDer::pem_slice_iter(pem).collect::<Result<Vec<_>, _>>().map_err(|e| anyhow::anyhow!("the CA certificates: {e}"))?;
                roots.add_parsable_certificates(cas.iter().cloned());
                // Each alone too, to tell which verified a server's
                // certificate (see `Verifier`).
                if check.0.is_some() {
                    for ca in cas {
                        let mut one = RootCertStore::empty();
                        if one.add(ca.clone()).is_ok() {
                            alone.push((ca, one));
                        }
                    }
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?;
        let verifier = Verifier { webpki, alone, check, algorithms: provider.signature_verification_algorithms.all };
        let key = match identity {
            Some((cert, key)) => {
                let chain = CertificateDer::pem_slice_iter(cert).collect::<Result<Vec<_>, _>>().map_err(|e| anyhow::anyhow!("the client certificate: {e}"))?;
                let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| anyhow::anyhow!("the client key: {e}"))?;
                // Fails for a key that isn't the certificate's.
                Some(Arc::new(CertifiedKey::from_der(chain, key, &provider)?))
            }
            None => None,
        };
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_cert_resolver(Arc::new(Asked { key: key.clone(), contact: Contact::new(true) }));
        config.alpn_protocols = vec![b"h2".to_vec()];
        let name = ServerName::try_from(name.to_string()).map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
        Ok(Self { config: Arc::new(config), key, name })
    }

    /// The TLS handshake of a connection (`contact`), told to `link`. A
    /// connection broken once the server asked for the client's
    /// certificate (TLS 1.2 servers check it in the handshake) was refused
    /// for it.
    async fn handshake(&self, tcp: TcpStream, contact: &Arc<Contact>, link: Option<&Link>) -> io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
        let mut config = (*self.config).clone();
        config.client_auth_cert_resolver = Arc::new(Asked { key: self.key.clone(), contact: contact.clone() });
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tls = connector.connect(self.name.clone(), tcp).await.map_err(|e| judge(e, contact, link))?;
        if tls.get_ref().1.alpn_protocol() != Some(b"h2") {
            // As tonic says it (see `fault::fault`).
            return Err(io::Error::other("HTTP/2 was not negotiated."));
        }
        Ok(tls)
    }
}

/// `e`, which broke a connection (`contact`), as what it says about it,
/// told to `link`: the server refusing the client's certificate (typed, or
/// as `ClientCertificateRefused`), the CA that verified the server's
/// certificate refused (`CaRefused`, in its own words), or the connection
/// to no service at all.
fn judge(e: io::Error, contact: &Contact, link: Option<&Link>) -> io::Error {
    contact.broke.store(true, Ordering::Relaxed);
    let ca = e.get_ref().and_then(|inner| match inner.downcast_ref::<rustls::Error>() {
        Some(rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(why)))) => why.downcast_ref::<CaRefused>().cloned(),
        _ => None,
    });
    if let Some(ca) = ca {
        return io::Error::new(io::ErrorKind::InvalidData, ca);
    }
    let (e, seen) = match fault::certificate(&e) {
        Some(Certificate::Client) => {
            let seen = Seen::Refused(fault::describe(&e));
            (e, Some(seen))
        }
        // Connecting fails with it.
        Some(Certificate::Server) => (e, None),
        None if contact.refused() => {
            let e = io::Error::new(e.kind(), ClientCertificateRefused(Box::new(e)));
            let seen = Seen::Refused(fault::describe(&e));
            (e, Some(seen))
        }
        None => (e, contact.unheard().then_some(Seen::Unheard)),
    };
    if let (Some(link), Some(seen)) = (link, seen) {
        link.saw(seen);
    }
    e
}

/// Resolves the client's certificate for one connection, noting the server
/// asked for it (as rustls's own resolver, the same certificate whatever
/// the server's hints).
#[derive(Clone)]
struct Asked {
    key: Option<Arc<CertifiedKey>>,
    contact: Arc<Contact>,
}

impl std::fmt::Debug for Asked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Asked").field("key", &self.key.is_some()).finish()
    }
}

impl ResolvesClientCert for Asked {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        self.contact.asked.store(true, Ordering::Relaxed);
        self.key.clone()
    }

    fn has_certs(&self) -> bool {
        self.key.is_some()
    }
}

/// Verifies servers' certificates as rustls does, and then checks the CAs
/// given that verified one (`CaCheck`): those given each alone (`alone`),
/// under which the server's chain verifies.
struct Verifier {
    webpki: Arc<WebPkiServerVerifier>,
    alone: Vec<(CertificateDer<'static>, RootCertStore)>,
    check: CaCheck,
    algorithms: &'static [&'static dyn rustls::pki_types::SignatureVerificationAlgorithm],
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier").field("cas", &self.alone.len()).field("check", &self.check).finish()
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verified = self.webpki.verify_server_cert(end_entity, intermediates, name, ocsp, now)?;
        if let Some(check) = &self.check.0 {
            let cert = ParsedCertificate::try_from(end_entity)?;
            let by: Vec<&[u8]> = self
                .alone
                .iter()
                .filter(|(_, roots)| verify_server_cert_signed_by_trust_anchor(&cert, roots, intermediates, now, self.algorithms).is_ok())
                .map(|(ca, _)| ca.as_ref())
                .collect();
            check(&by).map_err(|why| rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(CaRefused(why))))))?;
        }
        Ok(verified)
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

/// Makes a channel's connections (as tonic does, with TLS of its own when
/// `tls`), each telling `link`, when there is one, what it came to.
#[derive(Clone)]
pub(crate) struct Connector {
    http: HttpConnector,
    tls: Option<Arc<Tls>>,
    link: Option<Arc<Link>>,
}

impl Connector {
    pub(crate) fn new(tls: Option<Arc<Tls>>, link: Option<Arc<Link>>, timeout: Duration) -> Self {
        // tonic's settings for its own (`Endpoint::connect`).
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(timeout));
        Self { http, tls, link }
    }

    /// The same connections, telling `link`.
    pub(crate) fn with_link(&self, link: Option<Arc<Link>>) -> Self {
        Self { link, ..self.clone() }
    }
}

impl Service<Uri> for Connector {
    type Response = TokioIo<Judged>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let contact = Contact::new(self.tls.is_some());
        if let Some(link) = &self.link {
            *link.last.lock().unwrap() = contact.clone();
        }
        let (tls, link) = (self.tls.clone(), self.link.clone());
        let connecting = self.http.call(uri);
        Box::pin(async move {
            let tcp = connecting.await?.into_inner();
            let io = match &tls {
                Some(tls) => Io::Tls(Box::new(tls.handshake(tcp, &contact, link.as_deref()).await?)),
                None => Io::Plain(tcp),
            };
            Ok(TokioIo::new(Judged { io, contact, link }))
        })
    }
}

enum Io {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

/// A connection made, telling its `Contact` whether anything came from the
/// service, and its errors, once it broke, as what they say about it (see
/// `judge`).
pub(crate) struct Judged {
    io: Io,
    contact: Arc<Contact>,
    link: Option<Arc<Link>>,
}

impl Judged {
    fn judge(&self, e: io::Error) -> io::Error {
        judge(e, &self.contact, self.link.as_deref())
    }
}

impl AsyncRead for Judged {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let (before, room) = (buf.filled().len(), buf.remaining() > 0);
        let read = match &mut self.io {
            Io::Plain(tcp) => ready!(Pin::new(tcp).poll_read(cx, buf)),
            Io::Tls(tls) => ready!(Pin::new(tls.as_mut()).poll_read(cx, buf)),
        };
        Poll::Ready(match read {
            Ok(()) if buf.filled().len() > before => {
                self.contact.answered.store(true, Ordering::Relaxed);
                if let (Some(link), true) = (&self.link, self.contact.tls) {
                    link.saw(Seen::Answered);
                }
                Ok(())
            }
            // The end (TLS's close_notify) once the server asked for the
            // client's certificate, and before anything came: refused.
            Ok(()) if room && self.contact.refused() => Err(self.judge(io::ErrorKind::UnexpectedEof.into())),
            Ok(()) => {
                if room {
                    self.contact.broke.store(true, Ordering::Relaxed);
                }
                if let (Some(link), true) = (&self.link, room && self.contact.tls && self.contact.unheard()) {
                    link.saw(Seen::Unheard);
                }
                Ok(())
            }
            Err(e) => Err(self.judge(e)),
        })
    }
}

impl AsyncWrite for Judged {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let written = match &mut self.io {
            Io::Plain(tcp) => ready!(Pin::new(tcp).poll_write(cx, buf)),
            Io::Tls(tls) => ready!(Pin::new(tls.as_mut()).poll_write(cx, buf)),
        };
        Poll::Ready(written.map_err(|e| self.judge(e)))
    }

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        let written = match &mut self.io {
            Io::Plain(tcp) => ready!(Pin::new(tcp).poll_write_vectored(cx, bufs)),
            Io::Tls(tls) => ready!(Pin::new(tls.as_mut()).poll_write_vectored(cx, bufs)),
        };
        Poll::Ready(written.map_err(|e| self.judge(e)))
    }

    fn is_write_vectored(&self) -> bool {
        match &self.io {
            Io::Plain(tcp) => tcp.is_write_vectored(),
            Io::Tls(tls) => tls.is_write_vectored(),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let flushed = match &mut self.io {
            Io::Plain(tcp) => ready!(Pin::new(tcp).poll_flush(cx)),
            Io::Tls(tls) => ready!(Pin::new(tls.as_mut()).poll_flush(cx)),
        };
        Poll::Ready(flushed.map_err(|e| self.judge(e)))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.io {
            Io::Plain(tcp) => Pin::new(tcp).poll_shutdown(cx),
            Io::Tls(tls) => Pin::new(tls.as_mut()).poll_shutdown(cx),
        }
    }
}

/// A gRPC service (a channel) whose HTTP errors that aren't gRPC answers
/// (a proxy without a healthy backend answering 503 with an HTML page) come
/// back as the gRPC status they stand for, as gRPC maps them
/// (UNAVAILABLE for 429, 502, 503 and 504), with no body: tonic would
/// decode the page as a gRPC message, and fail with INTERNAL.
#[derive(Clone, Debug)]
pub struct HttpErrors<S>(pub S);

impl<S> Service<http::Request<Body>> for HttpErrors<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>>,
    S::Future: Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let response = self.0.call(request);
        Box::pin(async move { response.await.map(grpc_answer) })
    }
}

/// `response`, or for an HTTP error that isn't gRPC's, the gRPC status it
/// stands for: the server's answer, with no body.
fn grpc_answer(response: http::Response<Body>) -> http::Response<Body> {
    use tonic::Code::*;
    let grpc = response.headers().get(http::header::CONTENT_TYPE).and_then(|t| t.to_str().ok()).is_some_and(|t| t.starts_with("application/grpc"));
    let status = response.status();
    if status == http::StatusCode::OK || grpc {
        return response;
    }
    // As gRPC's HTTP to gRPC status mapping has them.
    let code = match status.as_u16() {
        400 => Internal,
        401 => Unauthenticated,
        403 => PermissionDenied,
        404 => Unimplemented,
        429 | 502 | 503 | 504 => Unavailable,
        _ => Unknown,
    };
    tonic::Status::new(code, format!("the server answered HTTP {status}, not gRPC")).into_http()
}
