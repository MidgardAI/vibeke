//! Key material (spec 16 §3).

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::b64;

/// Host id = lowercase RFC 4648 base32 (no padding) of the first 16 bytes of
/// `blake3(relay public key)`: 26 characters, safe in URLs and DNS labels.
pub fn host_id(relay_pub: &[u8; 32]) -> String {
    base32(&blake3::hash(relay_pub).as_bytes()[..16])
}

fn base32(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rng().fill_bytes(&mut b);
    b
}

/// X25519 public key for a private scalar (clamping is applied by the DH function).
pub fn x25519_public(private: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*private)).to_bytes()
}

/// The host's long-term keys. Serialized as base64url strings in `host.json`.
#[derive(Clone, Serialize, Deserialize)]
pub struct HostKeys {
    /// X25519 Noise static private key.
    #[serde(with = "b64_array")]
    pub noise_private: [u8; 32],
    /// Ed25519 seed for relay authentication.
    #[serde(with = "b64_array")]
    pub relay_seed: [u8; 32],
}

impl std::fmt::Debug for HostKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostKeys")
            .field("host_id", &self.host_id())
            .finish_non_exhaustive()
    }
}

impl HostKeys {
    pub fn generate() -> Self {
        HostKeys {
            noise_private: random_bytes(),
            relay_seed: random_bytes(),
        }
    }
    pub fn noise_public(&self) -> [u8; 32] {
        x25519_public(&self.noise_private)
    }
    fn signing(&self) -> SigningKey {
        SigningKey::from_bytes(&self.relay_seed)
    }
    pub fn relay_public(&self) -> [u8; 32] {
        self.signing().verifying_key().to_bytes()
    }
    pub fn host_id(&self) -> String {
        host_id(&self.relay_public())
    }
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing().sign(msg).to_bytes()
    }
}

/// Verify an Ed25519 signature by a relay public key.
pub fn verify(relay_pub: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(relay_pub) else {
        return false;
    };
    key.verify(msg, &ed25519_dalek::Signature::from_bytes(sig))
        .is_ok()
}

/// A device's Noise static key (used by Rust test clients; browsers keep theirs in IndexedDB).
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceKey {
    #[serde(with = "b64_array")]
    pub private: [u8; 32],
}

impl DeviceKey {
    pub fn generate() -> Self {
        DeviceKey {
            private: random_bytes(),
        }
    }
    pub fn public(&self) -> [u8; 32] {
        x25519_public(&self.private)
    }
}

pub mod b64_array {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(v: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::b64::encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        d: D,
    ) -> Result<[u8; N], D::Error> {
        let s = String::deserialize(d)?;
        crate::b64::decode_array(&s).map_err(serde::de::Error::custom)
    }
}

/// Short human fingerprint of a public key: first 8 base32 chars of blake3, grouped `abcd-efgh`.
pub fn fingerprint(public: &[u8]) -> String {
    let b = base32(&blake3::hash(public).as_bytes()[..5]);
    format!("{}-{}", &b[..4], &b[4..8])
}

pub fn encode_key(k: &[u8; 32]) -> String {
    b64::encode(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_known_vectors() {
        // RFC 4648 test vectors, lowercased, no padding.
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "my");
        assert_eq!(base32(b"fo"), "mzxq");
        assert_eq!(base32(b"foo"), "mzxw6");
        assert_eq!(base32(b"foob"), "mzxw6yq");
        assert_eq!(base32(b"fooba"), "mzxw6ytb");
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn host_id_is_26_chars_and_signature_verifies() {
        let k = HostKeys::generate();
        assert_eq!(k.host_id().len(), 26);
        let sig = k.sign(b"hello");
        assert!(verify(&k.relay_public(), b"hello", &sig));
        assert!(!verify(&k.relay_public(), b"hellO", &sig));
    }

    #[test]
    fn host_keys_roundtrip_json() {
        let k = HostKeys::generate();
        let j = serde_json::to_string(&k).unwrap();
        let k2: HostKeys = serde_json::from_str(&j).unwrap();
        assert_eq!(k.host_id(), k2.host_id());
        assert_eq!(k.noise_public(), k2.noise_public());
    }
}
