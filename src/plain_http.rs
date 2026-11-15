//! Plain HTTP on the HTTPS port. Browsers send plain HTTP when an address on a
//! port other than 443 is typed without `https://`. A TLS-only listener answers
//! that with a TLS alert, which browsers show as an error ("cannot decode raw
//! data" in Safari on iOS, an empty response in Chrome) instead of going on to
//! HTTPS.
//!
//! [`HttpsOnlyAcceptor`] peeks at the first byte of each connection. A TLS
//! handshake goes on to TLS untouched (nothing is read or buffered). Anything
//! else gets one plain-HTTP answer, then the connection is closed. The answer
//! serves nothing from the site and leaves nothing behind in the browser:
//!
//! - `GET`/`HEAD`: a `307` to the same URL over HTTPS, marked `no-store`. A
//!   permanent redirect (`301`/`308`) would be cached indefinitely, and keep
//!   redirecting if something else later serves plain HTTP on this address.
//! - A request a reverse proxy says came over HTTPS: a `400` naming the proxy,
//!   since redirecting it would loop.
//! - Any other method, or no usable host: a `400`, so a write is never replayed.
//!
//! The same design as Caddy's `http_redirect` listener wrapper. The redirect
//! keeps the host and port the browser was given (its `Host`), not the port the
//! server listens on, which differs behind a Docker port mapping.
//!
//! The connection closes gracefully, so a client still sending gets the answer
//! instead of a reset (see [`close_gracefully`]). One deadline covers each
//! connection from being accepted to an answer or a finished TLS handshake.

use axum::http::{
    HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header, uri::Authority,
};
use axum_server::accept::Accept;
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{convert::Infallible, future::Future, io, pin::Pin, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Instant, error::Elapsed, timeout, timeout_at};

/// First byte of every TLS connection: the record type of the ClientHello.
/// HTTP requests start with a method name, i.e. an ASCII letter.
const TLS_HANDSHAKE: u8 = 0x16;
/// Time from accepting a connection to an answered plain-HTTP request or a
/// finished TLS handshake. The TLS acceptor's own handshake timeout stays as a
/// backstop.
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep reading and discarding what a client still sends after
/// the answer, however much it is (see [`close_gracefully`]). Bounded by time
/// alone, like nginx's lingering close: closing after a set number of bytes
/// would reset a larger upload anyway. An upload still being sent after that
/// is reset; only a misdirected write sends that much plain HTTP.
const LINGER_TIME: Duration = Duration::from_secs(2);

/// Wraps the TLS acceptor: TLS connections are passed to it, plain HTTP is
/// answered here (see the module docs).
#[derive(Clone)]
pub struct HttpsOnlyAcceptor<A> {
    tls: A,
    timeout: Duration,
}

impl<A> HttpsOnlyAcceptor<A> {
    pub fn new(tls: A) -> Self {
        Self::with_timeout(tls, TIMEOUT)
    }

    /// [`HttpsOnlyAcceptor::new`] with a deadline other than [`TIMEOUT`].
    fn with_timeout(tls: A, timeout: Duration) -> Self {
        Self { tls, timeout }
    }
}

impl<A, S> Accept<TcpStream, S> for HttpsOnlyAcceptor<A>
where
    A: Accept<TcpStream, S> + Clone + Send + 'static,
    A::Future: Send,
    S: Send + 'static,
{
    type Stream = A::Stream;
    type Service = A::Service;
    type Future = Pin<Box<dyn Future<Output = io::Result<(A::Stream, A::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let tls = self.tls.clone();
        let deadline = Instant::now() + self.timeout;
        Box::pin(async move {
            if first_byte(&stream, deadline).await? == TLS_HANDSHAKE {
                return timeout_at(deadline, tls.accept(stream, service))
                    .await
                    .map_err(timed_out)?;
            }
            serve_plain_http(stream, deadline).await;
            Err(io::Error::other("plain HTTP on the HTTPS port"))
        })
    }
}

/// The connection's first byte, left unread for TLS.
async fn first_byte(stream: &TcpStream, deadline: Instant) -> io::Result<u8> {
    let mut byte = [0u8; 1];
    match timeout_at(deadline, stream.peek(&mut byte))
        .await
        .map_err(timed_out)??
    {
        1 => Ok(byte[0]),
        _ => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

/// A passed deadline, as the error the connection ends with.
fn timed_out(_: Elapsed) -> io::Error {
    io::ErrorKind::TimedOut.into()
}

/// Answers one request, then closes gracefully. Bytes that aren't HTTP get
/// hyper's `400`.
async fn serve_plain_http(mut stream: TcpStream, deadline: Instant) {
    let service =
        service_fn(|request| async move { Ok::<_, Infallible>(redirect_or_refusal(&request)) });
    // Lent to hyper, so the stream comes back whether hyper answered a request
    // or gave up on bytes that aren't HTTP.
    let connection = http1::Builder::new()
        .keep_alive(false)
        .serve_connection(TokioIo::new(&mut stream), service);
    if timeout_at(deadline, connection).await.is_ok() {
        close_gracefully(stream).await;
    }
}

/// Closes after an answer the way RFC 9112 (section 9.6), nginx's lingering
/// close and Go's `closeWriteAndWait` do: end the sending side, then read and
/// discard what the client still sends (e.g. the rest of a body) until it
/// closes, within [`LINGER_TIME`]. Closing with unread data makes the OS reset
/// the connection, which Apple clients report as an error and which can discard
/// an answer still in flight. A browser closes as soon as it has read the
/// answer, so this usually ends at once.
async fn close_gracefully(mut stream: TcpStream) {
    // hyper has usually ended the sending side already.
    let _ = stream.shutdown().await;
    let mut buffer = [0u8; 8192];
    let _ = timeout(LINGER_TIME, async {
        while let Ok(1..) = stream.read(&mut buffer).await {}
    })
    .await;
}

/// The one answer plain HTTP gets (see the module docs).
fn redirect_or_refusal<B>(request: &Request<B>) -> Response<String> {
    let proxied = forwarded_from_https(request.headers());
    let redirect = https_url(request)
        .filter(|_| !proxied && matches!(*request.method(), Method::GET | Method::HEAD));
    let (status, body) = match (&redirect, proxied) {
        (Some(_), _) => (
            StatusCode::TEMPORARY_REDIRECT,
            "This site is served over HTTPS only.\n",
        ),
        (None, true) => (
            StatusCode::BAD_REQUEST,
            "A proxy sends plain HTTP to this HTTPS-only server: point it at https://\n",
        ),
        (None, false) => (
            StatusCode::BAD_REQUEST,
            "This site is served over HTTPS only: open it with https://\n",
        ),
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store");
    if let Some(url) = redirect {
        response = response.header(header::LOCATION, url);
    }
    response
        .body(body.into())
        .expect("static headers are valid")
}

/// Whether a reverse proxy says the client used HTTPS (`X-Forwarded-Proto`, or
/// `proto` in RFC 7239 `Forwarded`). The proxy then sends plain HTTP here, and a
/// redirect to HTTPS would come back through it as plain HTTP again: a loop.
/// Django's `SECURE_PROXY_SSL_HEADER` reads the same signal. The header only
/// ever turns a redirect into an error, so a forged one affects only its sender.
fn forwarded_from_https(headers: &HeaderMap) -> bool {
    let values = |name| {
        headers
            .get_all(name)
            .into_iter()
            .filter_map(|value| value.to_str().ok())
    };
    let is_https = |value: &str| value.trim().trim_matches('"').eq_ignore_ascii_case("https");
    let x_forwarded = values("x-forwarded-proto").flat_map(|value| value.split(','));
    let forwarded = values("forwarded")
        .flat_map(|value| value.split([',', ';']))
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            name.trim().eq_ignore_ascii_case("proto").then_some(value)
        });
    x_forwarded.chain(forwarded).any(is_https)
}

/// `request`'s URL with `https://`: the host and port the browser was given,
/// and the path and query. `None` without a usable host.
fn https_url<B>(request: &Request<B>) -> Option<HeaderValue> {
    // An absolute-form target (`GET http://host/ HTTP/1.1`) overrides `Host`
    // (RFC 9112, section 3.2.2).
    let authority = match request.uri().authority() {
        Some(authority) => authority.clone(),
        None => request
            .headers()
            .get(header::HOST)?
            .to_str()
            .ok()?
            .parse::<Authority>()
            .ok()?,
    };
    // User info (`user@host`) is never part of a browser's `Host`.
    if authority.host().is_empty() || authority.as_str().contains('@') {
        return None;
    }
    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str())
        .filter(|path| path.starts_with('/'))
        .unwrap_or("/");
    HeaderValue::try_from(format!("https://{authority}{path}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config, tls};
    use axum::{Router, routing::get};
    use axum_server::tls_rustls::RustlsAcceptor;
    use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
    use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
    use std::io::{Read, Write};
    use std::net::SocketAddr;
    use std::sync::Arc;

    /// A short deadline, so that tests of it stay fast.
    const SHORT_TIMEOUT: Duration = Duration::from_secs(1);

    fn request(method: Method, target: &str, host: Option<&str>) -> Request<()> {
        let mut request = Request::builder().method(method).uri(target);
        if let Some(host) = host {
            request = request.header(header::HOST, host);
        }
        request.body(()).unwrap()
    }

    fn location(response: &Response<String>) -> Option<&str> {
        let location = response.headers().get(header::LOCATION)?;
        Some(location.to_str().unwrap())
    }

    #[test]
    fn reads_are_redirected_to_the_same_url_over_https() {
        for (target, host, expected) in [
            ("/", "albums.local:8443", "https://albums.local:8443/"),
            (
                "/albums/q2bMf0?x=1",
                "192.168.1.20:18443",
                "https://192.168.1.20:18443/albums/q2bMf0?x=1",
            ),
            ("/", "[::1]:8443", "https://[::1]:8443/"),
            // Port 80 mapped to this server: the default HTTPS port.
            ("/", "albums.local", "https://albums.local/"),
            (
                "http://albums.local:8443/a",
                "ignored:1",
                "https://albums.local:8443/a",
            ),
        ] {
            for method in [Method::GET, Method::HEAD] {
                let response = redirect_or_refusal(&request(method, target, Some(host)));
                assert_eq!(
                    response.status(),
                    StatusCode::TEMPORARY_REDIRECT,
                    "{target} {host}"
                );
                assert_eq!(location(&response), Some(expected));
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            }
        }
    }

    #[test]
    fn writes_and_requests_without_a_usable_host_are_refused() {
        for (method, target, host) in [
            (Method::POST, "/login", Some("albums.local:8443")),
            (Method::PUT, "/", Some("albums.local:8443")),
            (Method::GET, "/", None),
            (Method::GET, "/", Some("")),
            (Method::GET, "/", Some("user@evil.example")),
            (Method::GET, "/", Some(":8443")),
            (Method::OPTIONS, "*", Some("albums.local:8443")),
        ] {
            let response = redirect_or_refusal(&request(method.clone(), target, host));
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{method} {target} {host:?}"
            );
            assert_eq!(location(&response), None);
        }
    }

    #[test]
    fn forwarded_https_is_refused_not_redirected() {
        for (name, value) in [
            ("x-forwarded-proto", "https"),
            ("x-forwarded-proto", "http, HTTPS"),
            ("forwarded", "for=192.0.2.1;proto=https"),
            ("forwarded", "for=192.0.2.1, proto=\"https\""),
        ] {
            let mut forwarded = request(Method::GET, "/", Some("albums.local"));
            forwarded
                .headers_mut()
                .insert(name, HeaderValue::from_static(value));
            let response = redirect_or_refusal(&forwarded);
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{name}: {value}"
            );
            assert_eq!(location(&response), None);
            assert!(response.body().contains("proxy"), "{name}: {value}");
        }
        let mut forwarded = request(Method::GET, "/", Some("albums.local"));
        forwarded
            .headers_mut()
            .insert("x-forwarded-proto", HeaderValue::from_static("http"));
        let response = redirect_or_refusal(&forwarded);
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    }

    /// Serves "albums" over HTTPS behind [`HttpsOnlyAcceptor`]; returns the
    /// address and the self-signed certificate.
    async fn server(dir: &std::path::Path) -> (SocketAddr, CertificateDer<'static>) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let settings = config::Tls {
            cert: dir.join("cert.pem"),
            key: dir.join("key.pem"),
            self_signed_names: vec!["localhost".into()],
            min_version: config::TlsVersion::Tls13,
            hsts_max_age: Duration::ZERO,
        };
        let tls = RustlsAcceptor::new(tls::load_or_create(&settings).unwrap());
        let acceptor = HttpsOnlyAcceptor::with_timeout(tls, SHORT_TIMEOUT);
        let certificate = CertificateDer::from_pem_file(&settings.cert).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(|| async { "albums" }));
        let server = axum_server::from_tcp(listener).unwrap().acceptor(acceptor);
        tokio::spawn(server.serve(app.into_make_service()));
        (address, certificate)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_port_answers_plain_http_and_serves_https() {
        let dir = tempfile::tempdir().unwrap();
        let (address, certificate) = server(dir.path()).await;

        let head = b"GET /albums/a?b=1 HTTP/1.1\r\nHost: albums.local:18443\r\n\r\n".to_vec();
        let response = exchange(address, head, Duration::ZERO, Vec::new()).await;
        assert!(response.starts_with("HTTP/1.1 307 "), "{response}");
        assert!(
            response.contains("location: https://albums.local:18443/albums/a?b=1\r\n"),
            "{response}"
        );
        assert!(
            !response.contains("albums\n") && !response.contains("set-cookie"),
            "{response}"
        );

        // TLS on the same port is untouched, and the generated certificate
        // verifies when trusted (name, EKU, validity).
        let body = tokio::task::spawn_blocking(move || {
            let mut roots = RootCertStore::empty();
            roots.add(certificate).unwrap();
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = ServerName::try_from("localhost").unwrap();
            let connection = ClientConnection::new(Arc::new(config), name).unwrap();
            let socket = std::net::TcpStream::connect(address).unwrap();
            let mut tls = StreamOwned::new(connection, socket);
            tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut response = Vec::new();
            let _ = tls.read_to_end(&mut response);
            String::from_utf8(response).unwrap()
        })
        .await
        .unwrap();
        assert!(body.starts_with("HTTP/1.1 200 "), "{body}");
        assert!(body.ends_with("albums"), "{body}");
    }

    /// Sends `head` over plain TCP, then `rest` after `pause`, then reads until
    /// the server closes. Returns the answer, after checking the connection
    /// ended cleanly rather than with a reset that could have discarded it.
    async fn exchange(
        address: SocketAddr,
        head: Vec<u8>,
        pause: Duration,
        rest: Vec<u8>,
    ) -> String {
        let (read, answer) = tokio::task::spawn_blocking(move || {
            let mut socket = std::net::TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket.write_all(&head).unwrap();
            std::thread::sleep(pause);
            for chunk in rest.chunks(64 << 10) {
                if socket.write_all(chunk).is_err() {
                    break;
                }
            }
            let mut answer = Vec::new();
            let read = socket.read_to_end(&mut answer).map(|_| ());
            (read, String::from_utf8_lossy(&answer).into_owned())
        })
        .await
        .unwrap();
        assert!(read.is_ok(), "{read:?} after {answer:?}");
        answer
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_still_being_sent_gets_a_clean_close() {
        const BODY: usize = 4 << 20;
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = server(dir.path()).await;
        let head =
            format!("POST /login HTTP/1.1\r\nHost: albums.local\r\nContent-Length: {BODY}\r\n\r\n");
        // The body is still on its way when the server has answered (a slow upload).
        let pause = Duration::from_millis(200);
        let answer = exchange(address, head.into_bytes(), pause, vec![b'a'; BODY]).await;
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bytes_that_are_not_http_get_a_clean_close() {
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = server(dir.path()).await;
        let head = b"\x01\x02\x03\r\n\r\n".to_vec();
        // More keeps coming, as from a client that isn't speaking HTTP at all.
        let answer = exchange(address, head, Duration::ZERO, vec![b'a'; 4 << 20]).await;
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_connection_gets_one_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (address, _) = server(dir.path()).await;
        let elapsed = tokio::task::spawn_blocking(move || {
            let mut socket = std::net::TcpStream::connect(address).unwrap();
            socket.set_read_timeout(Some(SHORT_TIMEOUT * 20)).unwrap();
            let started = std::time::Instant::now();
            // The first byte arrives late, then the handshake never comes.
            std::thread::sleep(SHORT_TIMEOUT / 2);
            socket.write_all(&[TLS_HANDSHAKE]).unwrap();
            let _ = socket.read(&mut [0u8; 64]); // EOF once the server gives up
            started.elapsed()
        })
        .await
        .unwrap();
        assert!(elapsed < SHORT_TIMEOUT * 3 / 2, "closed after {elapsed:?}");
    }
}
