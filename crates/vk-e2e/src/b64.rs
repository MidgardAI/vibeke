//! Unpadded base64url, the only binary-to-text encoding on the wire.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn encode(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn decode(s: &str) -> crate::Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|e| crate::Error::Bad(format!("base64: {e}")))
}

/// Decode exactly `N` bytes.
pub fn decode_array<const N: usize>(s: &str) -> crate::Result<[u8; N]> {
    decode(s)?
        .try_into()
        .map_err(|v: Vec<u8>| crate::Error::Bad(format!("expected {N} bytes, got {}", v.len())))
}
