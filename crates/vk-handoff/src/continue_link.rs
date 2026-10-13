//! Continue links (spec 20 §4.2): `<app>/#/continue?d=<b64url json ContinueLink>`. The link names
//! the encrypted bundle on the account server and carries its key and a short brief, so a teammate
//! sees what the session is before anything is downloaded.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use crate::stream::KEY_LEN;
use crate::{Error, Result};

/// Link format version.
pub const VERSION: u32 = 1;
/// A longer link is refused at creation (after the brief is truncated).
pub const MAX_URL: usize = 4096;
/// The agent's last message in the brief, in characters.
pub const MAX_LAST: usize = 280;
/// Other brief fields, in characters.
pub const MAX_FIELD: usize = 200;
/// Route of the continue screen in the apps.
pub const ROUTE: &str = "#/continue?d=";

/// What the receiving screen shows before anything is downloaded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    /// Repository name.
    #[serde(default)]
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `claude`, `codex`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Task or pane title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The agent's last message, at most [`MAX_LAST`] characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// The sender's GitHub login (as the sender's host knows it; the receiver trusts the account
    /// server's blob metadata instead).
    #[serde(default)]
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinueLink {
    pub v: u32,
    /// Account server origin.
    pub server: String,
    /// Blob id on the account server.
    pub blob: String,
    /// 32-byte key, base64url without padding.
    pub k: String,
    /// Lowercase hex sha256 of the plaintext bundle.
    pub sha256: String,
    /// Plaintext bundle size.
    pub size: u64,
    /// Unix seconds when the blob expires.
    pub exp: u64,
    pub brief: Brief,
}

fn bad(m: impl Into<String>) -> Error {
    Error::new("invalid_params", m)
}

/// `s` without control characters, at most `max` characters; longer text ends with `…`.
fn cut(s: &str, max: usize) -> String {
    let clean: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect();
    let clean = clean.trim();
    if clean.chars().count() <= max {
        return clean.to_string();
    }
    let mut out: String = clean.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn cut_opt(s: &Option<String>, max: usize) -> Option<String> {
    s.as_deref().map(|v| cut(v, max)).filter(|v| !v.is_empty())
}

impl Brief {
    /// The brief with every field cleaned and within its limit.
    pub fn bounded(&self) -> Brief {
        Brief {
            repo: cut(&self.repo, MAX_FIELD),
            branch: cut_opt(&self.branch, MAX_FIELD),
            harness: cut_opt(&self.harness, 40),
            title: cut_opt(&self.title, MAX_FIELD),
            last: cut_opt(&self.last, MAX_LAST),
            from: cut(&self.from, 100),
        }
    }
}

/// A blob id travels in URL paths: short, and only `[A-Za-z0-9_-]`.
pub fn valid_blob_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl ContinueLink {
    pub fn new(
        server: &str,
        blob: &str,
        key: &[u8; KEY_LEN],
        sha256: &str,
        size: u64,
        exp: u64,
        brief: Brief,
    ) -> ContinueLink {
        ContinueLink {
            v: VERSION,
            server: server.trim_end_matches('/').to_string(),
            blob: blob.to_string(),
            k: URL_SAFE_NO_PAD.encode(key),
            sha256: sha256.to_ascii_lowercase(),
            size,
            exp,
            brief,
        }
    }

    /// The key from `k`.
    pub fn key(&self) -> Result<[u8; KEY_LEN]> {
        let raw = URL_SAFE_NO_PAD
            .decode(self.k.trim_end_matches('='))
            .map_err(|_| bad("the link's key is not base64url"))?;
        raw.try_into()
            .map_err(|_| bad("the link's key is not 32 bytes"))
    }

    /// The `d` parameter: base64url (no padding) of the JSON.
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("a link serializes"))
    }

    /// `<app_url>/#/continue?d=<encode()>`.
    pub fn to_url(&self, app_url: &str) -> String {
        format!("{}/{ROUTE}{}", app_url.trim_end_matches('/'), self.encode())
    }

    /// Fit the link into [`MAX_URL`]: bound the brief, then shorten the last message, then drop
    /// it, then drop the title. `too_large` when even that is too long.
    pub fn fit(&mut self, app_url: &str) -> Result<String> {
        self.brief = self.brief.bounded();
        loop {
            let url = self.to_url(app_url);
            if url.len() <= MAX_URL {
                return Ok(url);
            }
            let over = url.len() - MAX_URL;
            if let Some(last) = self.brief.last.clone() {
                // base64 makes 3 bytes 4 characters; a character is at least one byte.
                let n = last.chars().count();
                let keep = n.saturating_sub(over * 3 / 4 + 1);
                self.brief.last = (keep >= 20).then(|| cut(&last, keep));
                continue;
            }
            if self.brief.title.take().is_some() {
                continue;
            }
            return Err(Error::new(
                "too_large",
                format!("the continue link would be longer than {MAX_URL} bytes"),
            ));
        }
    }

    /// A link as a whole URL (`…#/continue?d=…`, any origin) or just its `d` value.
    pub fn parse(s: &str) -> Result<ContinueLink> {
        let s = s.trim();
        let d = match s.find("#/continue?") {
            Some(i) => {
                let query = &s[i + "#/continue?".len()..];
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("d="))
                    .ok_or_else(|| bad("the link has no d= parameter"))?
            }
            None if s.contains("://") || s.contains('#') => {
                return Err(bad("not a continue link (#/continue?d=…)"));
            }
            None => s,
        };
        if d.len() > MAX_URL {
            return Err(bad("the link is too long"));
        }
        let json = URL_SAFE_NO_PAD
            .decode(d.trim_end_matches('='))
            .map_err(|_| bad("the link's d= is not base64url"))?;
        let link: ContinueLink = serde_json::from_slice(&json)
            .map_err(|e| bad(format!("the link is unreadable: {e}")))?;
        link.check()?;
        Ok(link)
    }

    /// Shape checks a receiver makes before using a link.
    pub fn check(&self) -> Result<()> {
        if self.v != VERSION {
            return Err(Error::new(
                "unsupported",
                format!("continue link version {} needs a newer Vibeke", self.v),
            ));
        }
        if !valid_blob_id(&self.blob) {
            return Err(bad("the link's blob id is invalid"));
        }
        if !(self.server.starts_with("https://") || self.server.starts_with("http://")) {
            return Err(bad("the link's account server is not an http(s) origin"));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("the link's sha256 is not 64 hex digits"));
        }
        self.key()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> ContinueLink {
        ContinueLink::new(
            "https://cloud.example/",
            "b_01HZX",
            &[7u8; 32],
            &"AB".repeat(32),
            1234,
            1_800_000_000,
            Brief {
                repo: "app".into(),
                branch: Some("feat/login".into()),
                harness: Some("claude".into()),
                title: Some("Login form".into()),
                last: Some("Done. The form validates.".into()),
                from: "alice".into(),
            },
        )
    }

    #[test]
    fn round_trips_as_url_and_bare() {
        let l = link();
        assert_eq!(l.server, "https://cloud.example");
        assert_eq!(l.sha256, "ab".repeat(32));
        let url = l.to_url("https://app.example/");
        assert!(
            url.starts_with("https://app.example/#/continue?d="),
            "{url}"
        );
        assert_eq!(ContinueLink::parse(&url).unwrap(), l);
        assert_eq!(ContinueLink::parse(&l.encode()).unwrap(), l);
        assert_eq!(
            ContinueLink::parse(&format!("  {url}&x=1  ")).unwrap(),
            l,
            "extra params and whitespace"
        );
        assert_eq!(l.key().unwrap(), [7u8; 32]);
    }

    #[test]
    fn the_json_has_the_wire_field_names() {
        let v: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(link().encode()).unwrap()).unwrap();
        for k in ["v", "server", "blob", "k", "sha256", "size", "exp", "brief"] {
            assert!(v.get(k).is_some(), "{k}");
        }
        for k in ["repo", "branch", "harness", "title", "last", "from"] {
            assert!(v["brief"].get(k).is_some(), "brief.{k}");
        }
        // Missing optional brief fields parse.
        let mut bare = v.clone();
        bare["brief"] = serde_json::json!({"repo": "app"});
        let d = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&bare).unwrap());
        let l = ContinueLink::parse(&d).unwrap();
        assert_eq!(l.brief.branch, None);
        assert_eq!(l.brief.from, "");
    }

    #[test]
    fn refuses_bad_links() {
        let l = link();
        let bad_with = |f: &dyn Fn(&mut ContinueLink)| {
            let mut x = l.clone();
            f(&mut x);
            ContinueLink::parse(&x.encode()).unwrap_err()
        };
        assert_eq!(bad_with(&|x| x.v = 2).kind, "unsupported");
        assert_eq!(bad_with(&|x| x.blob = "../x".into()).kind, "invalid_params");
        assert_eq!(bad_with(&|x| x.blob = String::new()).kind, "invalid_params");
        assert_eq!(bad_with(&|x| x.k = "AAAA".into()).kind, "invalid_params");
        assert_eq!(bad_with(&|x| x.sha256 = "zz".into()).kind, "invalid_params");
        assert_eq!(
            bad_with(&|x| x.server = "file:///etc".into()).kind,
            "invalid_params"
        );
        for s in [
            "https://app.example/#/pair?d=abc",
            "https://app.example/#/continue?x=1",
            "!!!",
            "",
        ] {
            assert!(ContinueLink::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn the_last_message_is_bounded_to_280_characters() {
        let mut l = link();
        l.brief.last = Some("é".repeat(1000));
        let url = l.fit("https://app.example").unwrap();
        assert!(url.len() <= MAX_URL);
        let last = l.brief.last.clone().unwrap();
        assert_eq!(last.chars().count(), MAX_LAST);
        assert!(last.ends_with('…'));
        assert_eq!(ContinueLink::parse(&url).unwrap(), l);
    }

    #[test]
    fn a_long_link_is_truncated_then_refused() {
        // A long app URL leaves little room: the last message shrinks, then goes.
        let mut l = link();
        l.brief.last = Some("x".repeat(MAX_LAST));
        let app = format!("https://app.example/{}", "p".repeat(3500));
        let url = l.fit(&app).unwrap();
        assert!(url.len() <= MAX_URL, "{}", url.len());
        assert!(
            l.brief
                .last
                .as_ref()
                .is_none_or(|s| s.chars().count() < MAX_LAST)
        );
        // Nothing left to drop.
        let mut l = link();
        let app = format!("https://app.example/{}", "p".repeat(MAX_URL));
        let e = l.fit(&app).unwrap_err();
        assert_eq!(e.kind, "too_large");
        assert!(l.brief.title.is_none() && l.brief.last.is_none());
    }
}
