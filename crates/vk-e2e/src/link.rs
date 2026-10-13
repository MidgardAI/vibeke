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
    /// Present on share/handoff invitations (spec 16 §15): `{kind, scope, until, label}`, plus
    /// `access`, `uses`, `notes`, `preview` and `sid` (spec 20 §2.1). Kept as JSON so a newer
    /// gateway's fields survive; [`PairingLink::share_info`] reads the known ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<serde_json::Value>,
    /// Relay admission ticket for the pairing connection (`pid:<pid>` subject, see
    /// [`crate::relay::Ticket`]). Absent for relays that do not require tickets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tk: Option<String>,
}

/// The preview a review link opens (spec 20 §3.5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkPreview {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// The known fields of [`PairingLink::share`]. Every field is optional, so links from older
/// gateways parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkShare {
    /// `share`, `review`, `handoff` or `peer`.
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub until: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `link` (first opener, the default) or `github` (attested GitHub accounts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    /// How many devices may redeem the link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses: Option<u32>,
    /// Review links: `direct` or `held`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<LinkPreview>,
    /// Review links: the content origin's label (`<sid>.p.<app domain>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

impl LinkShare {
    /// The opener must sign in with GitHub first.
    pub fn github(&self) -> bool {
        self.access.as_deref() == Some("github")
    }
}

impl PairingLink {
    /// The known share fields, when this is an invitation and they parse.
    pub fn share_info(&self) -> Option<LinkShare> {
        serde_json::from_value(self.share.clone()?).ok()
    }

    /// `<app>/#/review?d=<base64url(json)>` (spec 20 §2.1).
    pub fn to_review_url(&self, app_base: &str) -> String {
        let d = b64::encode(serde_json::to_vec(self).expect("link serializes"));
        format!("{}/#/review?d={d}", app_base.trim_end_matches('/'))
    }

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
            tk: None,
        };
        let url = l.to_url("https://r.example/");
        assert!(url.starts_with("https://r.example/#/pair?d="));
        assert_eq!(PairingLink::parse(&url).unwrap(), l);
        let d = url.split("d=").nth(1).unwrap();
        assert_eq!(PairingLink::parse(d).unwrap(), l);
        let with_ticket = PairingLink {
            tk: Some("a.b".into()),
            ..l.clone()
        };
        let url = with_ticket.to_url("https://r.example");
        assert_eq!(PairingLink::parse(&url).unwrap(), with_ticket);
        assert!(!l.to_url("https://r.example").contains("tk"));
    }

    #[test]
    fn share_fields_are_optional() {
        let base = PairingLink {
            v: 1,
            relay: "wss://r.example".into(),
            host: "abc".into(),
            hk: b64::encode([1u8; 32]),
            pid: "p1".into(),
            psk: b64::encode([2u8; 32]),
            exp: 99,
            name: "devbox".into(),
            share: None,
            tk: None,
        };
        assert_eq!(base.share_info(), None);
        // An older gateway's share object.
        let old = PairingLink {
            share: Some(
                serde_json::json!({"kind": "share", "scope": "view", "until": 5,
                                           "label": null, "limit": {"pane": "p1"}}),
            ),
            ..base.clone()
        };
        let info = old.share_info().unwrap();
        assert_eq!((info.kind.as_str(), info.until), ("share", 5));
        assert_eq!((info.access, info.uses, info.preview), (None, None, None));
        assert!(!old.share_info().unwrap().github());
        // A review link carries the new fields and its own route.
        let review = PairingLink {
            share: Some(
                serde_json::json!({"kind": "review", "scope": "view", "until": 5,
                "access": "github", "uses": 20, "notes": "direct",
                "preview": {"id": "pv1", "label": "web"}, "sid": "abcdefghijklmnop"}),
            ),
            ..base
        };
        let info = review.share_info().unwrap();
        assert!(info.github());
        assert_eq!(info.uses, Some(20));
        assert_eq!(info.notes.as_deref(), Some("direct"));
        assert_eq!(
            info.preview,
            Some(LinkPreview {
                id: "pv1".into(),
                label: Some("web".into())
            })
        );
        assert_eq!(info.sid.as_deref(), Some("abcdefghijklmnop"));
        let url = review.to_review_url("https://app.example/");
        assert!(url.starts_with("https://app.example/#/review?d="));
        assert_eq!(PairingLink::parse(&url).unwrap(), review);
    }
}
