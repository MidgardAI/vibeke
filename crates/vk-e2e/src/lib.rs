//! The Vibeke end-to-end channel (spec 16 §3–§5).
//!
//! - [`keys`]: host/device key material and the relay host id.
//! - [`link`]: the pairing link carried by the QR code.
//! - [`hello`]: the plaintext hello that is also the Noise prologue.
//! - [`noise`]: sans-IO Noise IK / IKpsk2 handshakes and the transport session with chunked framing.
//! - [`relay`]: relay control messages and the signatures that prove host-id ownership.
//!
//! Everything here is transport-agnostic: callers move the returned byte frames over WebSockets.

pub mod b64;
pub mod hello;
pub mod keys;
pub mod link;
pub mod noise;
pub mod relay;

pub use hello::{Hello, Mode};
pub use keys::{DeviceKey, HostKeys, host_id};
pub use link::PairingLink;
pub use noise::{Initiator, Responder, Session};

/// Protocol identifier carried in every hello.
pub const PROTO: &str = "vibeke-e2e/1";
/// Hello/wire version.
pub const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("bad message: {0}")]
    Bad(String),
    #[error("message too large")]
    TooLarge,
}

pub type Result<T> = std::result::Result<T, Error>;
