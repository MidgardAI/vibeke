//! Capability-scoped plugin tokens (07 §7.2–7.3, 09 §3.2 `plugin` identity, 09 §6).
//!
//! * A process plugin gets one token for its process lifetime (`VIBEKE_PLUGIN_TOKEN`, and the
//!   same identity on its stdio channel); argv actions and `[[on]]` hooks get a short-lived
//!   token (60 s).
//! * Only a hash is kept. A connection that presents a token in `client.hello` becomes
//!   `kind = "native-plugin:<id>#<hash>"`; every call re-checks that the token still exists,
//!   has not expired, and that the plugin is still active under the very consent it was issued
//!   for (disable, removal, revocation or a widened manifest cut it off at once).

use super::{PLUGIN_KIND, state};
use crate::Server;
use std::time::{Duration, Instant};
use vk_compat::native::caps::Capabilities;

/// Lifetime of an argv action / hook token (09 §6).
pub const SHORT_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Process,
    Action,
    Hook,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Process => "process",
            TokenKind::Action => "action",
            TokenKind::Hook => "hook",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub plugin: String,
    pub consent_id: String,
    /// The capabilities in force when it was issued (the manifest's, within the consent).
    pub caps: Capabilities,
    pub expires: Option<Instant>,
    pub kind: TokenKind,
    /// The invocation (command record id) or process incarnation it belongs to.
    pub invocation: String,
}

fn hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex()[..32].to_string()
}

/// Issue a token; returns `(token, ctx kind)`.
pub fn issue(server: &Server, info: TokenInfo) -> (String, String) {
    let raw: String = (0..32)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    let token = format!("vkp_{raw}");
    let h = hash(&token);
    let kind = format!("{PLUGIN_KIND}{}#{h}", info.plugin);
    state(server).tokens.lock().unwrap().insert(h, info);
    (token, kind)
}

/// The ctx kind for a presented token, when it is a live plugin token.
pub fn kind_for(server: &Server, token: &str) -> Option<String> {
    if !token.starts_with("vkp_") {
        return None;
    }
    let h = hash(token);
    let info = state(server).tokens.lock().unwrap().get(&h).cloned()?;
    if info.expires.is_some_and(|e| Instant::now() >= e) {
        return None;
    }
    Some(format!("{PLUGIN_KIND}{}#{h}", info.plugin))
}

/// `(plugin, hash)` of a plugin ctx kind.
pub fn parse_kind(kind: &str) -> Option<(&str, &str)> {
    kind.strip_prefix(PLUGIN_KIND)?.rsplit_once('#')
}

/// The token behind a ctx kind, if it is still valid (exists, not expired).
pub fn lookup(server: &Server, kind: &str) -> Option<TokenInfo> {
    let (plugin, h) = parse_kind(kind)?;
    let st = state(server);
    let mut t = st.tokens.lock().unwrap();
    let info = t.get(h)?.clone();
    if info.plugin != plugin {
        return None;
    }
    if info.expires.is_some_and(|e| Instant::now() >= e) {
        t.remove(h);
        return None;
    }
    Some(info)
}

/// Drop one token (process exit, invocation finished long after its TTL anyway). Open
/// connections, subscriptions and render streams holding it end (auth epoch).
pub fn revoke_kind(server: &Server, kind: &str) {
    if let Some((_, h)) = parse_kind(kind) {
        let gone = state(server).tokens.lock().unwrap().remove(h).is_some();
        if gone {
            crate::auth::bump_epoch(server);
        }
    }
}

/// Drop every token of `plugin` (disable, removal, revocation, consent change).
pub fn revoke_plugin(server: &Server, plugin: &str) -> usize {
    let st = state(server);
    let mut t = st.tokens.lock().unwrap();
    let before = t.len();
    t.retain(|_, i| i.plugin != plugin);
    let n = before - t.len();
    drop(t);
    if n > 0 {
        crate::auth::bump_epoch(server);
    }
    n
}

/// Drop expired tokens (housekeeping).
pub fn sweep(server: &Server) {
    let now = Instant::now();
    state(server)
        .tokens
        .lock()
        .unwrap()
        .retain(|_, i| i.expires.is_none_or(|e| e > now));
}
