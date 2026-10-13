//! Continue-link encryption (spec 20 §4.3): a handoff bundle encrypted with XChaCha20-Poly1305 in
//! the STREAM construction, exactly as `aead::stream::StreamBE32` builds it.
//!
//! ```text
//! file       = "VKC1" ‖ prefix[19] ‖ chunk* ‖ last_chunk
//! chunk      = encrypt_next(1 MiB plaintext)          (1 MiB + 16 bytes)
//! last_chunk = encrypt_last(0..=1 MiB plaintext)      (at most 1 MiB + 16 bytes)
//! nonce      = prefix[19] ‖ counter (u32, big-endian) ‖ last (0 or 1)
//! AAD        = "vibeke-continue/1"
//! ```
//!
//! A plaintext of a whole number of chunks ends with a full last chunk; an empty plaintext is one
//! empty last chunk. Decryption fails on a bad tag, on bytes after the last chunk and when the file
//! ends without a last chunk. Both directions hash the plaintext (`sha256`, `size`) as it passes,
//! so the receiver checks it against the link without reading the file again.

use std::io::{Read, Write};
use std::path::Path;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const MAGIC: &[u8; 4] = b"VKC1";
pub const KEY_LEN: usize = 32;
pub const PREFIX_LEN: usize = 19;
/// Magic and nonce prefix.
pub const HEADER_LEN: usize = MAGIC.len() + PREFIX_LEN;
/// Plaintext bytes per chunk.
pub const CHUNK: usize = 1024 * 1024;
pub const TAG_LEN: usize = 16;
pub const AAD: &[u8] = b"vibeke-continue/1";

/// What an encryption wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// Plaintext bytes.
    pub size: u64,
    /// Lowercase hex sha256 of the plaintext.
    pub sha256: String,
    /// Bytes of the encrypted file (header included).
    pub cipher_size: u64,
}

/// What a decryption produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub size: u64,
    pub sha256: String,
}

/// A fresh random key.
pub fn new_key() -> [u8; KEY_LEN] {
    rand::random()
}

/// Size of the encrypted file for a plaintext of `plain` bytes.
pub fn cipher_len(plain: u64) -> u64 {
    let chunks = plain.div_ceil(CHUNK as u64).max(1);
    HEADER_LEN as u64 + plain + chunks * TAG_LEN as u64
}

fn nonce(prefix: &[u8; PREFIX_LEN], counter: u32, last: bool) -> XNonce {
    let mut n = [0u8; 24];
    n[..PREFIX_LEN].copy_from_slice(prefix);
    n[PREFIX_LEN..PREFIX_LEN + 4].copy_from_slice(&counter.to_be_bytes());
    n[23] = u8::from(last);
    *XNonce::from_slice(&n)
}

fn cipher(key: &[u8; KEY_LEN]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(Key::from_slice(key))
}

/// Reads until `buf` is full or the input ends; returns the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn io_err(e: std::io::Error) -> Error {
    Error::new("internal", e.to_string())
}

fn damaged(why: &str) -> Error {
    Error::new(
        "conflict",
        format!("the continue bundle does not open: {why}"),
    )
}

/// Encrypt everything `r` yields into `w` with `key` and the nonce `prefix`.
pub fn encrypt<R: Read, W: Write>(
    mut r: R,
    mut w: W,
    key: &[u8; KEY_LEN],
    prefix: &[u8; PREFIX_LEN],
) -> std::io::Result<Sealed> {
    let c = cipher(key);
    w.write_all(MAGIC)?;
    w.write_all(prefix)?;
    let mut h = Sha256::new();
    let (mut size, mut out) = (0u64, HEADER_LEN as u64);
    let mut cur = vec![0u8; CHUNK];
    let mut next = vec![0u8; CHUNK];
    let mut n = read_full(&mut r, &mut cur)?;
    let mut counter: u32 = 0;
    loop {
        // A full chunk is the last one only when nothing follows it.
        let m = if n == CHUNK {
            read_full(&mut r, &mut next)?
        } else {
            0
        };
        let last = m == 0;
        h.update(&cur[..n]);
        size += n as u64;
        let ct = c
            .encrypt(
                &nonce(prefix, counter, last),
                Payload {
                    msg: &cur[..n],
                    aad: AAD,
                },
            )
            .map_err(|_| std::io::Error::other("encryption failed"))?;
        w.write_all(&ct)?;
        out += ct.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        n = m;
        counter = counter
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("too many chunks"))?;
    }
    w.flush()?;
    Ok(Sealed {
        size,
        sha256: crate::hex(&h.finalize()),
        cipher_size: out,
    })
}

/// Decrypt a continue bundle from `r` into `w`. Fails (`conflict`) on a wrong magic, a bad tag,
/// data after the last chunk or a missing last chunk, and (`too_large`) when the plaintext would
/// exceed `max` bytes. `w` may hold a partial plaintext after a failure.
pub fn decrypt<R: Read, W: Write>(
    mut r: R,
    mut w: W,
    key: &[u8; KEY_LEN],
    max: u64,
) -> Result<Opened> {
    let mut head = [0u8; HEADER_LEN];
    if read_full(&mut r, &mut head).map_err(io_err)? < HEADER_LEN {
        return Err(damaged("the file is too short"));
    }
    if &head[..MAGIC.len()] != MAGIC {
        return Err(damaged("not a continue bundle"));
    }
    let mut prefix = [0u8; PREFIX_LEN];
    prefix.copy_from_slice(&head[MAGIC.len()..]);
    let c = cipher(key);
    let open = |counter: u32, last: bool, ct: &[u8]| {
        c.decrypt(
            &nonce(&prefix, counter, last),
            Payload { msg: ct, aad: AAD },
        )
        .ok()
    };
    const SEG: usize = CHUNK + TAG_LEN;
    let mut h = Sha256::new();
    let mut size = 0u64;
    let mut cur = vec![0u8; SEG];
    let mut next = vec![0u8; SEG];
    let mut n = read_full(&mut r, &mut cur).map_err(io_err)?;
    let mut counter: u32 = 0;
    loop {
        if n < TAG_LEN {
            return Err(damaged("the file ends without a last chunk"));
        }
        let m = if n == SEG {
            read_full(&mut r, &mut next).map_err(io_err)?
        } else {
            0
        };
        let at_end = m == 0;
        let ct = &cur[..n];
        let pt = if at_end {
            match open(counter, true, ct) {
                Some(p) => p,
                None if open(counter, false, ct).is_some() => {
                    return Err(damaged("the file ends without a last chunk"));
                }
                None => return Err(damaged("bad tag (wrong key or damaged file)")),
            }
        } else {
            match open(counter, false, ct) {
                Some(p) => p,
                None if open(counter, true, ct).is_some() => {
                    return Err(damaged("data after the last chunk"));
                }
                None => return Err(damaged("bad tag (wrong key or damaged file)")),
            }
        };
        size += pt.len() as u64;
        if size > max {
            return Err(Error::new(
                "too_large",
                format!("the continue bundle is larger than {max} bytes"),
            ));
        }
        h.update(&pt);
        w.write_all(&pt).map_err(io_err)?;
        if at_end {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        n = m;
        counter = counter
            .checked_add(1)
            .ok_or_else(|| damaged("too many chunks"))?;
    }
    w.flush().map_err(io_err)?;
    Ok(Opened {
        size,
        sha256: crate::hex(&h.finalize()),
    })
}

fn create_private(p: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(p)
}

/// Encrypt the file `src` into a new file `dest` (0600) with a random nonce prefix.
pub fn encrypt_file(src: &Path, dest: &Path, key: &[u8; KEY_LEN]) -> std::io::Result<Sealed> {
    let prefix: [u8; PREFIX_LEN] = rand::random();
    let input = std::io::BufReader::new(std::fs::File::open(src)?);
    let out = create_private(dest)?;
    let r = encrypt(input, std::io::BufWriter::new(&out), key, &prefix);
    match r {
        Ok(s) => {
            out.sync_all()?;
            Ok(s)
        }
        Err(e) => {
            let _ = std::fs::remove_file(dest);
            Err(e)
        }
    }
}

/// Decrypt the file `src` into a new file `dest` (0600). `dest` is removed when it fails.
pub fn decrypt_file(src: &Path, dest: &Path, key: &[u8; KEY_LEN], max: u64) -> Result<Opened> {
    let input = std::io::BufReader::new(std::fs::File::open(src).map_err(io_err)?);
    let out = create_private(dest).map_err(io_err)?;
    let r = decrypt(input, std::io::BufWriter::new(&out), key, max)
        .and_then(|o| out.sync_all().map(|_| o).map_err(io_err));
    if r.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    r
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
