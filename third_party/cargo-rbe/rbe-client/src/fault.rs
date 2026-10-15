//! What a failed connection or call says about the service: whether it
//! answered, whether there was a connection at all, and so whether calling
//! again may help, or building without it. Shared by the REAPI client and
//! the build event uploader. Decided from the errors' types and kinds, and
//! from their words only for those of types the transport doesn't export.

use std::error::Error;
use std::sync::Arc;

/// Calls (and connections) a service that never answered anything gets
/// again when the connection broke once made: a server restarting, or a
/// load balancer's hiccup. One there was no connection to, or that left a
/// call unanswered, gets none: each would cost as much again (a refused
/// connection takes Windows 2 s).
pub const FIRST_CONTACT_RETRIES: u32 = 1;

/// What a call left unanswered fails with (see `no_answer`), and a
/// connection that couldn't be made in time: what tells them apart from
/// the server's own DEADLINE_EXCEEDED.
#[derive(Debug)]
pub struct NoAnswer;

impl std::fmt::Display for NoAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no answer")
    }
}

impl Error for NoAnswer {}

/// DEADLINE_EXCEEDED for a call or a stream the server left unanswered.
pub fn no_answer(message: impl Into<String>) -> tonic::Status {
    let mut s = tonic::Status::deadline_exceeded(message);
    s.set_source(Arc::new(NoAnswer));
    s
}

/// Whether `s` is the server's own status: an answer. One standing for a
/// broken connection carries the transport's error as its source, one for
/// a call left unanswered `NoAnswer`.
pub fn from_server(s: &tonic::Status) -> bool {
    s.source().is_none()
}

/// What a failure says about the connection to the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// There was no connection: refused, no route to the host, a name that
    /// doesn't resolve.
    NoConnection,
    /// Nothing answered in time: connecting, the TLS handshake, or a call.
    NoAnswer,
    /// The connection broke once made (reset, closed).
    Broke,
    /// The connection was refused for what it is: a certificate (the
    /// server's, or the client's), or a protocol the other end doesn't
    /// speak (plaintext HTTP/2 to a TLS server).
    Refused,
    /// No fault of the transport's: the server's own answer, or what
    /// failed before connecting (an endpoint's scheme, the TLS settings).
    None,
}

impl Fault {
    /// Whether the service can't be reached: no connection, or no answer.
    /// What builds may do without (`--remote_local_fallback`), unlike a
    /// certificate refused or a server's answer, which another try (or a
    /// local build) doesn't fix.
    pub fn unreachable(self) -> bool {
        matches!(self, Fault::NoConnection | Fault::NoAnswer)
    }
}

/// What `e`, with its causes, says about the connection.
pub fn fault(e: &(dyn Error + 'static)) -> Fault {
    use std::io::ErrorKind::*;
    if certificate(e).is_some() {
        return Fault::Refused;
    }
    // Past the contexts (which name the endpoint), the transport's own
    // errors: their types and kinds first, then their words, whole (an
    // error's words may hold a host's name: `rbe-timeouts.corp`).
    let mut transport = false;
    let mut cause = Some(e);
    while let Some(c) = cause {
        if c.is::<NoAnswer>() || c.is::<tonic::TimeoutExpired>() {
            return Fault::NoAnswer;
        }
        if let Some(s) = c.downcast_ref::<tonic::Status>() {
            if from_server(s) {
                return Fault::None;
            }
            transport = true;
        }
        transport |= c.is::<tonic::transport::Error>();
        if let Some(io) = c.downcast_ref::<std::io::Error>() {
            match io.kind() {
                ConnectionRefused | HostUnreachable | NetworkUnreachable | NetworkDown | AddrNotAvailable | NotConnected => return Fault::NoConnection,
                TimedOut => return Fault::NoAnswer,
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof => return Fault::Broke,
                // rustls's: an alert received, a message it can't read.
                InvalidData => return Fault::Refused,
                _ => {}
            }
        }
        if transport {
            let text = c.to_string().to_ascii_lowercase();
            match text.as_str() {
                // hyper's connector's: a name that doesn't resolve has no
                // error kind of its own.
                "dns error" | "tcp connect error" => return Fault::NoConnection,
                // tonic's TLS handshake timeout.
                "tls handshake timeout." => return Fault::NoAnswer,
                // tonic's TLS without HTTP/2, and an HTTP/2 protocol error
                // found here (`detected`, not `received`): the other end
                // speaks something else.
                "http/2 was not negotiated." => return Fault::Refused,
                t if t.starts_with("connection error detected: ") || t.starts_with("stream error detected: ") => return Fault::Refused,
                _ => {}
            }
        }
        cause = c.source();
    }
    // A transport error saying no more (hyper's "connection closed"): it
    // broke.
    if transport { Fault::Broke } else { Fault::None }
}

/// Whose certificate a connection was refused for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Certificate {
    /// The server's, by this client: not trusted, expired, for another
    /// name, or none at all.
    Server,
    /// This client's, by the server, whose alert says why: none sent where
    /// one is required, one it doesn't trust, expired, revoked...
    Client,
}

/// The alerts of a server refusing the client's certificate.
const CLIENT_CERTIFICATE_ALERTS: [rustls::AlertDescription; 8] = {
    use rustls::AlertDescription::*;
    [BadCertificate, UnsupportedCertificate, CertificateRevoked, CertificateExpired, CertificateUnknown, UnknownCA, CertificateRequired, AccessDenied]
};

/// Whether `e`, with its causes, is a certificate refused, and whose: a
/// `Fault::Refused` that no other server, nor calling again, would fix.
/// Told from rustls's error, which tokio-rustls wraps in an `io::Error`
/// (reached with `get_ref`: that error's `source` skips it), or from its
/// words, all that is left of it once the connection was made (h2 keeps
/// an `io::Error`'s kind and words only). Not every refusal says so: a
/// server may close the connection right after its alert, which Windows
/// then resets before the client read it, or close it without one; that is
/// a connection that broke (`Fault::Broke`).
pub fn certificate(e: &(dyn Error + 'static)) -> Option<Certificate> {
    let mut cause = Some(e);
    while let Some(c) = cause {
        if let Some(tls) = c.downcast_ref::<rustls::Error>() {
            return whose(tls);
        }
        if let Some(io) = c.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::InvalidData
            && let Some(inner) = io.get_ref()
        {
            return match inner.downcast_ref::<rustls::Error>() {
                Some(tls) => whose(tls),
                None => whose_words(&inner.to_string()),
            };
        }
        cause = c.source();
    }
    None
}

/// Whose certificate rustls's error `e` is about, if it's one refused.
fn whose(e: &rustls::Error) -> Option<Certificate> {
    match e {
        // On a client, the server's.
        rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented => Some(Certificate::Server),
        rustls::Error::AlertReceived(alert) if CLIENT_CERTIFICATE_ALERTS.contains(alert) => Some(Certificate::Client),
        _ => None,
    }
}

/// `whose`, from the words of rustls's error, whole but for what its
/// certificate errors add (which may name a host).
fn whose_words(text: &str) -> Option<Certificate> {
    if let Some(alert) = text.strip_prefix("received fatal alert: ") {
        return CLIENT_CERTIFICATE_ALERTS.iter().any(|a| format!("{a:?}") == alert).then_some(Certificate::Client);
    }
    (text.starts_with("invalid peer certificate: ") || text == "peer sent no certificates").then_some(Certificate::Server)
}

/// Whether `e` is the service saying it can't serve now: UNAVAILABLE from
/// the server itself (a proxy without a healthy backend, a service
/// shedding load). After the retries, as good as unreachable to a build
/// that may do without it.
pub fn unavailable(e: &(dyn Error + 'static)) -> bool {
    let mut cause = Some(e);
    while let Some(c) = cause {
        if let Some(s) = c.downcast_ref::<tonic::Status>() {
            return from_server(s) && s.code() == tonic::Code::Unavailable;
        }
        cause = c.source();
    }
    false
}

/// Whether `s` stands for a connection that broke once made (reset,
/// closed), rather than an answer, a call left unanswered, a connection
/// there never was or one refused for its certificate: worth another try
/// even before the service answered anything.
pub fn broken_connection(s: &tonic::Status) -> bool {
    fault(s) == Fault::Broke
}

/// An extra gRPC header (`--remote_header`, `--bes_header`) as metadata:
/// a valid name (lowercased) and an ASCII value.
pub fn header(name: &str, value: &str) -> anyhow::Result<(tonic::metadata::AsciiMetadataKey, tonic::metadata::AsciiMetadataValue)> {
    use anyhow::Context;
    let key = tonic::metadata::AsciiMetadataKey::from_bytes(name.to_ascii_lowercase().as_bytes()).with_context(|| format!("invalid header name `{name}`"))?;
    let value = value.parse().with_context(|| format!("invalid value for header `{name}`"))?;
    Ok((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults() {
        let io = |kind| -> Box<dyn Error + Send + Sync> { Box::new(std::io::Error::new(kind, "x")) };
        let status = |e: Box<dyn Error + Send + Sync>| {
            let mut s = tonic::Status::unavailable("transport error");
            s.set_source(Arc::from(e));
            s
        };
        // The server's answer, whatever its code.
        assert_eq!(fault(&tonic::Status::unavailable("no healthy upstream")), Fault::None);
        assert_eq!(fault(&no_answer("no answer within 1s")), Fault::NoAnswer);
        assert_eq!(fault(&status(io(std::io::ErrorKind::ConnectionRefused))), Fault::NoConnection);
        assert_eq!(fault(&status(io(std::io::ErrorKind::TimedOut))), Fault::NoAnswer);
        assert_eq!(fault(&status(io(std::io::ErrorKind::ConnectionReset))), Fault::Broke);
        assert_eq!(fault(&status(io(std::io::ErrorKind::InvalidData))), Fault::Refused);
        assert_eq!(fault(&status("connection closed".into())), Fault::Broke);
        assert_eq!(fault(&status("dns error".into())), Fault::NoConnection);
        assert_eq!(fault(&status("connection error detected: frame with invalid size".into())), Fault::Refused);
        assert_eq!(fault(&status("TLS handshake timeout.".into())), Fault::NoAnswer);
        // Contexts' words don't count: they name the endpoint.
        let e = anyhow::anyhow!("unsupported endpoint scheme `htps`").context("connecting to grpcs://certificate-timeout.example:1");
        assert_eq!(fault(e.as_ref()), Fault::None);
        let e = anyhow::Error::new(NoAnswer).context("connecting to grpcs://x:1: no connection within 20s");
        assert_eq!(fault(e.as_ref()), Fault::NoAnswer);
        assert!(Fault::NoConnection.unreachable() && Fault::NoAnswer.unreachable());
        assert!(!Fault::Broke.unreachable() && !Fault::Refused.unreachable() && !Fault::None.unreachable());
        // UNAVAILABLE said by the server, not a transport error standing
        // for one.
        let e = anyhow::Error::new(tonic::Status::unavailable("no healthy upstream")).context("GetCapabilities");
        assert!(unavailable(e.as_ref()));
        assert!(!unavailable(&status(io(std::io::ErrorKind::ConnectionReset))));
        assert!(!unavailable(&tonic::Status::permission_denied("no")));
    }

    /// An error saying `text`, caused by `source`: a level of a chain.
    #[derive(Debug)]
    struct Said(&'static str, Box<dyn Error + Send + Sync>);

    impl std::fmt::Display for Said {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for Said {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.1.as_ref())
        }
    }

    /// Certificates refused are told from rustls's errors, wrapped in an
    /// `io::Error` as tokio-rustls does: the server's, the client's (the
    /// server's alert), and what isn't one.
    #[test]
    fn certificates_refused() {
        use rustls::{AlertDescription, CertificateError};
        let tls = |e: rustls::Error| {
            let mut s = tonic::Status::unknown("transport error");
            s.set_source(Arc::new(Said("connection error", Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))));
            s
        };
        for (e, whose) in [
            (rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer), Some(Certificate::Server)),
            (rustls::Error::InvalidCertificate(CertificateError::Expired), Some(Certificate::Server)),
            (rustls::Error::NoCertificatesPresented, Some(Certificate::Server)),
            (rustls::Error::AlertReceived(AlertDescription::CertificateRequired), Some(Certificate::Client)),
            (rustls::Error::AlertReceived(AlertDescription::CertificateExpired), Some(Certificate::Client)),
            (rustls::Error::AlertReceived(AlertDescription::UnknownCA), Some(Certificate::Client)),
            // Refused, but not for a certificate: another server may do.
            (rustls::Error::AlertReceived(AlertDescription::HandshakeFailure), None),
            (rustls::Error::AlertReceived(AlertDescription::ProtocolVersion), None),
            (rustls::Error::InvalidMessage(rustls::InvalidMessage::InvalidContentType), None),
        ] {
            let s = tls(e.clone());
            assert_eq!((certificate(&s), fault(&s)), (whose, Fault::Refused), "{e:?}");
            // Once the connection was made, h2 keeps its words only.
            let mut s = tonic::Status::unknown("transport error");
            s.set_source(Arc::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())));
            assert_eq!((certificate(&s), fault(&s)), (whose, Fault::Refused), "{e}");
        }
        // Words don't count, whatever names they hold: a certificate of
        // another name than `rbe-timeouts.corp` isn't a timeout.
        let name = Said(
            "invalid peer certificate: certificate not valid for name \"rbe-timeouts.corp\"",
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, rustls::Error::InvalidCertificate(CertificateError::NotValidForName))),
        );
        let mut s = tonic::Status::unknown("transport error");
        s.set_source(Arc::new(name));
        assert_eq!((certificate(&s), fault(&s)), (Some(Certificate::Server), Fault::Refused));
        let mut s = tonic::Status::unknown("transport error");
        s.set_source(Arc::new(Said("tls handshake timeout of rbe-timeouts.corp", "x".into())));
        assert_eq!(fault(&s), Fault::Broke);
        // A connection reset carries no rustls error, nor does the server's
        // own answer.
        let mut reset = tonic::Status::unknown("transport error");
        reset.set_source(Arc::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)));
        assert_eq!((certificate(&reset), fault(&reset)), (None, Fault::Broke));
        assert_eq!(certificate(&tonic::Status::permission_denied("certificate")), None);
    }
}
