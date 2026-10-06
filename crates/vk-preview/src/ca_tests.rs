//! The local preview CA: first-use generation and permissions, name constraints (ours and the
//! client's), leaf caching/rotation, expiry, and that nothing here touches a trust store.

use super::*;
use rustls::pki_types::ServerName;
use std::os::unix::fs::PermissionsExt;
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

pub(crate) fn client_trusting(cas: &[CertificateDer<'static>]) -> Arc<rustls::ClientConfig> {
    client_config_trusting(cas)
}

/// In-memory TLS handshake: the client trusts only `trusted`, the server presents `key`.
async fn handshake(
    trusted: &[CertificateDer<'static>],
    key: Arc<CertifiedKey>,
    sni: &str,
) -> Result<(), String> {
    let (c, s) = tokio::io::duplex(64 * 1024);
    let acceptor = TlsAcceptor::from(server_config(Arc::new(Fixed(key))));
    let connector = TlsConnector::from(client_trusting(trusted));
    let name = ServerName::try_from(sni.to_string()).map_err(|e| e.to_string())?;
    let server = tokio::spawn(async move { acceptor.accept(s).await.map(|_| ()) });
    let client = connector.connect(name, c).await.map(|_| ());
    let _ = server.await;
    client.map_err(|e| e.to_string())
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn generated_on_first_use_with_private_permissions_and_reused() {
    let d = tmp();
    let dir = d.path().join("tls");
    assert!(!dir.exists());
    let ca = LocalCa::load_or_create(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(CA_KEY_FILE)), 0o600);
    assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
    assert_eq!(ca.ca_path(), dir.join(CA_CERT_FILE));
    let fp = ca.fingerprint_sha256();
    assert_eq!(fp.split(':').count(), 32, "{fp}");
    assert!(ca.spki_sha256_base64().len() == 44);
    // A second load is the same CA (the user's trust keeps working).
    let again = LocalCa::load_or_create(&dir).unwrap();
    assert_eq!(again.fingerprint_sha256(), fp);
    // A loosened key mode is tightened, not trusted.
    std::fs::set_permissions(
        dir.join(CA_KEY_FILE),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let again = LocalCa::load_or_create(&dir).unwrap();
    assert_eq!(again.fingerprint_sha256(), fp);
    assert_eq!(mode(&dir.join(CA_KEY_FILE)), 0o600);
    // A key that does not belong to the certificate: a new CA, never a mismatched pair.
    let other = KeyPair::generate().unwrap();
    std::fs::write(dir.join(CA_KEY_FILE), other.serialize_pem()).unwrap();
    let fresh = LocalCa::load_or_create(&dir).unwrap();
    assert_ne!(fresh.fingerprint_sha256(), fp);
    assert_eq!(mode(&dir.join(CA_KEY_FILE)), 0o600);
}

#[test]
fn certificate_carries_the_name_constraint_and_is_a_leafless_ca() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    let der = ca.cert_der().as_ref();
    let has = |needle: &[u8]| der.windows(needle.len()).any(|w| w == needle);
    // id-ce-nameConstraints (2.5.29.30) and the permitted DNS subtree.
    assert!(has(&[0x55, 0x1d, 0x1e]), "no nameConstraints extension");
    assert!(has(PERMITTED_DOMAIN.as_bytes()));
    // id-ce-basicConstraints (2.5.29.19), critical.
    assert!(has(&[0x55, 0x1d, 0x13]));
}

#[test]
fn refuses_to_sign_outside_the_constraint() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    for bad in [
        "example.com",
        "localhost",
        "evil.example.com",
        "vibeke.localhost.evil.com",
        "notvibeke.localhost",
        "xvibeke.localhost",
        "*.vibeke.localhost",
        "a..vibeke.localhost",
        ".vibeke.localhost",
        "vibeke.localhost.",
        "UPPER.vibeke.localhost",
        "-a.vibeke.localhost",
        "a b.vibeke.localhost",
        "127.0.0.1",
        "",
    ] {
        assert!(
            matches!(ca.issue(bad), Err(CaError::OutsideConstraints(_))),
            "{bad:?} must be refused"
        );
    }
    assert_eq!(ca.cached(), 0);
    for ok in [
        "vibeke.localhost",
        "v4-web-s0123456789.vibeke.localhost",
        "a.b.vibeke.localhost",
    ] {
        assert!(ca.issue(ok).is_ok(), "{ok}");
    }
    assert!(host_permitted("v1.vibeke.localhost"));
    assert!(!host_permitted(&format!(
        "{}.vibeke.localhost",
        "a".repeat(64)
    )));
}

#[tokio::test]
async fn a_client_trusting_only_the_ca_verifies_leaves_and_enforces_constraints() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    let trusted = [ca.cert_der().clone()];
    let host = "v4-web-s0123456789.vibeke.localhost";
    let now = SystemTime::now();
    // Right name: verifies.
    let k = ca.issue_at(host, now).unwrap();
    handshake(&trusted, k.clone(), host).await.unwrap();
    // The leaf is for that host only.
    let e = handshake(&trusted, k, "v5-web-s0123456789.vibeke.localhost")
        .await
        .unwrap_err();
    assert!(e.to_lowercase().contains("certificate"), "{e}");
    // A leaf the CA key signed for a foreign name (a bug or a stolen key) is rejected by the
    // client: the name constraint is in the CA certificate itself.
    for foreign in ["evil.example.com", "localhost", "accounts.google.com"] {
        let k = ca.issue_unchecked(foreign, now).unwrap();
        let e = handshake(&trusted, k, foreign).await.unwrap_err();
        assert!(e.to_lowercase().contains("certificate"), "{foreign}: {e}");
    }
    // A client that trusts another CA does not accept ours.
    let other = LocalCa::load_or_create(&d.path().join("other")).unwrap();
    let k = ca.issue_at(host, now).unwrap();
    assert!(
        handshake(&[other.cert_der().clone()], k, host)
            .await
            .is_err()
    );
}

#[test]
fn leaves_are_cached_then_rotated_before_they_expire() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    let t0 = SystemTime::now();
    let host = "v1-s0123456789.vibeke.localhost";
    let a = ca.issue_at(host, t0).unwrap();
    let b = ca.issue_at(host, t0 + Duration::from_secs(3600)).unwrap();
    assert!(Arc::ptr_eq(&a, &b), "cached");
    let c = ca.issue_at("v2-s0123456789.vibeke.localhost", t0).unwrap();
    assert!(!Arc::ptr_eq(&a, &c));
    assert_eq!(ca.cached(), 2);
    // Still valid for 6 days; inside the renewal window the next handshake gets a fresh one.
    let almost = t0 + LEAF_TTL - LEAF_RENEW_BEFORE + Duration::from_secs(60);
    let still = ca
        .issue_at(host, almost - Duration::from_secs(120))
        .unwrap();
    assert!(Arc::ptr_eq(&a, &still));
    let renewed = ca.issue_at(host, almost).unwrap();
    assert!(!Arc::ptr_eq(&a, &renewed));
    assert_ne!(a.cert[0].as_ref(), renewed.cert[0].as_ref());
    assert_eq!(ca.cached(), 2);
    // The renewal is itself cached.
    let again = ca.issue_at(host, almost + Duration::from_secs(60)).unwrap();
    assert!(Arc::ptr_eq(&renewed, &again));
}

#[tokio::test]
async fn an_expired_leaf_is_rejected_and_a_rotated_one_accepted() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    let trusted = [ca.cert_der().clone()];
    let host = "v1-s0123456789.vibeke.localhost";
    // Issued 8 days ago: expired now (the client checks the wall clock).
    let old = ca
        .issue_at(host, SystemTime::now() - Duration::from_secs(8 * 24 * 3600))
        .unwrap();
    assert!(handshake(&trusted, old, host).await.is_err());
    // Asking again now rotates it.
    let fresh = ca.issue(host).unwrap();
    handshake(&trusted, fresh, host).await.unwrap();
}

#[test]
fn an_expiring_ca_is_replaced() {
    let d = tmp();
    let now = SystemTime::now();
    let a = LocalCa::load_or_create_at(d.path(), now).unwrap();
    let same = LocalCa::load_or_create_at(d.path(), now + Duration::from_secs(86400)).unwrap();
    assert_eq!(a.fingerprint_sha256(), same.fingerprint_sha256());
    let late =
        LocalCa::load_or_create_at(d.path(), now + CA_TTL - Duration::from_secs(86400)).unwrap();
    assert_ne!(a.fingerprint_sha256(), late.fingerprint_sha256());
}

#[tokio::test]
async fn the_resolver_only_serves_allowed_names() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    let allowed = "v1-s0123456789.vibeke.localhost";
    let r = Arc::new(SniResolver::new(ca.clone(), move |h| h == allowed));
    let acceptor = TlsAcceptor::from(server_config(r));
    let trusted = [ca.cert_der().clone()];
    for (sni, ok) in [(allowed, true), ("v2-s0123456789.vibeke.localhost", false)] {
        let (c, s) = tokio::io::duplex(64 * 1024);
        let acc = acceptor.clone();
        let srv = tokio::spawn(async move { acc.accept(s).await.is_ok() });
        let r = TlsConnector::from(client_trusting(&trusted))
            .connect(ServerName::try_from(sni.to_string()).unwrap(), c)
            .await;
        assert_eq!(r.is_ok(), ok, "{sni}");
        assert_eq!(srv.await.unwrap(), ok, "{sni}");
    }
    // Only the allowed name got a certificate.
    assert_eq!(ca.cached(), 1);
}

/// Nothing here installs, trusts or modifies a trust store: the CA and the proxy never spawn a
/// process (the `security`/`update-ca-certificates` commands only appear in printed text), and
/// the only files written are inside the CA directory.
#[test]
fn nothing_touches_system_trust_stores() {
    for (name, src) in [
        ("ca.rs", include_str!("ca.rs")),
        ("proxy.rs", include_str!("proxy.rs")),
    ] {
        for needle in ["Command::new", "std::process", "tokio::process"] {
            assert!(!src.contains(needle), "{name} spawns a process: {needle}");
        }
    }
    // The printed instructions name the file and fingerprint; they are text, never executed.
    let t = trust_instructions(Path::new("/x/ca.pem"), "AA:BB");
    assert!(t.contains("/x/ca.pem") && t.contains("AA:BB"));
    assert!(t.contains("macOS") && t.contains("Debian") && t.contains("Firefox"));
    // Generating and issuing writes only inside the CA directory.
    let d = tmp();
    let before: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
    assert!(before.is_empty());
    let ca = LocalCa::load_or_create(&d.path().join("tls")).unwrap();
    ca.issue("v1.vibeke.localhost").unwrap();
    let names: Vec<_> = std::fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("tls")]);
}
