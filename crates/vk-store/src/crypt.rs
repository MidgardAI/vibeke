//! Optional at-rest encryption of operational data (09 §9.1 `security.encrypt_state`): scrollback
//! segments and session blobs are sealed with AES-256-GCM under a key kept in the OS keychain
//! ([`crate::keychain`]). It protects backups and disk images, not against processes running as
//! the same user (they can ask the keychain too).
//!
//! Format: a sealed file is a sequence of records, each
//! `"VKE1" | key id (8) | nonce (12) | ciphertext length (u32 LE) | ciphertext+tag`, with the
//! first 12 bytes as associated data. A blob is one record; a scrollback segment gets one record
//! per flush (each holding one zstd frame), so appending never rewrites earlier data. Plain
//! files (no magic) are read as before, so turning encryption on or off never strands data:
//! files keep the mode they were created with until `security.encryption.migrate` rewrites them.
//!
//! The key id is the first 8 bytes of blake3(key). Readers find keys in a process-wide registry
//! ([`register`]), filled by whoever unlocked the key ([`unlock`]); writers hold their cipher
//! explicitly. `<state>/encryption.json` records which key and keychain item a state dir uses
//! (never the key).

use crate::keychain::{Keychain, KeychainError};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

pub const MAGIC: &[u8; 4] = b"VKE1";
const AAD_LEN: usize = 4 + 8;
const HEADER: usize = AAD_LEN + 12 + 4;
const TAG: usize = 16;
/// Bytes a record adds to its plaintext.
pub const OVERHEAD: usize = HEADER + TAG;
/// Largest record accepted when reading (a corrupt length must not allocate gigabytes).
const MAX_RECORD: usize = 256 << 20;

/// The marker file in a session state dir.
pub const MARKER: &str = "encryption.json";

pub struct StateCipher {
    aead: Aes256Gcm,
    id: [u8; 8],
}

impl std::fmt::Debug for StateCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StateCipher({})", self.id_hex())
    }
}

fn key_id(key: &[u8; 32]) -> [u8; 8] {
    let h = blake3::hash(key);
    let mut id = [0u8; 8];
    id.copy_from_slice(&h.as_bytes()[..8]);
    id
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl StateCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        StateCipher {
            aead: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
            id: key_id(key),
        }
    }

    /// A fresh random key, returned with its hex form for the keychain.
    pub fn generate() -> (Self, String) {
        use rand::RngCore;
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let h = hex(&key);
        let c = StateCipher::new(&key);
        key.fill(0);
        (c, h)
    }

    /// Parse a 64-hex-character key (as stored in the keychain).
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.len() != 64 {
            return None;
        }
        let mut key = [0u8; 32];
        for (i, k) in key.iter_mut().enumerate() {
            *k = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        let c = StateCipher::new(&key);
        key.fill(0);
        Some(c)
    }

    pub fn id(&self) -> [u8; 8] {
        self.id
    }

    pub fn id_hex(&self) -> String {
        hex(&self.id)
    }

    /// Seal `plain` into one record.
    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        use rand::RngCore;
        let mut nonce = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let mut out = Vec::with_capacity(plain.len() + OVERHEAD);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.id);
        let ct = self
            .aead
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: &out[..AAD_LEN],
                },
            )
            .expect("AES-GCM encryption of an in-memory buffer cannot fail");
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&(ct.len() as u32).to_le_bytes());
        out.extend_from_slice(&ct);
        out
    }

    fn open_record(&self, aad: &[u8], nonce: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
        self.aead
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
            .ok()
    }
}

static REGISTRY: LazyLock<RwLock<HashMap<[u8; 8], Arc<StateCipher>>>> =
    LazyLock::new(Default::default);

/// Make a key available to readers in this process (idempotent).
pub fn register(c: Arc<StateCipher>) {
    REGISTRY.write().unwrap().insert(c.id, c);
}

pub fn registered(id: &[u8; 8]) -> Option<Arc<StateCipher>> {
    REGISTRY.read().unwrap().get(id).cloned()
}

/// Does this data start a sealed record stream?
pub fn is_sealed(data: &[u8]) -> bool {
    data.starts_with(MAGIC)
}

/// Is the file at `p` sealed? (`false` for a missing or short file.)
pub fn file_is_sealed(p: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 4];
    std::fs::File::open(p)
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok()
        && &b == MAGIC
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// Sealed with a key that is not unlocked in this process (hex key id).
    MissingKey(String),
    /// A record failed authentication or is truncated.
    Corrupt,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::MissingKey(id) => write!(
                f,
                "encrypted with state key {id}, which is not unlocked (check [security] keychain)"
            ),
            OpenError::Corrupt => write!(f, "encrypted data is damaged"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<OpenError> for std::io::Error {
    fn from(e: OpenError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}

/// What [`open_lossy`] recovered.
#[derive(Debug, Default)]
pub struct Opened {
    pub data: Vec<u8>,
    /// Records decrypted.
    pub records: usize,
    /// A record after the good ones was damaged or truncated; `data` holds what came before.
    pub damaged: bool,
}

/// Decrypt a record stream, stopping at the first damaged record. Plain data is returned as it
/// is. A record sealed with an unknown key is an error, never silently skipped.
pub fn open_lossy(data: &[u8]) -> Result<Opened, OpenError> {
    if !is_sealed(data) {
        return Ok(Opened {
            data: data.to_vec(),
            records: 0,
            damaged: false,
        });
    }
    let mut out = Opened::default();
    let mut at = 0;
    while at < data.len() {
        let rest = &data[at..];
        if rest.len() < HEADER || !rest.starts_with(MAGIC) {
            out.damaged = true;
            break;
        }
        let mut id = [0u8; 8];
        id.copy_from_slice(&rest[4..12]);
        let len = u32::from_le_bytes(rest[24..28].try_into().unwrap()) as usize;
        if !(TAG..=MAX_RECORD).contains(&len) || rest.len() < HEADER + len {
            out.damaged = true;
            break;
        }
        let Some(c) = registered(&id) else {
            return Err(OpenError::MissingKey(hex(&id)));
        };
        match c.open_record(&rest[..AAD_LEN], &rest[12..24], &rest[HEADER..HEADER + len]) {
            Some(p) => out.data.extend_from_slice(&p),
            None => {
                out.damaged = true;
                break;
            }
        }
        out.records += 1;
        at += HEADER + len;
    }
    Ok(out)
}

/// Decrypt a record stream; any damage is an error. Plain data is returned as it is.
pub fn open_all(data: &[u8]) -> Result<Vec<u8>, OpenError> {
    let o = open_lossy(data)?;
    if o.damaged {
        return Err(OpenError::Corrupt);
    }
    Ok(o.data)
}

/// Read a possibly sealed file and return its plaintext.
pub fn read_file(p: &Path) -> std::io::Result<Vec<u8>> {
    let raw = std::fs::read(p)?;
    if !is_sealed(&raw) {
        return Ok(raw);
    }
    Ok(open_all(&raw)?)
}

/// Plaintext length of a possibly sealed file, from the record headers (no decryption).
pub fn plain_len(p: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let total = f.metadata()?.len();
    let mut head = [0u8; HEADER];
    if total < HEADER as u64 || f.read_exact(&mut head[..4]).is_err() || &head[..4] != MAGIC {
        return Ok(total);
    }
    f.seek(SeekFrom::Start(0))?;
    let mut at = 0u64;
    let mut plain = 0u64;
    while at + HEADER as u64 <= total {
        f.seek(SeekFrom::Start(at))?;
        f.read_exact(&mut head)?;
        let len = u32::from_le_bytes(head[24..28].try_into().unwrap()) as u64;
        if len < TAG as u64 {
            break;
        }
        plain += len - TAG as u64;
        at += HEADER as u64 + len;
    }
    Ok(plain)
}

/// Write `data` to `p` (0600) via a temp file and rename, sealed when `cipher` is given.
pub fn write_file(p: &Path, data: &[u8], cipher: Option<&StateCipher>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let bytes;
    let body: &[u8] = match cipher {
        Some(c) => {
            bytes = c.seal(data);
            &bytes
        }
        None => data,
    };
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = p.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(body)?;
    f.sync_all()?;
    std::fs::rename(&tmp, p)
}

/// `<state>/encryption.json`: which key a state dir's sealed files use. No secret inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub key_id: String,
    /// `[security] keychain` setting the key was stored with.
    pub keychain: String,
    pub service: String,
    pub account: String,
    pub created_at_ms: i64,
}

pub fn read_marker(state_dir: &Path) -> Option<Marker> {
    std::fs::read(state_dir.join(MARKER))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_marker(state_dir: &Path, m: &Marker) -> std::io::Result<()> {
    write_file(
        &state_dir.join(MARKER),
        &serde_json::to_vec_pretty(m).unwrap_or_default(),
        None,
    )
}

/// The keychain account of a state dir's key: one key per session state dir.
pub fn account_for(state_dir: &Path) -> String {
    let canon = std::fs::canonicalize(state_dir).unwrap_or_else(|_| state_dir.to_path_buf());
    let h = blake3::hash(canon.to_string_lossy().as_bytes()).to_hex();
    format!("state-key-{}", &h[..16])
}

#[derive(Debug)]
pub enum UnlockError {
    Keychain(KeychainError),
    /// The keychain item exists but is not a key, or its id differs from the marker's.
    WrongKey(String),
    Io(std::io::Error),
}

impl std::fmt::Display for UnlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnlockError::Keychain(e) => write!(f, "{e}"),
            UnlockError::WrongKey(m) => write!(f, "state key: {m}"),
            UnlockError::Io(e) => write!(f, "state key marker: {e}"),
        }
    }
}

impl std::error::Error for UnlockError {}

/// Outcome of [`unlock`].
#[derive(Debug)]
pub struct Unlocked {
    pub cipher: Arc<StateCipher>,
    pub marker: Marker,
    /// A new key was generated and stored.
    pub created: bool,
}

/// Load the state dir's key from `keychain` (the marker's keychain item when a marker exists),
/// registering it for readers. With `create` and no key yet, a new key is generated, stored in
/// the keychain and recorded in the marker. `Ok(None)`: no marker and `create` is false.
pub fn unlock(
    state_dir: &Path,
    keychain: &Keychain,
    create: bool,
) -> Result<Option<Unlocked>, UnlockError> {
    let marker = read_marker(state_dir);
    if marker.is_none() && !create {
        return Ok(None);
    }
    let (service, account) = match &marker {
        Some(m) => (m.service.clone(), m.account.clone()),
        None => (crate::keychain::SERVICE.to_string(), account_for(state_dir)),
    };
    let stored = keychain
        .get(&service, &account)
        .map_err(UnlockError::Keychain)?;
    if let Some(hexkey) = stored {
        let c = StateCipher::from_hex(&hexkey)
            .ok_or_else(|| UnlockError::WrongKey("the keychain item is not a state key".into()))?;
        if let Some(m) = &marker
            && m.key_id != c.id_hex()
        {
            return Err(UnlockError::WrongKey(format!(
                "the keychain holds key {} but this state dir was sealed with {}",
                c.id_hex(),
                m.key_id
            )));
        }
        let c = Arc::new(c);
        register(c.clone());
        let marker = match marker {
            Some(m) => m,
            None => {
                let m = Marker {
                    key_id: c.id_hex(),
                    keychain: keychain.setting(),
                    service,
                    account,
                    created_at_ms: crate::now_ms(),
                };
                write_marker(state_dir, &m).map_err(UnlockError::Io)?;
                m
            }
        };
        return Ok(Some(Unlocked {
            cipher: c,
            marker,
            created: false,
        }));
    }
    if let Some(m) = &marker {
        return Err(UnlockError::WrongKey(format!(
            "state key {} is missing from the keychain ({}/{})",
            m.key_id, m.service, m.account
        )));
    }
    let (c, hexkey) = StateCipher::generate();
    keychain
        .set(&service, &account, &hexkey)
        .map_err(UnlockError::Keychain)?;
    let c = Arc::new(c);
    register(c.clone());
    let m = Marker {
        key_id: c.id_hex(),
        keychain: keychain.setting(),
        service,
        account,
        created_at_ms: crate::now_ms(),
    };
    std::fs::create_dir_all(state_dir).map_err(UnlockError::Io)?;
    write_marker(state_dir, &m).map_err(UnlockError::Io)?;
    Ok(Some(Unlocked {
        cipher: c,
        marker: m,
        created: true,
    }))
}

/// Offline readers (doctor, debug bundle): register the state dir's key if it has a marker,
/// using the keychain recorded in the marker. Errors are returned for reporting.
pub fn unlock_for_reading(state_dir: &Path) -> Result<Option<Arc<StateCipher>>, UnlockError> {
    let Some(m) = read_marker(state_dir) else {
        return Ok(None);
    };
    let kc = Keychain::from_setting(&m.keychain)
        .map_err(|e| UnlockError::Keychain(KeychainError::Invalid(e)))?;
    Ok(unlock(state_dir, &kc, false)?.map(|u| u.cipher))
}

/// Files of a state dir that hold sealable operational data: scrollback segments and session
/// blobs (not their `.json` sidecars, temp files or the purge staging area).
pub fn sealable_files(state_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut walk = |root: PathBuf, want: &dyn Fn(&Path) -> bool| {
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                match e.file_type() {
                    Ok(t) if t.is_dir() => stack.push(p),
                    Ok(t) if t.is_file() && want(&p) => out.push(p),
                    _ => {}
                }
            }
        }
    };
    walk(state_dir.join("scrollback"), &|p| {
        p.extension().is_some_and(|x| x == "zst")
    });
    walk(state_dir.join("blobs"), &|p| {
        !p.extension().is_some_and(|x| x == "json")
    });
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> Arc<StateCipher> {
        let (c, _) = StateCipher::generate();
        let c = Arc::new(c);
        register(c.clone());
        c
    }

    #[test]
    fn seal_open_roundtrip_and_tamper() {
        let c = cipher();
        let mut s = c.seal(b"hello");
        assert!(is_sealed(&s));
        assert_eq!(s.len(), 5 + OVERHEAD);
        s.extend(c.seal(b" world"));
        assert_eq!(open_all(&s).unwrap(), b"hello world");
        // Plain data passes through.
        assert_eq!(open_all(b"plain").unwrap(), b"plain");
        // A flipped ciphertext byte in the second record: the first survives a lossy read.
        let n = s.len();
        s[n - 3] ^= 1;
        let o = open_lossy(&s).unwrap();
        assert_eq!(o.data, b"hello");
        assert!(o.damaged);
        assert_eq!(open_all(&s).unwrap_err(), OpenError::Corrupt);
        // Truncated tail.
        let t = c.seal(b"abc");
        let o = open_lossy(&t[..t.len() - 1]).unwrap();
        assert!(o.damaged && o.data.is_empty());
    }

    #[test]
    fn unknown_key_is_an_error_not_a_skip() {
        let (c, _) = StateCipher::generate();
        let s = c.seal(b"x");
        assert!(matches!(open_lossy(&s), Err(OpenError::MissingKey(_))));
    }

    #[test]
    fn hex_roundtrip_keeps_id() {
        let (c, h) = StateCipher::generate();
        let d = StateCipher::from_hex(&h).unwrap();
        assert_eq!(c.id(), d.id());
        assert!(StateCipher::from_hex("zz").is_none());
        assert!(!format!("{c:?}").contains(&h));
    }

    #[test]
    fn files_and_plain_len() {
        let c = cipher();
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("b.png");
        write_file(&p, b"0123456789", Some(&c)).unwrap();
        assert!(file_is_sealed(&p));
        assert_eq!(plain_len(&p).unwrap(), 10);
        assert_eq!(read_file(&p).unwrap(), b"0123456789");
        write_file(&p, b"plain", None).unwrap();
        assert!(!file_is_sealed(&p));
        assert_eq!(plain_len(&p).unwrap(), 5);
        assert_eq!(read_file(&p).unwrap(), b"plain");
    }

    #[test]
    fn unlock_creates_then_reuses_the_key_with_a_fake_keychain() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("s");
        let kc = Keychain::File(d.path().join("kc.json"));
        assert!(unlock(&state, &kc, false).unwrap().is_none());
        let a = unlock(&state, &kc, true).unwrap().unwrap();
        assert!(a.created);
        let m = read_marker(&state).unwrap();
        assert_eq!(m.key_id, a.cipher.id_hex());
        // The marker holds no key material.
        let raw = std::fs::read_to_string(state.join(MARKER)).unwrap();
        let stored = kc.get(&m.service, &m.account).unwrap().unwrap();
        assert!(!raw.contains(&stored));
        let b = unlock(&state, &kc, false).unwrap().unwrap();
        assert!(!b.created);
        assert_eq!(b.cipher.id(), a.cipher.id());
        assert_eq!(
            unlock_for_reading(&state).unwrap().unwrap().id(),
            a.cipher.id()
        );
        // The key vanished from the keychain: refuse instead of making a new one.
        kc.delete(&m.service, &m.account).unwrap();
        assert!(matches!(
            unlock(&state, &kc, true),
            Err(UnlockError::WrongKey(_))
        ));
    }
}
