//! The per-user local CA behind `preview.tls_origin` (06 B4).
//!
//! Some apps need a secure context beyond what `http://*.localhost` gives (service workers over
//! a non-loopback-looking origin, Secure cookies in Safari, HTTPS-only APIs). With
//! `tls_origin` the proxy serves a preview at `https://<host>.vibeke.localhost:<port>` using a
//! short-lived leaf certificate for exactly that hostname, signed by a CA that lives in
//! Vibeke's state directory.
//!
//! Safety properties:
//! - The CA is **name-constrained** (X.509 `nameConstraints`, critical): permitted DNS subtree
//!   `vibeke.localhost` (the apex and every `*.vibeke.localhost`) and excluded IP subtrees
//!   `0.0.0.0/0` and `::/0`. Name constraints apply per name form (RFC 5280 §4.2.1.10), so the
//!   DNS subtree alone would leave IP-address certificates unrestricted; with the exclusions
//!   even a leaked key cannot make a trusted copy vouch for any other site or any IP address.
//!   [`LocalCa::issue`] additionally refuses to sign anything outside the DNS subtree.
//! - The CA key is `0600` in a `0700` directory, generated on first use, never leaves the
//!   machine and is never installed into a trust store by Vibeke: the user does that
//!   explicitly (`vibeke preview trust-ca`).
//! - Every file operation refuses symlinks (`O_NOFOLLOW` opens, `lstat` checks) for the
//!   directory, the lock, the key, the certificate and the metadata, requires them to be owned
//!   by this user, and writes by temp file + `rename` in the same directory: a planted link
//!   can neither redirect a write nor feed a foreign key in.
//! - Leaf certificates live [`LEAF_TTL`] (7 days), are created per hostname on first handshake,
//!   cached in memory only and re-issued when less than [`LEAF_RENEW_BEFORE`] is left.
//! - A long-running proxy holds a [`CaStore`], not a fixed CA: it re-checks the CA files'
//!   identity before every issuance and reloads (dropping its leaves) when another process
//!   renewed the CA, so the served chain always matches the CA file `trust-ca` installs.

use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, PublicKeyData, SanType,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
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
const LOCK_FILE: &str = ".lock";
/// Version of the CA's constraint set, recorded in the metadata. A stored CA of an older
/// version (before the IP exclusions) is replaced on load.
const CONSTRAINTS_VERSION: u32 = 2;

#[derive(Debug)]
pub enum CaError {
    /// The hostname is not `vibeke.localhost` or below (or is not a plain DNS name).
    OutsideConstraints(String),
    /// A CA path is a symlink, not a regular file/directory, or not owned by this user.
    Unsafe(String),
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
            CaError::Unsafe(e) => write!(f, "preview CA: refusing an unsafe path: {e}"),
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
        // Constraints are per name form: without these a leaked key could still sign
        // certificates for any IP address.
        excluded_subtrees: vec![
            GeneralSubtree::IpAddress(CidrSubnet::V4([0; 4], [0; 4])),
            GeneralSubtree::IpAddress(CidrSubnet::V6([0; 16], [0; 16])),
        ],
    });
    p.not_before = odt(not_before);
    p.not_after = odt(not_after);
    p
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Meta {
    not_after: u64,
    #[serde(default)]
    constraints: u32,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn unsafe_path(p: &Path, why: &str) -> CaError {
    CaError::Unsafe(format!("{}: {why}", p.display()))
}

fn nofollow_err(path: &Path, e: std::io::Error) -> CaError {
    match e.raw_os_error() {
        Some(libc::ELOOP) => unsafe_path(path, "is a symlink"),
        _ => CaError::Io(e),
    }
}

/// The CA directory: created `0700` when missing; an existing one must be a real directory
/// (never a symlink) owned by this user, and is tightened to `0700` through its own descriptor.
fn ensure_dir(dir: &Path) -> Result<(), CaError> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match std::fs::DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    let md = std::fs::symlink_metadata(dir)?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return Err(unsafe_path(
            dir,
            "not a plain directory (symlink or other file)",
        ));
    }
    if md.uid() != uid() {
        return Err(unsafe_path(dir, &format!("owned by uid {}", md.uid())));
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(dir)
        .map_err(|e| nofollow_err(dir, e))?;
    if f.metadata()?.permissions().mode() & 0o777 != 0o700 {
        f.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// `lstat` an entry of the CA directory: `None` when missing; a symlink, a non-regular file or
/// a file of another user is refused.
fn check_entry(path: &Path) -> Result<Option<std::fs::Metadata>, CaError> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(md) if md.file_type().is_symlink() => Err(unsafe_path(path, "is a symlink")),
        Ok(md) if !md.is_file() => Err(unsafe_path(path, "not a regular file")),
        Ok(md) if md.uid() != uid() => {
            Err(unsafe_path(path, &format!("owned by uid {}", md.uid())))
        }
        Ok(md) => Ok(Some(md)),
    }
}

/// Open an existing entry for reading without following a symlink, re-checking the opened file.
/// `Ok(None)` when it is missing.
fn open_existing(path: &Path) -> Result<Option<std::fs::File>, CaError> {
    if check_entry(path)?.is_none() {
        return Ok(None);
    }
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(nofollow_err(path, e)),
    };
    let md = f.metadata()?;
    if !md.is_file() || md.uid() != uid() {
        return Err(unsafe_path(path, "not a regular file of this user"));
    }
    Ok(Some(f))
}

fn read_entry(path: &Path) -> Result<Option<Vec<u8>>, CaError> {
    let Some(mut f) = open_existing(path)? else {
        return Ok(None);
    };
    let mut v = Vec::new();
    f.read_to_end(&mut v)?;
    Ok(Some(v))
}

/// Write `data` to `path` with `mode`: a fresh temp file in the same directory (`O_EXCL`,
/// `O_NOFOLLOW`, created with `mode`, never readable by others even briefly), then `rename`
/// over the target. A symlink at the target is refused, never followed.
fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<(), CaError> {
    check_entry(path)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let suffix: u64 = rand::random();
    let tmp = dir.join(format!(".{name}.{suffix:016x}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    let r = (|| -> std::io::Result<()> {
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Exclusive advisory lock on `<dir>/.lock` (blocking), released on drop. The lock file is
/// opened with `O_NOFOLLOW` and must be a regular file of this user.
fn lock_dir(dir: &Path) -> Result<std::fs::File, CaError> {
    use std::os::fd::AsRawFd;
    let path = dir.join(LOCK_FILE);
    check_entry(&path)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| nofollow_err(&path, e))?;
    let md = f.metadata()?;
    if !md.is_file() || md.uid() != uid() {
        return Err(unsafe_path(&path, "not a regular file of this user"));
    }
    // SAFETY: a valid open file descriptor; flock has no other preconditions.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
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
        ensure_dir(dir)?;
        // One creator at a time (the server and `vibeke preview trust-ca` can both be first):
        // an advisory lock held while loading/generating, released when `_lock` drops.
        let _lock = lock_dir(dir)?;
        if let Some(ca) = Self::load(dir, now)? {
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
        // Refuse before writing anything if any entry is unsafe.
        for f in [CA_KEY_FILE, CA_CERT_FILE, CA_META_FILE] {
            check_entry(&dir.join(f))?;
        }
        // Key first: a certificate without a key is ignored on load, never the reverse.
        write_atomic(
            &dir.join(CA_KEY_FILE),
            key.serialize_pem().as_bytes(),
            0o600,
        )?;
        write_atomic(&dir.join(CA_CERT_FILE), cert.pem().as_bytes(), 0o644)?;
        let meta = serde_json::to_vec(&Meta {
            not_after: unix(not_after),
            constraints: CONSTRAINTS_VERSION,
        })
        .map_err(crypto)?;
        write_atomic(&dir.join(CA_META_FILE), &meta, 0o600)?;
        Self::load(dir, now)?
            .map(Arc::new)
            .ok_or_else(|| CaError::Crypto("the generated CA could not be read back".to_string()))
    }

    /// `Ok(None)`: missing, inconsistent, outdated or expiring (replace it). `Err`: an unsafe
    /// entry (symlink, foreign owner): refused, nothing is replaced.
    fn load(dir: &Path, now: SystemTime) -> Result<Option<LocalCa>, CaError> {
        let key_path = dir.join(CA_KEY_FILE);
        // Every entry is checked (and an unsafe one refused) before any is used.
        let entries = [
            check_entry(&key_path)?,
            check_entry(&dir.join(CA_CERT_FILE))?,
            check_entry(&dir.join(CA_META_FILE))?,
        ];
        if entries.iter().any(Option::is_none) {
            return Ok(None);
        }
        let Some(key_pem) = Self::read_key(&key_path)? else {
            return Ok(None);
        };
        let Some(cert_pem) =
            read_entry(&dir.join(CA_CERT_FILE))?.and_then(|b| String::from_utf8(b).ok())
        else {
            return Ok(None);
        };
        let Some(meta) = read_entry(&dir.join(CA_META_FILE))?
            .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
        else {
            return Ok(None);
        };
        let not_after = UNIX_EPOCH + Duration::from_secs(meta.not_after);
        if now + CA_RENEW_BEFORE >= not_after || meta.constraints < CONSTRAINTS_VERSION {
            return Ok(None);
        }
        let Ok(key) = KeyPair::from_pem(&key_pem) else {
            return Ok(None);
        };
        let spki = key.subject_public_key_info();
        let Some(pem) = pem_der(&cert_pem) else {
            return Ok(None);
        };
        // The stored certificate must belong to the stored key.
        if !contains(&pem, &spki_key_bytes(&key)) {
            return Ok(None);
        }
        // Deterministic issuer parameters: same subject and key id as the stored certificate.
        let issuer = Issuer::new(ca_params(now, not_after), key);
        Ok(Some(LocalCa {
            dir: dir.to_path_buf(),
            issuer,
            cert_pem,
            cert_der: CertificateDer::from(pem),
            spki,
            not_after,
            cache: Mutex::default(),
        }))
    }

    /// The key (`O_NOFOLLOW`); a loosened mode is tightened through the open descriptor rather
    /// than trusted.
    fn read_key(path: &Path) -> Result<Option<String>, CaError> {
        let Some(mut f) = open_existing(path)? else {
            return Ok(None);
        };
        if f.metadata()?.permissions().mode() & 0o077 != 0 {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut s = String::new();
        Ok(f.read_to_string(&mut s).ok().map(|_| s))
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
        self.sign_sans(
            host,
            vec![SanType::DnsName(host.try_into().map_err(crypto)?)],
            now,
        )
    }

    /// Sign a leaf for arbitrary SANs with the CA key, bypassing every check of [`Self::issue`]
    /// (tests only: what a stolen key could produce).
    #[cfg(test)]
    pub(crate) fn sign_raw(
        &self,
        sans: Vec<SanType>,
        now: SystemTime,
    ) -> Result<Arc<CertifiedKey>, CaError> {
        self.sign_sans("raw", sans, now).map(|x| x.0)
    }

    fn sign_sans(
        &self,
        cn: &str,
        sans: Vec<SanType>,
        now: SystemTime,
    ) -> Result<(Arc<CertifiedKey>, SystemTime), CaError> {
        let key = KeyPair::generate().map_err(crypto)?;
        let not_after = now + LEAF_TTL;
        let mut p = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, cn);
        p.distinguished_name = dn;
        p.subject_alt_names = sans;
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

/// Identity of the CA's files (device, inode, size, mtime of the key, certificate and
/// metadata); `None` entries are missing files.
type FilesId = [Option<(u64, u64, u64, i64, i64)>; 3];

fn files_id(dir: &Path) -> FilesId {
    let one = |f: &str| {
        std::fs::symlink_metadata(dir.join(f))
            .ok()
            .map(|m| (m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
    };
    [one(CA_KEY_FILE), one(CA_CERT_FILE), one(CA_META_FILE)]
}

struct StoreState {
    ca: Arc<LocalCa>,
    id: FilesId,
}

/// The CA as a long-running process holds it: the loaded [`LocalCa`] keyed by the identity of
/// its files plus its fingerprint. [`CaStore::current`] re-checks the files before each use
/// (every leaf issuance) and reloads when they changed (another process renewed or replaced
/// the CA) or the CA is due for renewal; a reload with a different fingerprint starts a fresh
/// leaf cache, so leaves are re-issued by the new key and the served chain, the CA file and
/// the reported fingerprint always agree.
pub struct CaStore {
    dir: PathBuf,
    state: Mutex<StoreState>,
}

impl std::fmt::Debug for CaStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaStore").field("dir", &self.dir).finish()
    }
}

impl CaStore {
    /// Load (or generate) the CA in `dir`.
    pub fn open(dir: &Path) -> Result<Arc<CaStore>, CaError> {
        let ca = LocalCa::load_or_create(dir)?;
        Ok(Arc::new(CaStore {
            dir: dir.to_path_buf(),
            state: Mutex::new(StoreState {
                ca,
                id: files_id(dir),
            }),
        }))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The CA to sign with now (see the type docs).
    pub fn current(&self) -> Result<Arc<LocalCa>, CaError> {
        self.current_at(SystemTime::now())
    }

    pub fn current_at(&self, now: SystemTime) -> Result<Arc<LocalCa>, CaError> {
        let mut st = self.state.lock().unwrap();
        if files_id(&self.dir) == st.id && now + CA_RENEW_BEFORE < st.ca.not_after {
            return Ok(st.ca.clone());
        }
        let fresh = LocalCa::load_or_create_at(&self.dir, now)?;
        if fresh.fingerprint_sha256() != st.ca.fingerprint_sha256() {
            tracing::info!(
                old = %st.ca.fingerprint_sha256(),
                new = %fresh.fingerprint_sha256(),
                "preview CA changed on disk: reloaded, leaves will be re-issued"
            );
            st.ca = fresh;
        }
        st.id = files_id(&self.dir);
        Ok(st.ca.clone())
    }
}

/// Chooses the certificate by SNI at handshake time: only for names `allow` accepts (the
/// proxy: hostnames of registered `tls_origin` routes) and the CA permits. No SNI (an IP
/// literal) or any other name → no certificate, the handshake fails. The CA comes from a
/// [`CaStore`] on every handshake, so a renewed CA is picked up.
pub struct SniResolver {
    ca: Arc<CaStore>,
    allow: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl SniResolver {
    pub fn new(ca: Arc<CaStore>, allow: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
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
        match self.ca.current() {
            Ok(ca) => ca.issue(&name).ok(),
            Err(e) => {
                tracing::warn!(error = %e, "preview CA unavailable: TLS handshake refused");
                None
            }
        }
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
