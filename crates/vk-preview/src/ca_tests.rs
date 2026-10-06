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
    // Excluded subtrees [1] with iPAddress [7] 0.0.0.0/0 (8 zero bytes) and ::/0 (32).
    let v4 = [&[0x30u8, 0x0a, 0x87, 0x08][..], &[0u8; 8]].concat();
    let v6 = [&[0x30u8, 0x22, 0x87, 0x20][..], &[0u8; 32]].concat();
    assert!(has(&v4), "no excluded 0.0.0.0/0");
    assert!(has(&v6), "no excluded ::/0");
}

fn ip_leaf(ca: &LocalCa, ip: std::net::IpAddr) -> Arc<CertifiedKey> {
    ca.sign_raw(vec![SanType::IpAddress(ip)], SystemTime::now())
        .unwrap()
}

/// Name constraints apply per name form: the DNS subtree alone leaves IP SANs unrestricted.
/// Leaves for IPv4/IPv6 addresses signed directly with the CA key (outside `issue`, as a
/// stolen key would) fail webpki/rustls verification because of the excluded IP subtrees.
#[tokio::test]
async fn ip_address_leaves_signed_with_the_key_are_rejected_by_rustls() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    let trusted = [ca.cert_der().clone()];
    for ip in ["192.0.2.1", "127.0.0.1", "10.0.0.1", "::1", "2001:db8::1"] {
        let ip: std::net::IpAddr = ip.parse().unwrap();
        let (c, srv) = tokio::io::duplex(64 * 1024);
        let acceptor = TlsAcceptor::from(server_config(Arc::new(Fixed(ip_leaf(&ca, ip)))));
        let server = tokio::spawn(async move { acceptor.accept(srv).await.map(|_| ()) });
        let name = ServerName::IpAddress(ip.into());
        let r = TlsConnector::from(client_trusting(&trusted))
            .connect(name, c)
            .await;
        let _ = server.await;
        let e = r
            .err()
            .unwrap_or_else(|| panic!("{ip}: accepted"))
            .to_string();
        assert!(e.to_lowercase().contains("certificate"), "{ip}: {e}");
    }
    // Control: the same CA's DNS leaf inside the subtree verifies.
    let host = "v1.vibeke.localhost";
    handshake(&trusted, ca.issue(host).unwrap(), host)
        .await
        .unwrap();
}

/// The same check with OpenSSL's verifier, when `openssl` is installed (skipped otherwise).
#[test]
fn ip_address_leaves_fail_openssl_verify_too() {
    let Ok(out) = std::process::Command::new("openssl")
        .arg("version")
        .output()
    else {
        eprintln!("openssl not installed; skipping");
        return;
    };
    if !out.status.success() {
        eprintln!("openssl not usable; skipping");
        return;
    }
    let d = tmp();
    let ca = LocalCa::load_or_create(&d.path().join("tls")).unwrap();
    let pem = |der: &[u8]| {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD.encode(der);
        let lines: Vec<&str> = b
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!(
            "-----BEGIN CERTIFICATE-----
{}
-----END CERTIFICATE-----
",
            lines.join(
                "
"
            )
        )
    };
    let verify = |name: &str, leaf: &Arc<CertifiedKey>, check: &[&str]| -> bool {
        let f = d.path().join(format!("{name}.pem"));
        std::fs::write(&f, pem(leaf.cert[0].as_ref())).unwrap();
        let out = std::process::Command::new("openssl")
            .arg("verify")
            .arg("-CAfile")
            .arg(ca.ca_path())
            .args(check)
            .arg(&f)
            .output()
            .unwrap();
        out.status.success()
    };
    let dns = ca.issue("v1.vibeke.localhost").unwrap();
    assert!(
        verify("dns", &dns, &[]),
        "control: an issued DNS leaf verifies"
    );
    for ip in ["192.0.2.1", "2001:db8::1"] {
        let leaf = ip_leaf(&ca, ip.parse().unwrap());
        assert!(!verify("ip", &leaf, &[]), "{ip}: openssl accepted it");
        assert!(
            !verify("ip", &leaf, &["-verify_ip", ip]),
            "{ip}: openssl accepted it"
        );
    }
    let foreign = ca
        .issue_unchecked("evil.example.com", SystemTime::now())
        .unwrap();
    assert!(!verify("foreign", &foreign, &[]));
}

/// A CA generated before the IP exclusions existed (metadata without `constraints`) is
/// replaced, never kept.
#[test]
fn a_ca_without_the_ip_exclusions_is_replaced() {
    let d = tmp();
    let ca = LocalCa::load_or_create(d.path()).unwrap();
    let fp = ca.fingerprint_sha256();
    let meta = serde_json::json!({"not_after": unix(ca.not_after())}).to_string();
    std::fs::write(d.path().join(CA_META_FILE), meta).unwrap();
    let fresh = LocalCa::load_or_create(d.path()).unwrap();
    assert_ne!(fresh.fingerprint_sha256(), fp);
}

fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Every CA path refuses symlinks: the directory, the lock, the key, the certificate and the
/// metadata. The load is refused (never "regenerated" over the link) and the external target
/// is left exactly as it was.
#[test]
fn symlinked_ca_paths_are_refused_and_external_targets_untouched() {
    for entry in [LOCK_FILE, CA_KEY_FILE, CA_CERT_FILE, CA_META_FILE] {
        for populated in [false, true] {
            let d = tmp();
            let dir = d.path().join("tls");
            if populated {
                LocalCa::load_or_create(&dir).unwrap();
            } else {
                std::fs::create_dir(&dir).unwrap();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let outside = d.path().join("outside");
            std::fs::write(&outside, b"precious").unwrap();
            std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o640)).unwrap();
            let link = dir.join(entry);
            let _ = std::fs::remove_file(&link);
            symlink(&outside, &link);
            let r = LocalCa::load_or_create(&dir);
            assert!(
                matches!(r, Err(CaError::Unsafe(_))),
                "{entry} (populated {populated}): {r:?}"
            );
            // A store opened earlier refuses too (and keeps serving nothing new).
            assert!(
                matches!(CaStore::open(&dir), Err(CaError::Unsafe(_))),
                "{entry}"
            );
            assert_eq!(std::fs::read(&outside).unwrap(), b"precious", "{entry}");
            assert_eq!(mode(&outside), 0o640, "{entry}: target chmodded");
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{entry}: the link was replaced"
            );
        }
    }
    // A symlinked CA directory (to a real directory elsewhere).
    let d = tmp();
    let elsewhere = d.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dir = d.path().join("tls");
    symlink(&elsewhere, &dir);
    assert!(matches!(
        LocalCa::load_or_create(&dir),
        Err(CaError::Unsafe(_))
    ));
    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing written"
    );
    assert_eq!(mode(&elsewhere), 0o755, "not chmodded");
    // A key that is a hard link elsewhere is still a regular file of ours: loading works, but
    // a renewal replaces the directory entry by rename and never writes through the link.
    let d = tmp();
    let dir = d.path().join("tls");
    let ca = LocalCa::load_or_create_at(&dir, SystemTime::now()).unwrap();
    let keep = d.path().join("key-copy");
    std::fs::hard_link(dir.join(CA_KEY_FILE), &keep).unwrap();
    let before = std::fs::read(&keep).unwrap();
    let renewed = LocalCa::load_or_create_at(
        &dir,
        SystemTime::now() + CA_TTL - Duration::from_secs(86400),
    )
    .unwrap();
    assert_ne!(renewed.fingerprint_sha256(), ca.fingerprint_sha256());
    assert_eq!(
        std::fs::read(&keep).unwrap(),
        before,
        "hard-linked target rewritten"
    );
}

/// Files are created with their final modes and written by temp + rename (no temp files
/// left behind, no loosened modes).
#[test]
fn writes_are_atomic_with_final_modes() {
    let d = tmp();
    let dir = d.path().join("tls");
    LocalCa::load_or_create(&dir).unwrap();
    assert_eq!(mode(&dir.join(CA_KEY_FILE)), 0o600);
    assert_eq!(mode(&dir.join(CA_CERT_FILE)), 0o644);
    assert_eq!(mode(&dir.join(CA_META_FILE)), 0o600);
    assert_eq!(mode(&dir.join(LOCK_FILE)), 0o600);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    // An existing directory with a loose mode is tightened.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    LocalCa::load_or_create(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
}

/// A `CaStore` keeps its CA (and leaf cache) while the files are unchanged, and reloads with a
/// fresh leaf cache when another process renewed the CA.
#[test]
fn the_store_reloads_a_ca_renewed_elsewhere() {
    let d = tmp();
    let store = CaStore::open(d.path()).unwrap();
    let a = store.current().unwrap();
    let leaf = a.issue("v1.vibeke.localhost").unwrap();
    assert!(
        Arc::ptr_eq(&a, &store.current().unwrap()),
        "unchanged: same CA"
    );
    // Touching nothing but reading: still the same.
    let _ = LocalCa::load_or_create(d.path()).unwrap();
    assert!(Arc::ptr_eq(&a, &store.current().unwrap()));
    // Another process renews.
    let renewed = LocalCa::load_or_create_at(
        d.path(),
        SystemTime::now() + CA_TTL - Duration::from_secs(86400),
    )
    .unwrap();
    let b = store.current().unwrap();
    assert_eq!(b.fingerprint_sha256(), renewed.fingerprint_sha256());
    assert_ne!(b.fingerprint_sha256(), a.fingerprint_sha256());
    assert_eq!(b.cached(), 0, "leaves are re-issued by the new key");
    let leaf2 = b.issue("v1.vibeke.localhost").unwrap();
    assert_ne!(leaf.cert[0].as_ref(), leaf2.cert[0].as_ref());
    assert_eq!(leaf2.cert[1].as_ref(), renewed.cert_der().as_ref());
    // Due for renewal by the clock (nobody else renewed): the store renews it itself.
    let late = store
        .current_at(SystemTime::now() + 2 * CA_TTL - Duration::from_secs(86400))
        .unwrap();
    assert_ne!(late.fingerprint_sha256(), b.fingerprint_sha256());
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
    let store = CaStore::open(d.path()).unwrap();
    let ca = store.current().unwrap();
    let allowed = "v1-s0123456789.vibeke.localhost";
    let r = Arc::new(SniResolver::new(store.clone(), move |h| h == allowed));
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
