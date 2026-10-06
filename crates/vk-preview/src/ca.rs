//! The per-user local CA behind `preview.tls_origin` (06 B4).
//!
//! Some apps need a secure context beyond what `http://*.localhost` gives (service workers over
//! a non-loopback-looking origin, Secure cookies in Safari, HTTPS-only APIs). With
//! `tls_origin` the proxy serves a preview at `https://<host>.vibeke.localhost:<port>` using a
//! short-lived leaf certificate for exactly that hostname, signed by a CA that lives in
//! Vibeke's state directory.
//!
//! Safety properties:
//! - The CA is **name-constrained** (X.509 `nameConstraints`, permitted DNS subtree
//!   `vibeke.localhost`, which covers the apex and every `*.vibeke.localhost`): even if its key
//!   leaked, a trusted copy cannot vouch for any other site. [`LocalCa::issue`] additionally
//!   refuses to sign anything outside that subtree.
//! - The CA key is `0600` in a `0700` directory, generated on first use, never leaves the
//!   machine and is never installed into a trust store by Vibeke: the user does that
//!   explicitly (`vibeke preview trust-ca`).
//! - Leaf certificates live [`LEAF_TTL`] (7 days), are created per hostname on first handshake,
//!   cached in memory only and re-issued when less than [`LEAF_RENEW_BEFORE`] is left.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose,
    GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints, PublicKeyData,
    SanType,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

/// The only DNS subtree the CA may sign for (the apex and everything below it).
pub const PERMITTED_DOMAIN: &str = crate::proxy::DOMAIN;
/// Leaf certificate lifetime.
pub const LEAF_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// A cached leaf with less than this left is replaced on the next handshake.
pub const LEAF_RENEW_BEFORE: Duration = Duration::from_secs(24 * 3600);
/// CA lifetime; a CA with less than [`CA_RENEW_BEFORE`] left is replaced (and must be trusted again).
pub const CA_TTL: Duration = Duration::from_secs(5 * 365 * 24 * 3600);
pub const CA_RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 3600);
const CA_COMMON_NAME: &str = "Vibeke local preview CA (vibeke.localhost only)";

const CA_CERT_FILE: &str = "preview-ca.pem";
const CA_KEY_FILE: &str = "preview-ca-key.pem";
const CA_META_FILE: &str = "preview-ca.json";

#[derive(Debug)]
pub enum CaError {
    /// The hostname is not `vibeke.localhost` or below (or is not a plain DNS name).
    OutsideConstraints(String),
    Io(std::io::Error),
    Crypto(String),
}

impl std::fmt::Display for CaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaError::OutsideConstraints(h) => write!(
                f,
                "the preview CA only signs for {PERMITTED_DOMAIN} and its subdomains, not {h:?}"
            ),
            CaError::Io(e) => write!(f, "preview CA: {e}"),
            CaError::Crypto(e) => write!(f, "preview CA: {e}"),
        }
    }
}

impl std::error::Error for CaError {}

impl From<std::io::Error> for CaError {
    fn from(e: std::io::Error) -> Self {
        CaError::Io(e)
    }
}

fn crypto(e: impl std::fmt::Display) -> CaError {
    CaError::Crypto(e.to_string())
}

/// A plain lowercase DNS name equal to, or below, [`PERMITTED_DOMAIN`] (no wildcard, no dots
/// at the ends, labels of `[a-z0-9-]` of 1–63 bytes not starting/ending with `-`).
pub fn host_permitted(host: &str) -> bool {
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if host.len() > 253 {
        return false;
    }
    if host == PERMITTED_DOMAIN {
        return true;
    }
    host.strip_suffix(PERMITTED_DOMAIN)
        .and_then(|r| r.strip_suffix('.'))
        .is_some_and(|sub| sub.split('.').all(label_ok))
}

fn odt(t: SystemTime) -> OffsetDateTime {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn ca_params(not_before: SystemTime, not_after: SystemTime) -> CertificateParams {
    let mut p = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_COMMON_NAME);
    dn.push(DnType::OrganizationName, "Vibeke");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: vec![GeneralSubtree::DnsName(PERMITTED_DOMAIN.to_string())],
        excluded_subtrees: vec![],
    });
    p.not_before = odt(not_before);
    p.not_after = odt(not_after);
    p
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    not_after: u64,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write `data` to `path` with mode `0600` (temp file + rename: never readable by others,
/// not even briefly).
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Exclusive advisory lock on `<dir>/.lock` (blocking), released on drop.
fn lock_dir(dir: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join(".lock"))?;
    // SAFETY: a valid open file descriptor; flock has no other preconditions.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(f)
}

struct Cached {
    key: Arc<CertifiedKey>,
    not_after: SystemTime,
}

/// The CA plus the in-memory leaf cache.
pub struct LocalCa {
    dir: PathBuf,
    issuer: Issuer<'static, KeyPair>,
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    spki: Vec<u8>,
    not_after: SystemTime,
    cache: Mutex<HashMap<String, Cached>>,
}

impl std::fmt::Debug for LocalCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalCa")
            .field("dir", &self.dir)
            .field("fingerprint", &self.fingerprint_sha256())
            .finish()
    }
}

impl LocalCa {
    /// Load the CA from `dir` (`<state>/tls`), generating it on first use (or when it is about
    /// to expire or its files are inconsistent). Creates `dir` with mode `0700`. Nothing is
    /// installed anywhere.
    pub fn load_or_create(dir: &Path) -> Result<Arc<LocalCa>, CaError> {
        Self::load_or_create_at(dir, SystemTime::now())
    }

    pub fn load_or_create_at(dir: &Path, now: SystemTime) -> Result<Arc<LocalCa>, CaError> {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        // One creator at a time (the server and `vibeke preview trust-ca` can both be first):
        // an advisory lock held while loading/generating, released when `_lock` drops.
        let _lock = lock_dir(dir)?;
        if let Some(ca) = Self::load(dir, now) {
            return Ok(Arc::new(ca));
        }
        Self::generate(dir, now)
    }

    fn generate(dir: &Path, now: SystemTime) -> Result<Arc<LocalCa>, CaError> {
        let key = KeyPair::generate().map_err(crypto)?;
        let not_before = now - Duration::from_secs(300);
        let not_after = now + CA_TTL;
        let params = ca_params(not_before, not_after);
        let cert = params.self_signed(&key).map_err(crypto)?;
        // Key first: a certificate without a key is ignored on load, never the reverse.
        write_private(&dir.join(CA_KEY_FILE), key.serialize_pem().as_bytes())?;
        std::fs::write(dir.join(CA_CERT_FILE), cert.pem())?;
        std::fs::set_permissions(
            dir.join(CA_CERT_FILE),
            std::fs::Permissions::from_mode(0o644),
        )?;
        let meta = serde_json::to_vec(&Meta {
            not_after: unix(not_after),
        })
        .map_err(crypto)?;
        std::fs::write(dir.join(CA_META_FILE), meta)?;
        Self::load(dir, now)
            .map(Arc::new)
            .ok_or_else(|| CaError::Crypto("the generated CA could not be read back".to_string()))
    }

    fn load(dir: &Path, now: SystemTime) -> Option<LocalCa> {
        let key_pem = std::fs::read_to_string(dir.join(CA_KEY_FILE)).ok()?;
        let cert_pem = std::fs::read_to_string(dir.join(CA_CERT_FILE)).ok()?;
        let meta: Meta =
            serde_json::from_slice(&std::fs::read(dir.join(CA_META_FILE)).ok()?).ok()?;
        let not_after = UNIX_EPOCH + Duration::from_secs(meta.not_after);
        if now + CA_RENEW_BEFORE >= not_after {
            return None;
        }
        // The key file must stay private; tighten a loosened mode rather than trust it.
        let mode = std::fs::metadata(dir.join(CA_KEY_FILE))
            .ok()?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(
                dir.join(CA_KEY_FILE),
                std::fs::Permissions::from_mode(0o600),
            )
            .ok()?;
        }
        let key = KeyPair::from_pem(&key_pem).ok()?;
        let spki = key.subject_public_key_info();
        let pem = pem_der(&cert_pem)?;
        // The stored certificate must belong to the stored key.
        if !contains(&pem, &spki_key_bytes(&key)) {
            return None;
        }
        // Deterministic issuer parameters: same subject and key id as the stored certificate.
        let issuer = Issuer::new(ca_params(now, not_after), key);
        Some(LocalCa {
            dir: dir.to_path_buf(),
            issuer,
            cert_pem,
            cert_der: CertificateDer::from(pem),
            spki,
            not_after,
            cache: Mutex::default(),
        })
    }

    /// `<state>/tls/preview-ca.pem`: the file the user imports into a trust store.
    pub fn ca_path(&self) -> PathBuf {
        self.dir.join(CA_CERT_FILE)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }

    pub fn not_after(&self) -> SystemTime {
        self.not_after
    }

    /// SHA-256 of the CA certificate, `AB:CD:…` (what trust dialogs show).
    pub fn fingerprint_sha256(&self) -> String {
        Sha256::digest(self.cert_der.as_ref())
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Base64 SHA-256 of the CA's SubjectPublicKeyInfo: the value Chromium's
    /// `--ignore-certificate-errors-spki-list` takes (browser tests trust the CA this way, never
    /// through the system store).
    pub fn spki_sha256_base64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&self.spki))
    }

    /// A certificate for `host` (cached; re-issued when it has less than
    /// [`LEAF_RENEW_BEFORE`] left). Refuses names outside [`PERMITTED_DOMAIN`].
    pub fn issue(&self, host: &str) -> Result<Arc<CertifiedKey>, CaError> {
        self.issue_at(host, SystemTime::now())
    }

    pub fn issue_at(&self, host: &str, now: SystemTime) -> Result<Arc<CertifiedKey>, CaError> {
        if !host_permitted(host) {
            return Err(CaError::OutsideConstraints(host.to_string()));
        }
        let mut cache = self.cache.lock().unwrap();
        if let Some(c) = cache.get(host)
            && c.not_after > now + LEAF_RENEW_BEFORE
        {
            return Ok(c.key.clone());
        }
        let (key, not_after) = self.sign_leaf(host, now)?;
        cache.insert(
            host.to_string(),
            Cached {
                key: key.clone(),
                not_after,
            },
        );
        Ok(key)
    }

    /// Sign without the permitted-subtree check. Tests only: proves a client enforces the CA's
    /// name constraints even if a bug (or a stolen key) signed something outside them.
    #[cfg(test)]
    pub(crate) fn issue_unchecked(
        &self,
        host: &str,
        now: SystemTime,
    ) -> Result<Arc<CertifiedKey>, CaError> {
        self.sign_leaf(host, now).map(|x| x.0)
    }

    fn sign_leaf(
        &self,
        host: &str,
        now: SystemTime,
    ) -> Result<(Arc<CertifiedKey>, SystemTime), CaError> {
        let key = KeyPair::generate().map_err(crypto)?;
        let not_after = now + LEAF_TTL;
        let mut p = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host);
        p.distinguished_name = dn;
        p.subject_alt_names = vec![SanType::DnsName(host.try_into().map_err(crypto)?)];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.not_before = odt(now - Duration::from_secs(300));
        p.not_after = odt(not_after);
        p.use_authority_key_identifier_extension = true;
        let cert = p.signed_by(&key, &self.issuer).map_err(crypto)?;
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = rustls::crypto::ring::sign::any_supported_type(&der).map_err(crypto)?;
        Ok((
            Arc::new(CertifiedKey::new(
                // Leaf + CA: some clients (Chromium's SPKI pins) look for the CA in the served chain.
                vec![cert.der().clone(), self.cert_der.clone()],
                signing,
            )),
            not_after,
        ))
    }

    /// Number of cached leaves (tests, status).
    pub fn cached(&self) -> usize {
        self.cache.lock().unwrap().len()
    }
}

fn pem_der(pem: &str) -> Option<Vec<u8>> {
    let mut b64 = String::new();
    let mut inside = false;
    for l in pem.lines() {
        if l.starts_with("-----BEGIN CERTIFICATE") {
            inside = true;
        } else if l.starts_with("-----END CERTIFICATE") {
            break;
        } else if inside {
            b64.push_str(l.trim());
        }
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

fn spki_key_bytes(key: &KeyPair) -> Vec<u8> {
    key.der_bytes().to_vec()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Chooses the certificate by SNI at handshake time: only for names `allow` accepts (the
/// proxy: hostnames of registered `tls_origin` routes) and the CA permits. No SNI (an IP
/// literal) or any other name → no certificate, the handshake fails.
pub struct SniResolver {
    ca: Arc<LocalCa>,
    allow: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl SniResolver {
    pub fn new(ca: Arc<LocalCa>, allow: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        SniResolver {
            ca,
            allow: Box::new(allow),
        }
    }
}

impl std::fmt::Debug for SniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SniResolver")
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        if !(self.allow)(&name) {
            return None;
        }
        self.ca.issue(&name).ok()
    }
}

/// The proxy's server config: ring, TLS 1.2/1.3, ALPN `http/1.1`, no client auth.
pub fn server_config(resolver: Arc<dyn ResolvesServerCert>) -> Arc<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// A TLS client config that trusts exactly the given CA certificates (no system or bundled
/// roots), ALPN `http/1.1`. For tests and the browser test harness; Vibeke's own code never
/// needs to trust the CA.
pub fn client_config_trusting(cas: &[CertificateDer<'static>]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in cas {
        let _ = roots.add(c.clone());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// Per-OS instructions for trusting the CA, as printed by `vibeke preview trust-ca`. Never
/// executed here.
pub fn trust_instructions(ca_path: &Path, fingerprint: &str) -> String {
    let p = ca_path.display();
    format!(
        "Vibeke preview CA (name-constrained to {PERMITTED_DOMAIN} and *.{PERMITTED_DOMAIN})\n\
         file:        {p}\n\
         SHA-256:     {fingerprint}\n\n\
         Vibeke never changes a trust store by itself. To trust this CA yourself:\n\n\
         macOS (your login keychain, user trust only; the system asks you to authorise):\n  \
           security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db \"{p}\"\n  \
         or run `vibeke preview trust-ca --install` (asks you to type a confirmation first).\n  \
         Remove: security delete-certificate -c \"Vibeke local preview CA\" ~/Library/Keychains/login.keychain-db\n\n\
         Debian/Ubuntu:\n  \
           sudo cp \"{p}\" /usr/local/share/ca-certificates/vibeke-preview-ca.crt && sudo update-ca-certificates\n  \
         Remove: sudo rm /usr/local/share/ca-certificates/vibeke-preview-ca.crt && sudo update-ca-certificates --fresh\n\n\
         Fedora/RHEL:\n  \
           sudo cp \"{p}\" /etc/pki/ca-trust/source/anchors/vibeke-preview-ca.pem && sudo update-ca-trust\n  \
         Remove: sudo rm /etc/pki/ca-trust/source/anchors/vibeke-preview-ca.pem && sudo update-ca-trust\n\n\
         Arch: sudo trust anchor --store \"{p}\"\n\n\
         Firefox (own store): Settings > Privacy & Security > Certificates > View Certificates >\n  \
           Authorities > Import, choose the file, tick \"Trust this CA to identify websites\".\n\n\
         Chrome/Chromium on Linux (NSS): certutil -d sql:$HOME/.pki/nssdb -A -t \"C,,\" -n vibeke-preview-ca -i \"{p}\"\n\n\
         Because of the name constraint, a trusted copy cannot vouch for any site outside\n\
         {PERMITTED_DOMAIN}. Check the SHA-256 above in the trust dialog before accepting.\n"
    )
}

#[cfg(test)]
#[path = "ca_tests.rs"]
pub(crate) mod tests;
