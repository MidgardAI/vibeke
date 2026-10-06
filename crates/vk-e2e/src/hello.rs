//! The plaintext hello (spec 16 §4.2, §5). Its exact bytes are the Noise prologue, so a relay that
//! rewrites it breaks the handshake.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// First contact with a pairing secret (`Noise_IKpsk2`).
    Pair,
    /// A paired device (`Noise_IK`).
    Device,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub v: u32,
    pub proto: String,
    pub mode: Mode,
    /// Pairing id (pair mode only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<String>,
}

impl Hello {
    pub fn pair(pid: &str) -> Self {
        Hello {
            v: crate::VERSION,
            proto: crate::PROTO.into(),
            mode: Mode::Pair,
            pid: Some(pid.into()),
        }
    }
    pub fn device() -> Self {
        Hello {
            v: crate::VERSION,
            proto: crate::PROTO.into(),
            mode: Mode::Device,
            pid: None,
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("hello serializes")
    }
    /// Parse and validate a received hello. Callers keep the raw bytes as the prologue.
    pub fn parse(raw: &[u8]) -> crate::Result<Hello> {
        if raw.len() > 1024 {
            return Err(crate::Error::TooLarge);
        }
        let h: Hello =
            serde_json::from_slice(raw).map_err(|e| crate::Error::Bad(format!("hello: {e}")))?;
        if h.v != crate::VERSION || h.proto != crate::PROTO {
            return Err(crate::Error::Bad("unsupported_version".into()));
        }
        if (h.mode == Mode::Pair) != h.pid.is_some() {
            return Err(crate::Error::Bad(
                "hello: pid required exactly in pair mode".into(),
            ));
        }
        Ok(h)
    }
}

/// Plain-text error frame sent before closing when the hello is unacceptable.
pub fn version_error() -> String {
    serde_json::json!({"error": "unsupported_version", "supported": [crate::VERSION]}).to_string()
}
