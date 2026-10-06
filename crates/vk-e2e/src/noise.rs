//! Sans-IO Noise handshakes and the transport session (spec 16 §4.2–§5).
//!
//! Each returned `Vec<u8>` is exactly one WebSocket binary frame.

use crate::{Error, Result};

pub const IK: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub const IK_PSK2: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";

/// Noise's hard limit on one message.
pub const NOISE_MAX: usize = 65535;
/// Plaintext bytes per chunk (flag byte + chunk + 16-byte tag stays under [`NOISE_MAX`]).
pub const CHUNK: usize = 65000;
/// Largest reassembled application message.
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

const FLAG_MORE: u8 = 0;
const FLAG_FINAL: u8 = 1;

fn builder<'a>(
    pattern: &'a str,
    prologue: &'a [u8],
    local: &'a [u8; 32],
) -> Result<snow::Builder<'a>> {
    Ok(snow::Builder::new(pattern.parse()?)
        .prologue(prologue)?
        .local_private_key(local)?)
}

/// Device side.
pub struct Initiator {
    hs: snow::HandshakeState,
}

impl Initiator {
    /// `psk = Some(..)` selects IKpsk2 (pairing); `None` selects IK.
    pub fn new(
        prologue: &[u8],
        local_private: &[u8; 32],
        remote_public: &[u8; 32],
        psk: Option<&[u8; 32]>,
    ) -> Result<Self> {
        Self::build(prologue, local_private, remote_public, psk, None)
    }

    /// Deterministic ephemeral key, for conformance vectors only.
    #[doc(hidden)]
    pub fn new_with_ephemeral(
        prologue: &[u8],
        local_private: &[u8; 32],
        remote_public: &[u8; 32],
        psk: Option<&[u8; 32]>,
        ephemeral: &[u8; 32],
    ) -> Result<Self> {
        Self::build(prologue, local_private, remote_public, psk, Some(ephemeral))
    }

    fn build(
        prologue: &[u8],
        local_private: &[u8; 32],
        remote_public: &[u8; 32],
        psk: Option<&[u8; 32]>,
        ephemeral: Option<&[u8; 32]>,
    ) -> Result<Self> {
        let pattern = if psk.is_some() { IK_PSK2 } else { IK };
        let mut b = builder(pattern, prologue, local_private)?.remote_public_key(remote_public)?;
        if let Some(psk) = psk {
            b = b.psk(2, psk)?;
        }
        if let Some(e) = ephemeral {
            b = b.fixed_ephemeral_key_for_testing_only(e);
        }
        Ok(Initiator {
            hs: b.build_initiator()?,
        })
    }

    /// Handshake message 1 (`-> e, es, s, ss[, psk]`).
    pub fn write_first(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; NOISE_MAX];
        let n = self.hs.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Handshake message 2 (`<- e, ee, se`) → server payload and the transport session.
    pub fn read_second(mut self, msg: &[u8]) -> Result<(Vec<u8>, Session)> {
        let mut buf = vec![0u8; NOISE_MAX];
        let n = self.hs.read_message(msg, &mut buf)?;
        buf.truncate(n);
        Ok((buf, Session::new(self.hs.into_transport_mode()?)))
    }
}

/// Host (gateway) side.
pub struct Responder {
    hs: snow::HandshakeState,
}

impl Responder {
    pub fn new(prologue: &[u8], local_private: &[u8; 32], psk: Option<&[u8; 32]>) -> Result<Self> {
        Self::build(prologue, local_private, psk, None)
    }

    #[doc(hidden)]
    pub fn new_with_ephemeral(
        prologue: &[u8],
        local_private: &[u8; 32],
        psk: Option<&[u8; 32]>,
        ephemeral: &[u8; 32],
    ) -> Result<Self> {
        Self::build(prologue, local_private, psk, Some(ephemeral))
    }

    fn build(
        prologue: &[u8],
        local_private: &[u8; 32],
        psk: Option<&[u8; 32]>,
        ephemeral: Option<&[u8; 32]>,
    ) -> Result<Self> {
        let pattern = if psk.is_some() { IK_PSK2 } else { IK };
        let mut b = builder(pattern, prologue, local_private)?;
        if let Some(psk) = psk {
            b = b.psk(2, psk)?;
        }
        if let Some(e) = ephemeral {
            b = b.fixed_ephemeral_key_for_testing_only(e);
        }
        Ok(Responder {
            hs: b.build_responder()?,
        })
    }

    /// Read message 1 → (initiator static public key, payload). The caller authorizes the key
    /// before calling [`Responder::write_second`].
    pub fn read_first(&mut self, msg: &[u8]) -> Result<([u8; 32], Vec<u8>)> {
        let mut buf = vec![0u8; NOISE_MAX];
        let n = self.hs.read_message(msg, &mut buf)?;
        buf.truncate(n);
        let rs: [u8; 32] = self
            .hs
            .get_remote_static()
            .ok_or_else(|| Error::Bad("no remote static".into()))?
            .try_into()
            .map_err(|_| Error::Bad("remote static length".into()))?;
        Ok((rs, buf))
    }

    pub fn write_second(mut self, payload: &[u8]) -> Result<(Vec<u8>, Session)> {
        let mut buf = vec![0u8; NOISE_MAX];
        let n = self.hs.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok((buf, Session::new(self.hs.into_transport_mode()?)))
    }
}

/// An established channel: chunked encryption and reassembly (spec 16 §5).
pub struct Session {
    ts: snow::TransportState,
    partial: Vec<u8>,
    /// When the current unfinished message started (spec 16 §5: 30 s reassembly deadline).
    partial_since: Option<std::time::Instant>,
}

/// How long a chunked message may take to complete.
pub const REASSEMBLY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

impl Session {
    fn new(ts: snow::TransportState) -> Self {
        Session {
            ts,
            partial: Vec::new(),
            partial_since: None,
        }
    }

    /// Encrypt one application message into one or more frames.
    pub fn encrypt(&mut self, msg: &[u8]) -> Result<Vec<Vec<u8>>> {
        if msg.len() > MAX_MESSAGE {
            return Err(Error::TooLarge);
        }
        let mut frames = Vec::with_capacity(msg.len() / CHUNK + 1);
        let mut chunks = msg.chunks(CHUNK).peekable();
        if chunks.peek().is_none() {
            frames.push(self.encrypt_chunk(FLAG_FINAL, &[])?);
        }
        while let Some(c) = chunks.next() {
            let flag = if chunks.peek().is_some() {
                FLAG_MORE
            } else {
                FLAG_FINAL
            };
            frames.push(self.encrypt_chunk(flag, c)?);
        }
        Ok(frames)
    }

    fn encrypt_chunk(&mut self, flag: u8, chunk: &[u8]) -> Result<Vec<u8>> {
        let mut plain = Vec::with_capacity(chunk.len() + 1);
        plain.push(flag);
        plain.extend_from_slice(chunk);
        let mut out = vec![0u8; plain.len() + 16];
        let n = self.ts.write_message(&plain, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    /// Decrypt one frame. Returns the whole message once its final chunk arrives.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>> {
        if frame.len() > NOISE_MAX {
            return Err(Error::TooLarge);
        }
        let mut plain = vec![0u8; frame.len()];
        let n = self.ts.read_message(frame, &mut plain)?;
        let (&flag, chunk) = plain[..n]
            .split_first()
            .ok_or_else(|| Error::Bad("empty frame".into()))?;
        if self.partial.len() + chunk.len() > MAX_MESSAGE {
            return Err(Error::TooLarge);
        }
        if self
            .partial_since
            .is_some_and(|t| t.elapsed() > REASSEMBLY_DEADLINE)
        {
            return Err(Error::Bad("message reassembly deadline exceeded".into()));
        }
        self.partial.extend_from_slice(chunk);
        match flag {
            FLAG_FINAL => {
                self.partial_since = None;
                Ok(Some(std::mem::take(&mut self.partial)))
            }
            FLAG_MORE => {
                self.partial_since
                    .get_or_insert_with(std::time::Instant::now);
                Ok(None)
            }
            f => Err(Error::Bad(format!("frame flag {f}"))),
        }
    }

    /// The remote static key (always known after IK).
    pub fn remote_static(&self) -> Option<[u8; 32]> {
        self.ts.get_remote_static().and_then(|k| k.try_into().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{DeviceKey, HostKeys};

    fn pair(psk: Option<&[u8; 32]>, psk_resp: Option<&[u8; 32]>) -> Result<(Session, Session)> {
        let host = HostKeys::generate();
        let dev = DeviceKey::generate();
        let prologue = b"{\"v\":1}";
        let mut i = Initiator::new(prologue, &dev.private, &host.noise_public(), psk)?;
        let mut r = Responder::new(prologue, &host.noise_private, psk_resp)?;
        let m1 = i.write_first(b"")?;
        let (rs, _) = r.read_first(&m1)?;
        assert_eq!(rs, dev.public());
        let (m2, rsess) = r.write_second(b"{\"host_name\":\"x\"}")?;
        let (payload, isess) = i.read_second(&m2)?;
        assert_eq!(payload, b"{\"host_name\":\"x\"}");
        Ok((isess, rsess))
    }

    #[test]
    fn ik_roundtrip_and_chunking() {
        let (mut a, mut b) = pair(None, None).unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let frames = a.encrypt(&big).unwrap();
        assert_eq!(frames.len(), 4);
        let mut out = None;
        for f in &frames {
            assert!(f.len() <= NOISE_MAX);
            out = b.decrypt(f).unwrap();
        }
        assert_eq!(out.unwrap(), big);
        let back = b.encrypt(b"").unwrap();
        assert_eq!(a.decrypt(&back[0]).unwrap().unwrap(), b"");
    }

    #[test]
    fn psk_mismatch_fails() {
        assert!(pair(Some(&[1; 32]), Some(&[1; 32])).is_ok());
        assert!(pair(Some(&[1; 32]), Some(&[2; 32])).is_err());
    }

    #[test]
    fn prologue_mismatch_fails() {
        let host = HostKeys::generate();
        let dev = DeviceKey::generate();
        let mut i = Initiator::new(b"a", &dev.private, &host.noise_public(), None).unwrap();
        let mut r = Responder::new(b"b", &host.noise_private, None).unwrap();
        let m1 = i.write_first(b"").unwrap();
        assert!(r.read_first(&m1).is_err());
    }

    #[test]
    fn replayed_frame_fails() {
        let (mut a, mut b) = pair(None, None).unwrap();
        let f = a.encrypt(b"x").unwrap().remove(0);
        assert!(b.decrypt(&f).unwrap().is_some());
        assert!(b.decrypt(&f).is_err());
    }
}
