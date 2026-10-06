//! The pairing link carried by the QR code (spec 16 §4.1). The payload lives in the URL fragment,
//! which browsers never send to the server hosting the app.

use serde::{Deserialize, Serialize};

use crate::b64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingLink {
    pub v: u32,
    /// Relay WebSocket base, e.g. `wss://relay.example.com`.
    pub relay: String,
    /// Host id on the relay.
    pub host: String,
    /// Host Noise static public key (base64url).
    pub hk: String,
    /// Pairing id (not secret).
    pub pid: String,
    /// Pairing secret (base64url, 32 bytes).
    pub psk: String,
    /// Expiry, unix seconds.
    pub exp: u64,
    /// Host display name.
    pub name: String,
    /// Present on share/handoff invitations (spec 16 §15): `{kind, scope, until, label}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<serde_json::Value>,
}

impl PairingLink {
    /// `<app>/#/pair?d=<base64url(json)>`
    pub fn to_url(&self, app_base: &str) -> String {
        let d = b64::encode(serde_json::to_vec(self).expect("link serializes"));
        format!("{}/#/pair?d={d}", app_base.trim_end_matches('/'))
    }

    /// Accept a full URL or the bare `d` value.
    pub fn parse(s: &str) -> crate::Result<PairingLink> {
        let d = match s.find("d=") {
            Some(i) if s.contains('#') || s.contains('?') => &s[i + 2..],
            _ => s,
        };
        let d = d.split('&').next().unwrap_or(d);
        let link: PairingLink = serde_json::from_slice(&b64::decode(d)?)
            .map_err(|e| crate::Error::Bad(format!("link: {e}")))?;
        if link.v != crate::VERSION {
            return Err(crate::Error::Bad("unsupported_version".into()));
        }
        Ok(link)
    }

    pub fn host_key(&self) -> crate::Result<[u8; 32]> {
        b64::decode_array(&self.hk)
    }
    pub fn psk_bytes(&self) -> crate::Result<[u8; 32]> {
        b64::decode_array(&self.psk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let l = PairingLink {
            v: 1,
            relay: "wss://r.example".into(),
            host: "abc".into(),
            hk: b64::encode([1u8; 32]),
            pid: "p1".into(),
            psk: b64::encode([2u8; 32]),
            exp: 99,
            name: "devbox".into(),
            share: None,
        };
        let url = l.to_url("https://r.example/");
        assert!(url.starts_with("https://r.example/#/pair?d="));
        assert_eq!(PairingLink::parse(&url).unwrap(), l);
        let d = url.split("d=").nth(1).unwrap();
        assert_eq!(PairingLink::parse(d).unwrap(), l);
    }
}
