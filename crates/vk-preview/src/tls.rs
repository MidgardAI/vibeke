//! TLS to **loopback dev servers** (06 B2 TLS probe, B4 HTTPS upstreams).
//!
//! Dev servers on `localhost` serve self-signed (or locally-trusted) certificates, so the
//! certificate chain is not verified here — the handshake signature still is, so the peer must
//! hold the key of the certificate it presented. This connector is only ever applied to a
//! stream that already reaches a loopback port: this machine's `127.0.0.1`/`::1`, or a bridge
//! `tcp:` channel (the bridge refuses non-loopback targets before connecting). It is never used
//! for arbitrary hosts. Nothing secret is sent through it: no client certificate, and the probe
//! sends no cookies or `Authorization` headers.

use crate::probe::{connect_any, looks_like_web_page, targets};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Accepts any certificate chain (self-signed dev certificates), verifies handshake signatures.
#[derive(Debug)]
struct DevServerVerifier(Arc<CryptoProvider>);

impl ServerCertVerifier for DevServerVerifier {
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
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Client config for loopback dev servers: ring provider, TLS 1.2/1.3, ALPN `http/1.1` (the
/// proxy speaks HTTP/1.1 upstream), no client authentication.
pub fn dev_server_config() -> Arc<ClientConfig> {
    static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CFG.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut cfg = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(DevServerVerifier(provider)))
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Arc::new(cfg)
    })
    .clone()
}

/// TLS handshake (SNI `localhost`) over a stream that reaches a loopback dev server.
pub async fn connect_dev_server<S>(s: S, timeout: Duration) -> std::io::Result<TlsStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let name = ServerName::try_from("localhost").expect("valid DNS name");
    tokio::time::timeout(
        timeout,
        TlsConnector::from(dev_server_config()).connect(name, s),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "TLS handshake timed out"))?
}

/// What a TLS probe of a loopback port found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TlsProbe {
    /// The TLS handshake completed.
    pub tls: bool,
    /// An `HTTP/` reply came back over TLS.
    pub http: bool,
    /// The reply is a web page ([`looks_like_web_page`]).
    pub web_page: bool,
}

/// Probe `https://localhost:<port>/` on this machine's loopback: TLS handshake (self-signed
/// accepted), then `GET / HTTP/1.0` with no credentials, classified like the plain probe.
pub async fn probe(bind: Option<IpAddr>, port: u16, timeout: Duration) -> TlsProbe {
    let mut out = TlsProbe::default();
    let fut = async {
        let s = connect_any(&targets(bind, port), timeout).await?;
        let mut t = connect_dev_server(s, timeout).await.ok()?;
        out.tls = true;
        t.write_all(
            b"GET / HTTP/1.0\r\nHost: localhost\r\nAccept: text/html,application/xhtml+xml\r\nUser-Agent: vibeke-preview-probe\r\nConnection: close\r\n\r\n",
        )
        .await
        .ok()?;
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 2048];
        while buf.len() < 4096 {
            let n = match t.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => n,
                // Servers that close without close_notify: keep what arrived.
                Err(_) => break,
            };
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                && buf.len() >= i + 4 + 64
            {
                break;
            }
        }
        out.http = buf.starts_with(b"HTTP/");
        out.web_page = looks_like_web_page(&buf);
        Some(())
    };
    let _ = tokio::time::timeout(timeout * 2, fut).await;
    out
}

/// A TLS web page on `port` (an `https` output URL or a TLS listener, 06 B2).
pub async fn is_web_page(bind: Option<IpAddr>, port: u16, timeout: Duration) -> bool {
    probe(bind, port, timeout).await.web_page
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    #[tokio::test]
    async fn probes_a_self_signed_dev_server() {
        let Some(cert) = testutil::self_signed() else {
            eprintln!("openssl not available; skipping");
            return;
        };
        let srv = testutil::tls_server(
            &cert,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<!doctype html><html><body>secure dev</body></html>",
        )
        .await;
        let t = Duration::from_secs(2);
        let p = probe(None, srv.port, t).await;
        assert_eq!(
            p,
            TlsProbe {
                tls: true,
                http: true,
                web_page: true
            }
        );
        assert!(is_web_page(Some("127.0.0.1".parse().unwrap()), srv.port, t).await);
        // The probe sent no credentials.
        let seen = srv.requests.lock().unwrap().clone();
        assert!(!seen.is_empty());
        for r in &seen {
            let l = r.to_ascii_lowercase();
            assert!(l.starts_with("get / http/1.0\r\n"), "{r}");
            assert!(
                !l.contains("cookie:") && !l.contains("authorization:"),
                "{r}"
            );
        }
        // A TLS server that answers JSON is TLS + HTTP but not a page.
        let api = testutil::tls_server(
            &cert,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true}",
        )
        .await;
        let p = probe(None, api.port, t).await;
        assert!(p.tls && p.http && !p.web_page, "{p:?}");
    }

    #[tokio::test]
    async fn plain_http_and_closed_ports_are_not_tls() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut b = [0u8; 512];
                let _ = s.read(&mut b).await;
                let _ = s
                    .write_all(b"HTTP/1.0 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<html>")
                    .await;
            }
        });
        let t = Duration::from_millis(800);
        assert_eq!(probe(None, port, t).await, TlsProbe::default());
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(probe(None, closed, t).await, TlsProbe::default());
    }
}
