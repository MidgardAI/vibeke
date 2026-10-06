//! Relay control messages and host-ownership signatures (spec 16 §6.1).

use serde::{Deserialize, Serialize};

const HOST_AUTH: &[u8] = b"vibeke-relay/1 host-auth\0";
const ACCEPT: &[u8] = b"vibeke-relay/1 accept\0";

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
    }
}
