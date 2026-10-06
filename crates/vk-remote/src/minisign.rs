//! Minisign signature verification (09 §10, 06 A3) for release `SHA256SUMS` files and the
//! signed release manifest used by `bootstrap = "remote-download"`.
//!
//! Format (<https://jedisct1.github.io/minisign/>):
//! - public key: `untrusted comment: …` then base64 of `"Ed" | key_id[8] | ed25519_pk[32]`;
//! - signature: `untrusted comment: …`, base64 of `alg[2] | key_id[8] | sig[64]`,
//!   `trusted comment: <text>`, base64 of `global_sig[64]`.
//!
//! `alg` is `ED` (the signed message is BLAKE2b-512 of the file; minisign's default) or the
//! legacy `Ed` (the file itself). The global signature covers `sig | trusted_comment`, so the
//! trusted comment (version, file name) cannot be swapped. Only verification lives here; no
//! private key ever touches this code outside tests.

use base64::Engine;
use blake2::{Blake2b512, Digest};
use ed25519_dalek::{Signature as EdSignature, Verifier, VerifyingKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinisignError {
    /// The key or signature text is not in minisign format.
    Malformed(String),
    /// The signature was made with a key we do not trust.
    UnknownKey { key_id: String },
    /// The file signature does not verify.
    BadSignature,
    /// The trusted comment's global signature does not verify.
    BadTrustedComment,
}

impl std::fmt::Display for MinisignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MinisignError::Malformed(m) => write!(f, "malformed minisign data: {m}"),
            MinisignError::UnknownKey { key_id } => {
                write!(f, "signed with untrusted key {key_id}")
            }
            MinisignError::BadSignature => write!(f, "signature does not verify"),
            MinisignError::BadTrustedComment => {
                write!(f, "trusted comment signature does not verify")
            }
        }
    }
}

impl std::error::Error for MinisignError {}

fn b64(s: &str) -> Result<Vec<u8>, MinisignError> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| MinisignError::Malformed(format!("base64: {e}")))
}

/// The base64 line of a minisign file: skips an `untrusted comment:` line and blank lines.
fn payload_line(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("untrusted comment:"))
}

pub fn key_id_hex(id: &[u8; 8]) -> String {
    // minisign prints key ids as little-endian u64 in upper-case hex.
    format!("{:016X}", u64::from_le_bytes(*id))
}

#[derive(Debug, Clone)]
pub struct PublicKey {
    pub key_id: [u8; 8],
    key: VerifyingKey,
}

impl PublicKey {
    /// Parse a public key: the bare base64 line or a whole `.pub` file.
    pub fn parse(text: &str) -> Result<PublicKey, MinisignError> {
        let line = payload_line(text).ok_or_else(|| MinisignError::Malformed("empty".into()))?;
        let raw = b64(line)?;
        if raw.len() != 42 || &raw[..2] != b"Ed" {
            return Err(MinisignError::Malformed(
                "public key must be 42 bytes starting with \"Ed\"".into(),
            ));
        }
        let mut key_id = [0u8; 8];
        key_id.copy_from_slice(&raw[2..10]);
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&raw[10..42]);
        let key = VerifyingKey::from_bytes(&pk)
            .map_err(|e| MinisignError::Malformed(format!("public key: {e}")))?;
        Ok(PublicKey { key_id, key })
    }

    /// The base64 line for this key (as embedded in `TRUSTED_KEYS`).
    pub fn to_base64(&self) -> String {
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&self.key_id);
        raw.extend_from_slice(self.key.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    /// A key from raw parts (tests and key tooling).
    pub fn from_parts(key_id: [u8; 8], key: VerifyingKey) -> PublicKey {
        PublicKey { key_id, key }
    }
}

#[derive(Debug, Clone)]
pub struct Signature {
    pub prehashed: bool,
    pub key_id: [u8; 8],
    sig: EdSignature,
    pub trusted_comment: String,
    global: EdSignature,
}

impl Signature {
    /// Parse a `.minisig` file.
    pub fn parse(text: &str) -> Result<Signature, MinisignError> {
        let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));
        let mut next = || {
            lines
                .by_ref()
                .find(|l| !l.trim().is_empty())
                .ok_or_else(|| MinisignError::Malformed("truncated signature".into()))
        };
        let first = next()?;
        let sig_line = if first.starts_with("untrusted comment:") {
            next()?
        } else {
            first
        };
        let raw = b64(sig_line)?;
        if raw.len() != 74 {
            return Err(MinisignError::Malformed(
                "signature must be 74 bytes".into(),
            ));
        }
        let prehashed = match &raw[..2] {
            b"ED" => true,
            b"Ed" => false,
            _ => {
                return Err(MinisignError::Malformed(
                    "unknown signature algorithm".into(),
                ));
            }
        };
        let mut key_id = [0u8; 8];
        key_id.copy_from_slice(&raw[2..10]);
        let sig = EdSignature::from_slice(&raw[10..74])
            .map_err(|e| MinisignError::Malformed(format!("signature: {e}")))?;
        let tc_line = next()?;
        let trusted_comment = tc_line
            .strip_prefix("trusted comment: ")
            .ok_or_else(|| MinisignError::Malformed("missing trusted comment".into()))?
            .to_string();
        let g = b64(next()?)?;
        let global = EdSignature::from_slice(&g)
            .map_err(|e| MinisignError::Malformed(format!("global signature: {e}")))?;
        Ok(Signature {
            prehashed,
            key_id,
            sig,
            trusted_comment,
            global,
        })
    }
}

/// Verify `sig` over `data` with any of `keys` (base64 key lines or `.pub` texts). Returns the
/// verified trusted comment.
pub fn verify(keys: &[&str], data: &[u8], sig_text: &str) -> Result<String, MinisignError> {
    let sig = Signature::parse(sig_text)?;
    let mut parsed = Vec::new();
    for k in keys {
        parsed.push(PublicKey::parse(k)?);
    }
    let pk = parsed
        .iter()
        .find(|k| k.key_id == sig.key_id)
        .ok_or_else(|| MinisignError::UnknownKey {
            key_id: key_id_hex(&sig.key_id),
        })?;
    let ok = if sig.prehashed {
        let h = Blake2b512::digest(data);
        pk.key.verify(&h, &sig.sig)
    } else {
        pk.key.verify(data, &sig.sig)
    };
    ok.map_err(|_| MinisignError::BadSignature)?;
    let mut global_msg = sig.sig.to_bytes().to_vec();
    global_msg.extend_from_slice(sig.trusted_comment.as_bytes());
    pk.key
        .verify(&global_msg, &sig.global)
        .map_err(|_| MinisignError::BadTrustedComment)?;
    Ok(sig.trusted_comment)
}

/// Test-only signer producing real minisign files, so the verifier is exercised against the
/// exact format. Never compiled into release builds.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Deterministic test key (seed is public; honoured only by `cfg(test)` builds).
    pub const TEST_SEED: [u8; 32] = [0x5a; 32];
    pub const TEST_KEY_ID: [u8; 8] = *b"vk-test!";

    pub fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&TEST_SEED)
    }

    pub fn public_key_b64() -> String {
        PublicKey::from_parts(TEST_KEY_ID, signing_key().verifying_key()).to_base64()
    }

    pub fn sign_with(key: &SigningKey, key_id: [u8; 8], data: &[u8], comment: &str) -> String {
        let h = Blake2b512::digest(data);
        let sig = key.sign(&h);
        let mut raw = b"ED".to_vec();
        raw.extend_from_slice(&key_id);
        raw.extend_from_slice(&sig.to_bytes());
        let mut gm = sig.to_bytes().to_vec();
        gm.extend_from_slice(comment.as_bytes());
        let global = key.sign(&gm);
        let e = base64::engine::general_purpose::STANDARD;
        format!(
            "untrusted comment: signature from vibeke test key\n{}\ntrusted comment: {comment}\n{}\n",
            e.encode(raw),
            e.encode(global.to_bytes())
        )
    }

    pub fn sign(data: &[u8], comment: &str) -> String {
        sign_with(&signing_key(), TEST_KEY_ID, data, comment)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn verifies_prehashed_and_legacy() {
        let pk = public_key_b64();
        let data = b"abc123  vibeke-linux-x86_64\n";
        let sig = sign(data, "timestamp:1 file:SHA256SUMS");
        assert_eq!(
            verify(&[pk.as_str()], data, &sig).unwrap(),
            "timestamp:1 file:SHA256SUMS"
        );
        // A whole `.pub` file parses too.
        let pubfile = format!("untrusted comment: minisign public key\n{pk}\n");
        assert!(verify(&[pubfile.as_str()], data, &sig).is_ok());
        // Legacy `Ed` (unhashed) signatures.
        let k = signing_key();
        let s = k.sign(data);
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&TEST_KEY_ID);
        raw.extend_from_slice(&s.to_bytes());
        let mut gm = s.to_bytes().to_vec();
        gm.extend_from_slice(b"legacy");
        let e = base64::engine::general_purpose::STANDARD;
        let legacy = format!(
            "untrusted comment: x\n{}\ntrusted comment: legacy\n{}\n",
            e.encode(raw),
            e.encode(k.sign(&gm).to_bytes())
        );
        assert_eq!(verify(&[pk.as_str()], data, &legacy).unwrap(), "legacy");
    }

    #[test]
    fn rejects_tampering() {
        let pk = public_key_b64();
        let data = b"payload".to_vec();
        let sig = sign(&data, "ok");
        assert_eq!(
            verify(&[pk.as_str()], b"payloaD", &sig),
            Err(MinisignError::BadSignature)
        );
        // Swapped trusted comment.
        let swapped = sig.replace("trusted comment: ok", "trusted comment: evil");
        assert_eq!(
            verify(&[pk.as_str()], &data, &swapped),
            Err(MinisignError::BadTrustedComment)
        );
        // Another key with the same id cannot sign for it.
        let other = SigningKey::from_bytes(&[1u8; 32]);
        let forged = sign_with(&other, TEST_KEY_ID, &data, "ok");
        assert_eq!(
            verify(&[pk.as_str()], &data, &forged),
            Err(MinisignError::BadSignature)
        );
        // Unknown key id.
        let unknown = sign_with(&other, *b"otherkey", &data, "ok");
        assert!(matches!(
            verify(&[pk.as_str()], &data, &unknown),
            Err(MinisignError::UnknownKey { .. })
        ));
        // No keys at all.
        assert!(matches!(
            verify(&[], &data, &sig),
            Err(MinisignError::UnknownKey { .. })
        ));
        for junk in ["", "junk", "untrusted comment: x\nAAAA\n"] {
            assert!(matches!(
                verify(&[pk.as_str()], &data, junk),
                Err(MinisignError::Malformed(_))
            ));
        }
        assert!(PublicKey::parse("RWQ=").is_err());
    }
}
