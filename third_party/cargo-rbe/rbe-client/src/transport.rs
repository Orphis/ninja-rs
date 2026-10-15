//! What the client adds to tonic's transport: TCP connections noting
//! whether the other end ever sent anything (`Connector`, `Link`), and
//! HTTP errors that aren't gRPC's (a proxy's error page) read as the gRPC
//! status they stand for (`HttpErrors`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tonic::body::Body;
use tonic::codegen::{Service, http};
use tonic::transport::Uri;

/// What a client knows of its connection, to judge one that broke before
/// the service answered anything: whether it's plaintext, and whether the
/// other end sent anything on the last connection made (see `unheard`).
pub(crate) struct Link {
    tls: bool,
    heard: AtomicBool,
}

impl Link {
    pub(crate) fn new(tls: bool) -> Arc<Self> {
        Arc::new(Self { tls, heard: AtomicBool::new(false) })
    }

    /// Whether a connection that broke before the service answered was to
    /// no service at all, as through a tunnel whose far end is down or a
    /// load balancer restarting: on plaintext, or before the other end sent
    /// anything. With TLS, one that broke after the server sent something
    /// may be a server refusing the client's certificate (Windows resets
    /// the connection before the client read the alert): not that.
    pub(crate) fn unheard(&self) -> bool {
        !self.tls || !self.heard.load(Ordering::Relaxed)
    }
}

/// Makes a channel's TCP connections (as tonic does, TLS on top), each
/// noting in `link` whether the other end sent anything on it.
#[derive(Clone)]
pub(crate) struct Connector {
    http: HttpConnector,
    link: Arc<Link>,
}

impl Connector {
    pub(crate) fn new(link: Arc<Link>, timeout: Duration) -> Self {
        // tonic's settings for its own (`Endpoint::connect`).
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(timeout));
        Self { http, link }
    }
}

impl Service<Uri> for Connector {
    type Response = TokioIo<Heard>;
    type Error = <HttpConnector as Service<Uri>>::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.http.poll_ready(cx)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let link = self.link.clone();
        link.heard.store(false, Ordering::Relaxed);
        let connecting = self.http.call(uri);
        Box::pin(async move {
            let tcp = connecting.await?.into_inner();
            Ok(TokioIo::new(Heard { tcp, link }))
        })
    }
}

/// A TCP connection noting in its link whether anything came from the
/// other end.
pub(crate) struct Heard {
    tcp: TcpStream,
    link: Arc<Link>,
}

impl AsyncRead for Heard {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let read = Pin::new(&mut self.tcp).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.link.heard.store(true, Ordering::Relaxed);
        }
        read
    }
}

impl AsyncWrite for Heard {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.tcp).poll_write(cx, buf)
    }

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[std::io::IoSlice<'_>]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.tcp).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.tcp.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.tcp).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.tcp).poll_shutdown(cx)
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
