//! Box names and owner tags (spec 17 §6.1). Every box Vibeke creates carries the host that owns
//! it and the box key (task id or ad-hoc run key), so the reconciler can tell its own boxes
//! from a foreign host's and from boxes Vibeke never made. Providers without metadata (Sprites)
//! carry the tags in the name: `vk-<host>-<key>`, lowercase, at most 63 characters.

use serde::{Deserialize, Serialize};

/// Name prefix of every Vibeke box.
pub const PREFIX: &str = "vk-";

/// Owner tags: short hashes, safe in a DNS label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Tags {
    /// 8 hex characters of the owning host's id ([`host_tag`]).
    pub host: String,
    /// 10 hex characters of the box key ([`key_tag`]).
    pub key: String,
}

fn short_hash(s: &str, n: usize) -> String {
    let h = blake3::hash(s.as_bytes());
    h.to_hex()[..n].to_string()
}

/// Tag for a host id (the server's stable host id).
pub fn host_tag(host_id: &str) -> String {
    short_hash(host_id, 8)
}

/// Tag for a box key (task id or ad-hoc key).
pub fn key_tag(key: &str) -> String {
    short_hash(key, 10)
}

impl Tags {
    pub fn new(host_id: &str, key: &str) -> Tags {
        Tags {
            host: host_tag(host_id),
            key: key_tag(key),
        }
    }
}

/// `vk-<host>-<key>`.
pub fn box_name(t: &Tags) -> String {
    format!("{PREFIX}{}-{}", t.host, t.key)
}

/// Parse a name made by [`box_name`]; `None` for any other name.
pub fn parse_name(name: &str) -> Option<Tags> {
    let rest = name.strip_prefix(PREFIX)?;
    let (host, key) = rest.split_once('-')?;
    let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(host, 8) && hex(key, 10)).then(|| Tags {
        host: host.to_string(),
        key: key.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        let t = Tags::new("host-1", "01JTASK");
        let n = box_name(&t);
        assert!(n.starts_with("vk-") && n.len() == 3 + 8 + 1 + 10);
        assert_eq!(parse_name(&n), Some(t));
        assert_eq!(parse_name("my-sprite"), None);
        assert_eq!(parse_name("vk-zzzzzzzz-0123456789"), None);
    }
}
