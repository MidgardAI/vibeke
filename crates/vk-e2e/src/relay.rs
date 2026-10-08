//! Relay control messages and host-ownership signatures (spec 16 §6.1).

use serde::{Deserialize, Serialize};

use crate::b64;

const HOST_AUTH: &[u8] = b"vibeke-relay/1 host-auth\0";
const ACCEPT: &[u8] = b"vibeke-relay/1 accept\0";
const TICKET: &[u8] = b"vibeke-relay/1 ticket\0";

/// Messages on the host control socket and the first frame of a data socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Ctrl {
    /// relay → host: socket-bound nonce and the relay's canonical origin.
    Challenge { nonce: String, origin: String },
    /// host → relay
    Auth {
        host: String,
        #[serde(rename = "pub")]
        public: String,
        sig: String,
    },
    /// relay → host: registered under this generation.
    Ok { host: String, generation: u64 },
    /// relay → host: a client is waiting on `/v1/connect`.
    Incoming { conn: String, generation: u64 },
    /// host → relay (first frame on `/v1/accept`).
    Accept {
        host: String,
        conn: String,
        generation: u64,
        sig: String,
    },
    /// relay → host before closing.
    Error { code: u16, reason: String },
    /// host → relay: stop accepting tickets for this subject (`dev:<id>` or `pid:<id>`).
    Revoke { sub: String },
}

impl Ctrl {
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("ctrl serializes")
    }
    pub fn parse(s: &str) -> crate::Result<Ctrl> {
        serde_json::from_str(s).map_err(|e| crate::Error::Bad(format!("ctrl: {e}")))
    }
}

/// Signed by the host: `"vibeke-relay/1 host-auth\0" ‖ origin ‖ "\0" ‖ nonce` (spec 16 §6.2).
pub fn host_auth_message(origin: &str, nonce: &[u8]) -> Vec<u8> {
    [HOST_AUTH, origin.as_bytes(), b"\0", nonce].concat()
}

/// Signed by the host: `"vibeke-relay/1 accept\0" ‖ origin ‖ "\0" ‖ host ‖ "\0" ‖ gen ‖ "\0" ‖ conn` (§6.3).
pub fn accept_message(origin: &str, host: &str, generation: u64, conn: &str) -> Vec<u8> {
    [
        ACCEPT,
        origin.as_bytes(),
        b"\0",
        host.as_bytes(),
        b"\0",
        generation.to_string().as_bytes(),
        b"\0",
        conn.as_bytes(),
    ]
    .concat()
}

// ---------------------------------------------------------------------------------------------
// Device tickets (spec 16 §6.6)

/// A host-signed admission ticket a device presents on `/v1/connect?ticket=`.
///
/// Wire form: `base64url(json) "." base64url(sig)`, where the JSON is
/// `{"v":1,"host":<host id>,"sub":<subject>,"exp":<unix seconds>}` (field order fixed) and the
/// signature is Ed25519 by the host's relay key over [`ticket_message`]. Subjects are
/// `dev:<device id>` for paired devices and `pid:<pairing id>` for pairing links. The relay verifies
/// the signature against the public key the host registered with, so a ticket is only valid while
/// that host is online and only for that host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub v: u32,
    pub host: String,
    pub sub: String,
    pub exp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    Malformed,
    BadSubject,
    WrongHost,
    BadSignature,
    Expired,
}

impl TicketError {
    /// The `reason` sent to the device in the plaintext error before close 4401.
    pub fn reason(self) -> &'static str {
        match self {
            TicketError::Malformed | TicketError::BadSubject => "ticket_invalid",
            TicketError::WrongHost | TicketError::BadSignature => "ticket_invalid",
            TicketError::Expired => "ticket_expired",
        }
    }
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TicketError::Malformed => "malformed ticket",
            TicketError::BadSubject => "bad ticket subject",
            TicketError::WrongHost => "ticket for another host",
            TicketError::BadSignature => "bad ticket signature",
            TicketError::Expired => "ticket expired",
        })
    }
}

impl std::error::Error for TicketError {}

/// `sub` must be `dev:<id>` or `pid:<id>` with `<id>` in `[A-Za-z0-9_-]{1,64}`.
pub fn valid_ticket_subject(sub: &str) -> bool {
    let Some(id) = sub
        .strip_prefix("dev:")
        .or_else(|| sub.strip_prefix("pid:"))
    else {
        return false;
    };
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Signed by the host: `"vibeke-relay/1 ticket\0" ‖ host ‖ "\0" ‖ sub ‖ "\0" ‖ exp`.
pub fn ticket_message(host: &str, sub: &str, exp: u64) -> Vec<u8> {
    [
        TICKET,
        host.as_bytes(),
        b"\0",
        sub.as_bytes(),
        b"\0",
        exp.to_string().as_bytes(),
    ]
    .concat()
}

/// Issue a ticket for `sub` on this host, valid until `exp` (unix seconds).
pub fn sign_ticket(keys: &crate::HostKeys, sub: &str, exp: u64) -> String {
    let host = keys.host_id();
    let t = Ticket {
        v: crate::VERSION,
        host: host.clone(),
        sub: sub.to_string(),
        exp,
    };
    let sig = keys.sign(&ticket_message(&host, sub, exp));
    format!(
        "{}.{}",
        b64::encode(serde_json::to_vec(&t).expect("ticket serializes")),
        b64::encode(sig)
    )
}

/// Split a ticket string into its claims and signature without verifying either.
pub fn parse_ticket(s: &str) -> Result<(Ticket, [u8; 64]), TicketError> {
    if s.len() > 1024 {
        return Err(TicketError::Malformed);
    }
    let (body, sig) = s.split_once('.').ok_or(TicketError::Malformed)?;
    let body = b64::decode(body).map_err(|_| TicketError::Malformed)?;
    let t: Ticket = serde_json::from_slice(&body).map_err(|_| TicketError::Malformed)?;
    if t.v != crate::VERSION {
        return Err(TicketError::Malformed);
    }
    if !valid_ticket_subject(&t.sub) {
        return Err(TicketError::BadSubject);
    }
    let sig = b64::decode_array::<64>(sig).map_err(|_| TicketError::Malformed)?;
    Ok((t, sig))
}

/// Verify `s` as a ticket for `host`, signed by `relay_pub`, unexpired at `now`.
pub fn verify_ticket(
    relay_pub: &[u8; 32],
    host: &str,
    s: &str,
    now: u64,
) -> Result<Ticket, TicketError> {
    let (t, sig) = parse_ticket(s)?;
    if t.host != host {
        return Err(TicketError::WrongHost);
    }
    if !crate::keys::verify(relay_pub, &ticket_message(&t.host, &t.sub, t.exp), &sig) {
        return Err(TicketError::BadSignature);
    }
    if t.exp <= now {
        return Err(TicketError::Expired);
    }
    Ok(t)
}

/// Canonical origin: lowercase `scheme://host[:port]`, `ws`→`http`, `wss`→`https`, default ports
/// elided, no path. Both sides canonicalize before signing/verifying.
pub fn canonical_origin(url: &str) -> crate::Result<String> {
    let bad = || crate::Error::Bad(format!("origin: {url}"));
    let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        _ => return Err(bad()),
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if authority.is_empty() || authority.contains('@') {
        return Err(bad());
    }
    let authority = match (scheme, authority.rsplit_once(':')) {
        ("http", Some((h, "80"))) | ("https", Some((h, "443"))) => h.to_string(),
        _ => authority,
    };
    Ok(format!("{scheme}://{authority}"))
}

/// Close codes (spec 16 §6.1).
pub mod close {
    pub const BAD_REQUEST: u16 = 4400;
    pub const UNAUTHORIZED: u16 = 4401;
    pub const HOST_OFFLINE: u16 = 4404;
    pub const ACCEPT_TIMEOUT: u16 = 4408;
    pub const REPLACED: u16 = 4409;
    pub const TOO_LARGE: u16 = 4413;
    pub const RATE_LIMITED: u16 = 4429;
    pub const DRAINING: u16 = 4503;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert_eq!(
            canonical_origin("wss://Relay.Example.com:443/v1/host").unwrap(),
            "https://relay.example.com"
        );
        assert_eq!(
            canonical_origin("ws://localhost:8787").unwrap(),
            "http://localhost:8787"
        );
        assert_eq!(
            canonical_origin("https://r.example/").unwrap(),
            "https://r.example"
        );
        assert!(canonical_origin("ftp://x").is_err());
        assert!(canonical_origin("https://u@x").is_err());
    }

    #[test]
    fn ctrl_json_shape() {
        let c = Ctrl::Auth {
            host: "h".into(),
            public: "p".into(),
            sig: "s".into(),
        };
        assert_eq!(
            c.to_text(),
            r#"{"t":"auth","host":"h","pub":"p","sig":"s"}"#
        );
        assert_eq!(Ctrl::parse(&c.to_text()).unwrap(), c);
        let r = Ctrl::Revoke {
            sub: "dev:d1".into(),
        };
        assert_eq!(r.to_text(), r#"{"t":"revoke","sub":"dev:d1"}"#);
    }

    #[test]
    fn ticket_roundtrip_and_failures() {
        let keys = crate::HostKeys::generate();
        let host = keys.host_id();
        let t = sign_ticket(&keys, "dev:abc-1", 1_000);
        // Wire shape: two base64url parts, URL-safe characters only.
        assert_eq!(t.matches('.').count(), 1);
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        );
        let body = b64::decode(t.split('.').next().unwrap()).unwrap();
        assert_eq!(
            String::from_utf8(body).unwrap(),
            format!(r#"{{"v":1,"host":"{host}","sub":"dev:abc-1","exp":1000}}"#)
        );
        let ok = verify_ticket(&keys.relay_public(), &host, &t, 999).unwrap();
        assert_eq!(ok.sub, "dev:abc-1");
        assert_eq!(
            verify_ticket(&keys.relay_public(), &host, &t, 1_000),
            Err(TicketError::Expired)
        );
        let other = crate::HostKeys::generate();
        assert_eq!(
            verify_ticket(&other.relay_public(), &host, &t, 1),
            Err(TicketError::BadSignature)
        );
        assert_eq!(
            verify_ticket(&keys.relay_public(), "zzzz", &t, 1),
            Err(TicketError::WrongHost)
        );
        // Tamper with the claims.
        let forged = sign_ticket(&other, "dev:abc-1", 1_000);
        let (body, _) = forged.split_once('.').unwrap();
        let (_, sig) = t.split_once('.').unwrap();
        assert_eq!(
            verify_ticket(&keys.relay_public(), &host, &format!("{body}.{sig}"), 1),
            Err(TicketError::WrongHost)
        );
        assert_eq!(parse_ticket("nope"), Err(TicketError::Malformed));
        assert_eq!(parse_ticket(""), Err(TicketError::Malformed));
        let bad_sub = sign_ticket(&keys, "user:x", 1_000);
        assert_eq!(parse_ticket(&bad_sub), Err(TicketError::BadSubject));
        assert!(valid_ticket_subject("pid:p_1-2"));
        assert!(!valid_ticket_subject("dev:"));
        assert!(!valid_ticket_subject("dev:a b"));
        assert_eq!(TicketError::Expired.reason(), "ticket_expired");
    }
}
