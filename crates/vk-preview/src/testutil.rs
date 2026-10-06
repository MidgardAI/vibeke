//! Test helpers: a self-signed certificate generated in-test with the `openssl` CLI (no
//! network, no fixtures) and tiny loopback TLS servers.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Cert {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

/// A fresh self-signed RSA certificate for `localhost` (DER cert + PKCS#8 DER key), or `None`
/// when `openssl` is not installed.
pub fn self_signed() -> Option<Cert> {
    let dir = tempfile::tempdir().ok()?;
    let d = dir.path();
    let ok = std::process::Command::new("openssl")
        .current_dir(d)
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "key.pem",
            "-out",
            "cert.der",
            "-outform",
            "DER",
            "-days",
            "2",
            "-subj",
            "/CN=localhost",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?
        .success()
        && std::process::Command::new("openssl")
            .current_dir(d)
            .args([
                "pkcs8", "-topk8", "-nocrypt", "-in", "key.pem", "-outform", "DER", "-out",
                "key.der",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
    if !ok {
        return None;
    }
    Some(Cert {
        cert: std::fs::read(d.join("cert.der")).ok()?,
        key: std::fs::read(d.join("key.der")).ok()?,
    })
}

pub fn acceptor(c: &Cert) -> tokio_rustls::TlsAcceptor {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(c.cert.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(c.key.clone())),
        )
        .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(cfg))
}

pub struct TlsSrv {
    pub port: u16,
    /// Request heads received (as text).
    pub requests: Arc<Mutex<Vec<String>>>,
}

/// A TLS server on 127.0.0.1 answering every request with `resp` and closing.
pub async fn tls_server(c: &Cert, resp: &'static str) -> TlsSrv {
    let acc = acceptor(c);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let acc = acc.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let Ok(mut t) = acc.accept(s).await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 2048];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16384 {
                    match t.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let _ = t.write_all(resp.as_bytes()).await;
                let _ = t.shutdown().await;
            });
        }
    });
    TlsSrv { port, requests }
}
