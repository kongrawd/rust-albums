//! `rust-albums --healthcheck`: for container HEALTHCHECKs in images without
//! curl. Connects to this server over loopback TLS and requires the health route to
//! answer 200, which it does only while the listener is up and storage is
//! available (see `web::healthz`).
//!
//! The server's certificate is not verified: the check sends no credentials,
//! only reads a status line, and talks to the loopback interface, so there is
//! nothing for an impostor to learn or change. Verifying would also fail for
//! certificates issued to a public host name.

use crate::config::Config;
use crate::routes;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);

/// `Ok` if the server reports healthy; otherwise why not.
pub fn run(config: &Config) -> Result<(), String> {
    let bind = config.server.bind_addr;
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let address = SocketAddr::new(ip, bind.port());

    let client = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate))
        .with_no_client_auth();
    let name = ServerName::try_from("localhost").expect("valid name");
    let connection = ClientConnection::new(Arc::new(client), name).map_err(|e| e.to_string())?;
    let socket = TcpStream::connect_timeout(&address, TIMEOUT)
        .map_err(|e| format!("connecting to {address}: {e}"))?;
    socket.set_read_timeout(Some(TIMEOUT)).ok();
    socket.set_write_timeout(Some(TIMEOUT)).ok();
    let mut tls = StreamOwned::new(connection, socket);

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: rust-albums-healthcheck\r\n\
         Connection: close\r\n\r\n",
        routes::HEALTH
    );
    tls.write_all(request.as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    let mut response = Vec::new();
    match tls.read_to_end(&mut response) {
        Ok(_) => {}
        // Servers may close without a TLS close_notify; the response is complete.
        Err(e) if e.kind() == ErrorKind::UnexpectedEof && !response.is_empty() => {}
        Err(e) => return Err(format!("reading response: {e}")),
    }
    let response = String::from_utf8_lossy(&response);
    let (status_line, rest) = response.split_once("\r\n").unwrap_or((&response, ""));
    let body = rest
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body.trim());
    match status_line.split_whitespace().nth(1) {
        Some("200") => Ok(()),
        Some(status) => Err(format!("{} answered {status}: {body}", routes::HEALTH)),
        None => Err(format!("unexpected response: {status_line:?}")),
    }
}

#[derive(Debug)]
struct AnyCertificate;

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
