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
//! Large blobs (over [`CHUNKED_ABOVE`]) use a chunked format instead, so no record ever
//! exceeds what a reader accepts and a ranged read decrypts only the chunks it needs:
//! `"VKE2" | key id (8) | blob nonce (12) | chunk size (u32 LE) | plaintext length (u64 LE)`,
//! then every chunk's ciphertext+tag in order (each chunk `chunk size` bytes of plaintext, the
//! last one shorter; an empty blob is one empty chunk). Chunk `i` is sealed under the blob
//! nonce with its last 8 bytes XORed with `i` (big endian) — unique per chunk, never reused —
//! and the associated data is the whole header plus a final-chunk flag byte, so chunks can't
//! be reordered, dropped, truncated or moved between blobs undetected.
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
/// Magic of the chunked (large blob) format.
pub const MAGIC2: &[u8; 4] = b"VKE2";
/// Header of the chunked format.
const HEADER2: usize = 4 + 8 + 12 + 4 + 8;
/// Plaintext bytes per chunk of the chunked format.
pub const CHUNK: usize = 1 << 20;
/// Sealed files with more plaintext than this are written in the chunked format.
pub const CHUNKED_ABOVE: usize = 4 << 20;
const AAD_LEN: usize = 4 + 8;
const HEADER: usize = AAD_LEN + 12 + 4;
const TAG: usize = 16;
/// Bytes a record adds to its plaintext.
pub const OVERHEAD: usize = HEADER + TAG;
/// Largest chunk a chunked header may declare (a corrupt size must not allocate gigabytes),
/// and the old single-record limit. Scaled down in unit tests so the boundary tests stay fast
/// (unoptimized AES-GCM runs at a few MB/s); the logic is the same.
#[cfg(not(test))]
const MAX_RECORD: usize = 256 << 20;
#[cfg(test)]
const MAX_RECORD: usize = 6 << 20;

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

    /// Seal `plain` in the chunked format, writing it to `w` chunk by chunk.
    pub fn seal_chunked_to<W: std::io::Write>(
        &self,
        plain: &[u8],
        w: &mut W,
    ) -> std::io::Result<()> {
        use rand::RngCore;
        let mut nonce = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce);
        let header = chunked_header(&self.id, &nonce, CHUNK as u32, plain.len() as u64);
        w.write_all(&header)?;
        let n = chunk_count(plain.len() as u64, CHUNK as u64);
        let mut aad = header.to_vec();
        aad.push(0);
        for i in 0..n {
            let start = (i as usize) * CHUNK;
            let end = (start + CHUNK).min(plain.len());
            *aad.last_mut().unwrap() = u8::from(i + 1 == n);
            let ct = self
                .aead
                .encrypt(
                    Nonce::from_slice(&chunk_nonce(&nonce, i)),
                    Payload {
                        msg: &plain[start..end],
                        aad: &aad,
                    },
                )
                .expect("AES-GCM encryption of an in-memory buffer cannot fail");
            w.write_all(&ct)?;
        }
        Ok(())
    }

    /// Seal `plain` in the chunked format.
    pub fn seal_chunked(&self, plain: &[u8]) -> Vec<u8> {
        let n = chunk_count(plain.len() as u64, CHUNK as u64) as usize;
        let mut out = Vec::with_capacity(HEADER2 + plain.len() + n * TAG);
        self.seal_chunked_to(plain, &mut out)
            .expect("writing to a Vec cannot fail");
        out
    }
}

fn chunked_header(id: &[u8; 8], nonce: &[u8; 12], chunk: u32, len: u64) -> [u8; HEADER2] {
    let mut h = [0u8; HEADER2];
    h[..4].copy_from_slice(MAGIC2);
    h[4..12].copy_from_slice(id);
    h[12..24].copy_from_slice(nonce);
    h[24..28].copy_from_slice(&chunk.to_le_bytes());
    h[28..36].copy_from_slice(&len.to_le_bytes());
    h
}

/// Chunks of a `len`-byte plaintext (an empty one still has one, empty, final chunk).
fn chunk_count(len: u64, chunk: u64) -> u64 {
    len.div_ceil(chunk).max(1)
}

/// The nonce of chunk `i`: the blob nonce with its last 8 bytes XORed with `i`.
fn chunk_nonce(base: &[u8; 12], i: u64) -> [u8; 12] {
    let mut n = *base;
    let tail = u64::from_be_bytes(n[4..12].try_into().unwrap()) ^ i;
    n[4..12].copy_from_slice(&tail.to_be_bytes());
    n
}

/// A parsed chunked header.
struct Chunked {
    header: [u8; HEADER2],
    cipher: Arc<StateCipher>,
    nonce: [u8; 12],
    chunk: u64,
    len: u64,
    count: u64,
}

impl Chunked {
    /// Parse and check a header against the total file size.
    fn parse(head: &[u8], total: u64) -> Result<Chunked, OpenError> {
        if head.len() < HEADER2 || &head[..4] != MAGIC2 {
            return Err(OpenError::Corrupt);
        }
        let mut header = [0u8; HEADER2];
        header.copy_from_slice(&head[..HEADER2]);
        let mut id = [0u8; 8];
        id.copy_from_slice(&header[4..12]);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&header[12..24]);
        let chunk = u64::from(u32::from_le_bytes(header[24..28].try_into().unwrap()));
        let len = u64::from_le_bytes(header[28..36].try_into().unwrap());
        if chunk == 0 || chunk as usize > MAX_RECORD - TAG {
            return Err(OpenError::Corrupt);
        }
        let count = chunk_count(len, chunk);
        let want = (HEADER2 as u64)
            .checked_add(len)
            .and_then(|x| x.checked_add(count.checked_mul(TAG as u64)?));
        if want != Some(total) {
            return Err(OpenError::Corrupt);
        }
        let cipher = registered(&id).ok_or_else(|| OpenError::MissingKey(hex(&id)))?;
        Ok(Chunked {
            header,
            cipher,
            nonce,
            chunk,
            len,
            count,
        })
    }

    /// File offset and ciphertext length of chunk `i`.
    fn span(&self, i: u64) -> (u64, usize) {
        let plain = self.chunk.min(self.len - (i * self.chunk).min(self.len));
        (
            HEADER2 as u64 + i * (self.chunk + TAG as u64),
            plain as usize + TAG,
        )
    }

    fn open(&self, i: u64, ct: &[u8]) -> Result<Vec<u8>, OpenError> {
        let mut aad = self.header.to_vec();
        aad.push(u8::from(i + 1 == self.count));
        self.cipher
            .open_record(&aad, &chunk_nonce(&self.nonce, i), ct)
            .ok_or(OpenError::Corrupt)
    }

    /// Decrypt every chunk of an in-memory file; on damage, what came before.
    fn open_all(&self, data: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(self.len as usize);
        for i in 0..self.count {
            let (at, n) = self.span(i);
            let Some(ct) = data.get(at as usize..at as usize + n) else {
                return (out, true);
            };
            match self.open(i, ct) {
                Ok(p) => out.extend_from_slice(&p),
                Err(_) => return (out, true),
            }
        }
        (out, false)
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

/// Does this data start a sealed record stream (or a chunked sealed blob)?
pub fn is_sealed(data: &[u8]) -> bool {
    data.starts_with(MAGIC) || data.starts_with(MAGIC2)
}

/// Is the file at `p` sealed? (`false` for a missing or short file.)
pub fn file_is_sealed(p: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 4];
    std::fs::File::open(p)
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok()
        && (&b == MAGIC || &b == MAGIC2)
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
    if data.starts_with(MAGIC2) {
        let c = match Chunked::parse(data, data.len() as u64) {
            Ok(c) => c,
            Err(OpenError::MissingKey(k)) => return Err(OpenError::MissingKey(k)),
            Err(OpenError::Corrupt) => {
                return Ok(Opened {
                    damaged: true,
                    ..Default::default()
                });
            }
        };
        let (plain, damaged) = c.open_all(data);
        return Ok(Opened {
            data: plain,
            records: 1,
            damaged,
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
        // Bounded by the data actually present (already in memory), not by a fixed cap: a
        // single record larger than `MAX_RECORD` written by an older version stays readable
        // (and migratable).
        if len < TAG || rest.len() < HEADER + len {
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

/// Read `length` plaintext bytes at `offset` of a possibly sealed file (clamped to its end).
/// A chunked blob decrypts only the chunks the range touches; a record stream is decrypted
/// whole; a plain file is read in place.
pub fn read_range(p: &Path, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let total = f.metadata()?.len();
    let mut head = [0u8; HEADER2];
    let n = f.read(&mut head)?;
    if n >= 4 && &head[..4] == MAGIC2 {
        let mut rest = n;
        while rest < HEADER2 {
            let k = f.read(&mut head[rest..])?;
            if k == 0 {
                break;
            }
            rest += k;
        }
        let c = Chunked::parse(&head[..rest], total)?;
        let start = offset.min(c.len);
        let end = start.saturating_add(length).min(c.len);
        let mut out = Vec::with_capacity((end - start) as usize);
        if start == end {
            return Ok(out);
        }
        for i in start / c.chunk..=(end - 1) / c.chunk {
            let (at, len) = c.span(i);
            let mut ct = vec![0u8; len];
            f.seek(SeekFrom::Start(at))?;
            f.read_exact(&mut ct)?;
            let plain = c.open(i, &ct)?;
            let base = i * c.chunk;
            let a = start.max(base) - base;
            let b = end.min(base + plain.len() as u64) - base;
            out.extend_from_slice(&plain[a as usize..b as usize]);
        }
        return Ok(out);
    }
    if n >= 4 && &head[..4] == MAGIC {
        let all = read_file(p)?;
        let start = (offset as usize).min(all.len());
        let end = start.saturating_add(length as usize).min(all.len());
        return Ok(all[start..end].to_vec());
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity(length.min(total) as usize);
    f.take(length).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Plaintext length of a possibly sealed file, from the record headers (no decryption).
pub fn plain_len(p: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let total = f.metadata()?.len();
    let mut head = [0u8; HEADER2];
    if total >= HEADER2 as u64 && f.read_exact(&mut head).is_ok() && &head[..4] == MAGIC2 {
        return Ok(u64::from_le_bytes(head[28..36].try_into().unwrap()));
    }
    f.seek(SeekFrom::Start(0))?;
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

/// Write `data` to `p` (0600) via a temp file and rename, sealed when `cipher` is given:
/// one record up to [`CHUNKED_ABOVE`] bytes, the chunked format (streamed) above.
pub fn write_file(p: &Path, data: &[u8], cipher: Option<&StateCipher>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let chunked = cipher.filter(|_| data.len() > CHUNKED_ABOVE);
    let bytes;
    let body: &[u8] = match cipher {
        _ if chunked.is_some() => &[],
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
    match chunked {
        Some(c) => {
            let mut w = std::io::BufWriter::with_capacity(CHUNK + TAG, &mut f);
            c.seal_chunked_to(data, &mut w)?;
            w.flush()?;
        }
        None => f.write_all(body)?,
    }
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

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
    }

    /// Round-trip, ranged reads and migration (plain → sealed → plain) of one size.
    fn roundtrip(c: &StateCipher, dir: &Path, data: &[u8]) {
        let n = data.len();
        let p = dir.join("blob.bin");
        write_file(&p, data, Some(c)).unwrap();
        assert!(file_is_sealed(&p), "{n}");
        assert_eq!(plain_len(&p).unwrap(), n as u64, "{n}");
        let back = read_file(&p).unwrap();
        assert!(back == data, "round-trip of {n} bytes");
        drop(back);
        // Ranged reads: head, across a chunk boundary, the tail, past the end.
        for (off, len) in [
            (0u64, 7u64),
            (CHUNK as u64 - 5, 10),
            (n as u64 - n.min(9) as u64, 20),
            (n as u64 + 3, 5),
        ] {
            let got = read_range(&p, off, len).unwrap();
            let s = (off as usize).min(n);
            let e = (s + len as usize).min(n);
            assert_eq!(got, &data[s..e], "range {off}+{len} of {n}");
        }
        // Migration to plain and back.
        let plain = read_file(&p).unwrap();
        write_file(&p, &plain, None).unwrap();
        assert!(!file_is_sealed(&p));
        drop(plain);
        let again = read_file(&p).unwrap();
        write_file(&p, &again, Some(c)).unwrap();
        drop(again);
        assert!(file_is_sealed(&p));
        assert!(read_file(&p).unwrap() == data, "re-sealed {n}");
        std::fs::remove_file(&p).unwrap();
    }

    /// Final review P1 7: blobs on both sides of the old single-record limit
    /// (`MAX_RECORD - TAG`) and of the chunked threshold round-trip, read in ranges and
    /// migrate both ways. A legacy single record over the limit stays readable.
    #[test]
    fn large_blobs_round_trip_and_migrate_across_the_record_limit() {
        let c = cipher();
        let d = tempfile::tempdir().unwrap();
        let limit = MAX_RECORD - TAG;
        let big = pattern(limit + 1);
        for n in [
            0,
            1,
            CHUNK,
            CHUNKED_ABOVE,
            CHUNKED_ABOVE + 1,
            3 * CHUNK + 17,
            limit - 1,
            limit,
            limit + 1,
        ] {
            roundtrip(&c, d.path(), &big[..n]);
        }
        // A legacy (pre-chunking) single record over the limit: readable, migratable.
        let p = d.path().join("legacy.bin");
        std::fs::write(&p, c.seal(&big)).unwrap();
        assert_eq!(plain_len(&p).unwrap(), big.len() as u64);
        assert!(read_file(&p).unwrap() == big);
        assert_eq!(
            read_range(&p, limit as u64 - 2, 10).unwrap(),
            &big[limit - 2..]
        );
        let plain = read_file(&p).unwrap();
        write_file(&p, &plain, Some(&c)).unwrap();
        assert!(read_file(&p).unwrap() == big);
    }

    /// The chunked format detects reordering, truncation, a dropped final chunk and a header
    /// edit, and a ranged read only needs (and only authenticates) the chunks it touches.
    #[test]
    fn chunked_blobs_detect_tampering_and_read_ranges_locally() {
        let c = cipher();
        let d = tempfile::tempdir().unwrap();
        let data = pattern(CHUNKED_ABOVE + 3 * CHUNK + 5);
        let p = d.path().join("b.bin");
        write_file(&p, &data, Some(&c)).unwrap();
        let raw = std::fs::read(&p).unwrap();
        assert!(raw.starts_with(MAGIC2));
        let ct = CHUNK + TAG;
        let corrupt = |bytes: &[u8]| {
            std::fs::write(&p, bytes).unwrap();
            read_file(&p).is_err()
        };
        // Swap chunks 0 and 1.
        let mut swapped = raw.clone();
        let (a, b) = (HEADER2, HEADER2 + ct);
        let first = swapped[a..a + ct].to_vec();
        let second = swapped[b..b + ct].to_vec();
        swapped[a..a + ct].copy_from_slice(&second);
        swapped[b..b + ct].copy_from_slice(&first);
        assert!(corrupt(&swapped), "reordered chunks");
        // Truncated by one byte, and with the final chunk dropped entirely.
        assert!(corrupt(&raw[..raw.len() - 1]), "truncated");
        let last = (data.len() % CHUNK) + TAG;
        let mut dropped = raw[..raw.len() - last].to_vec();
        let shorter = (data.len() - data.len() % CHUNK) as u64;
        dropped[28..36].copy_from_slice(&shorter.to_le_bytes());
        assert!(corrupt(&dropped), "final chunk dropped (length edited)");
        // A damaged chunk 0 doesn't stop a range inside chunk 5; a range touching it fails.
        let mut bad = raw.clone();
        bad[HEADER2 + 3] ^= 1;
        std::fs::write(&p, &bad).unwrap();
        let off = 5 * CHUNK as u64 + 11;
        assert_eq!(
            read_range(&p, off, 100).unwrap(),
            &data[off as usize..off as usize + 100]
        );
        assert!(read_range(&p, 10, 10).is_err());
        // Distinct nonces per chunk and per blob.
        assert_ne!(chunk_nonce(&[7; 12], 0), chunk_nonce(&[7; 12], 1));
        write_file(&p, &data, Some(&c)).unwrap();
        let other = std::fs::read(&p).unwrap();
        assert_ne!(&other[12..24], &raw[12..24], "fresh blob nonce");
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
