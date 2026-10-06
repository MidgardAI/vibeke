//! Authenticated reverse proxy for the user's normal browser (06 B4, 09 §8).
//!
//! One HTTP/1.1 listener on `127.0.0.1:<port>` and `[::1]:<port>` (when the machine has IPv6
//! loopback; if another process holds `[::1]:<port>` the proxy refuses to start, since a
//! browser may resolve `*.localhost` to `::1` first). Each preview open gets its own origin
//! `http://<handle>-<26 base32 chars>.vibeke.localhost:<port>` (`*.localhost` resolves to
//! loopback in browsers and is a secure context), selected by the `Host` header; unknown hosts
//! get `421` (DNS-rebinding defence). The 26-character label is 128 random bits, generated
//! fresh on every open (a re-open rotates it and revokes the old origin's sessions), so two
//! opens, sessions or users never share a hostname and nobody can guess one.
//!
//! **Why unguessable.** No cookie attribute scopes a cookie by port, and browsers send
//! `Secure` cookies to `http://*.localhost` too: any other local listener serving the same
//! hostname on another port would receive the session cookie after a same-site navigation.
//! The defence is that only the opener learns the hostname (the server never shows it to
//! other pane-scoped callers or puts it in events), and the cookie is bound to the route: its
//! MAC covers the hostname and the scheme it was issued for, so a cookie minted over `https`
//! is refused over `http` and the other way round. The residual risk (spec 09 §8): a process
//! of the same user that learns a hostname some other way can still collect the cookie with a
//! same-site request and replay it until it expires.
//!
//! **Capability.** Opening a preview mints a one-time `vk_token` (60 s). The first navigation
//! carrying it is answered with a `303` to the same URL (absolute, on the preview's own
//! origin) without the token and an unguessable per-preview session cookie
//! (`__Host-vk_preview`: HttpOnly, SameSite=Strict, Secure, host-only, `Path=/`,
//! `Max-Age` = [`SESSION_TTL`]). Sessions end after [`SESSION_TTL`] (8 h) or [`SESSION_IDLE`]
//! (1 h without a request); re-authenticating is one `vibeke preview open <h> --proxy`.
//! There is no non-`Secure` copy. Browsers that drop `Secure` cookies on
//! `http://*.localhost` cannot use proxy mode (use the browser profile instead). A route with
//! `tls: true` (`preview.tls_origin`) is served over HTTPS on the same port: [`Proxy::set_tls`]
//! enables it, a connection starting with a TLS handshake record goes through rustls (a
//! certificate from the local CA, [`crate::ca`], chosen by SNI for registered `tls` hosts only),
//! anything else is plain HTTP; the two kinds of route never answer each other's connections. Every
//! request — including WebSocket upgrades — must carry a session of *that* host and scheme.
//! Only SHA-256 digests of tokens and keyed MACs of sessions are kept, in memory (a server
//! restart revokes everything).
//!
//! **Not leakable cross-origin.** All previews share the site `vibeke.localhost`, so SameSite
//! does not separate them and is not relied on: requests whose `Sec-Fetch-Site` is
//! `same-site` (another preview) are refused; `cross-site` requests are refused unless they
//! are top-level document navigations (`Sec-Fetch-Mode: navigate` and `Sec-Fetch-Dest:
//! document`; an iframe navigation has `Sec-Fetch-Dest: iframe`). Requests with a foreign
//! `Origin` are refused for WebSocket upgrades and non-GET/HEAD methods. Responses the proxy
//! generates itself carry `X-Frame-Options: DENY` and `frame-ancestors 'none'`. The proxy
//! strips `vk_token` and every reserved `vk_` cookie before forwarding, drops upstream
//! `Set-Cookie` for reserved names and strips `Domain=` from the rest (host-only cookies).
//!
//! **Revocation.** Before each forwarded request the route is re-checked with the
//! [`Upstream`] (the preview must still exist, not be gone, and still be on the same port);
//! a route that fails the check is removed together with its sessions (`410`).
//!
//! **Rewriting** (only what B4 lists): `Host` → `localhost:<port>`; `Origin`/`Referer` →
//! the upstream origin when they are the preview's own origin; response `Location` and
//! `Access-Control-Allow-Origin` mapped back. Bodies stream unbuffered (SSE), `Upgrade`
//! requests (WebSocket/HMR) are tunnelled after the `101`.

use crate::socks::{BoxFuture, Stream};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

pub type Body = BoxBody<Bytes, hyper::Error>;

/// The session cookie (spec name). `__Host-` = Secure, host-only, `Path=/`. The only cookie
/// the proxy sets or accepts (see the module docs for why there is no non-`Secure` copy).
pub const COOKIE: &str = "__Host-vk_preview";
/// The one-time query parameter.
pub const TOKEN_PARAM: &str = "vk_token";
/// Parent domain of every preview origin.
pub const DOMAIN: &str = "vibeke.localhost";
pub const TOKEN_TTL: Duration = Duration::from_secs(60);
/// Absolute lifetime of a session (also the cookie's `Max-Age`).
pub const SESSION_TTL: Duration = Duration::from_secs(8 * 3600);
/// A session unused for this long ends.
pub const SESSION_IDLE: Duration = Duration::from_secs(3600);
const MAX_TOKENS: usize = 16;
const MAX_SESSIONS: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One preview origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    /// `v4-<26 base32>.vibeke.localhost` (no port). Chosen by [`Proxy::register`]; whatever
    /// the caller puts here is replaced.
    pub host: String,
    /// Machine label; `local` (or empty) = this machine.
    pub machine: String,
    /// Preview id (ULID) on that machine.
    pub preview: String,
    pub handle: String,
    /// Upstream port on the machine's loopback.
    pub port: u16,
    /// `http` | `https` (TLS to the loopback upstream, certificate not verified).
    pub scheme: String,
    /// `preview.tls_origin`: this origin is served over HTTPS (`https://<host>:<port>`, a leaf
    /// certificate from the local CA) instead of plain HTTP. Plain-HTTP requests to such a
    /// host are refused (`421`), and the other way round.
    pub tls: bool,
}

/// Whether a route still points at the preview it was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteCheck {
    /// The preview exists, is not gone and is still on the route's port.
    Live,
    /// The preview was forgotten/retired or moved: the route and its sessions are revoked.
    Gone,
    /// The preview's machine could not be asked (link down): refuse this request only.
    Unavailable,
}

/// How the proxy reaches a route's upstream (a direct loopback connection, or a bridge `tcp:`
/// channel to the preview's machine).
pub trait Upstream: Send + Sync + 'static {
    fn connect(&self, route: &Route) -> BoxFuture<std::io::Result<Box<dyn Stream>>>;
    /// Re-check the route before a request is forwarded (default: always live).
    fn check(&self, _route: &Route) -> BoxFuture<RouteCheck> {
        Box::pin(async { RouteCheck::Live })
    }
}

type Digest32 = [u8; 32];

fn digest(s: &str) -> Digest32 {
    Sha256::digest(s.as_bytes()).into()
}

fn random_hex() -> String {
    let b: [u8; 32] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Session {
    /// MAC over (scheme, host, cookie value): the cookie is valid for this route only.
    mac: Digest32,
    expires: Instant,
    last_used: Instant,
}

struct Entry {
    route: Route,
    /// The pane that opened this origin (`None`: a full-scope client). Only it (and full-scope
    /// clients) may learn the hostname.
    opened_by: Option<String>,
    tokens: Vec<(Digest32, Instant)>,
    sessions: Vec<Session>,
}

#[derive(Default)]
struct State {
    by_host: HashMap<String, Entry>,
    /// (machine, preview id) → host of its current origin (a re-open replaces it).
    by_preview: HashMap<(String, String), String>,
}

/// Counters for `preview.status`.
#[derive(Debug, Default, Serialize)]
pub struct ProxyStats {
    pub requests: u64,
    pub denied: u64,
    pub websockets: u64,
}

pub struct Proxy {
    state: Mutex<State>,
    /// Per-process key of the session MACs.
    key: [u8; 32],
    port: AtomicU16,
    upstream: Arc<dyn Upstream>,
    /// TLS for `tls_origin` routes on the same port (see [`Proxy::set_tls`]).
    tls: Mutex<Option<tokio_rustls::TlsAcceptor>>,
    requests: AtomicU64,
    denied: AtomicU64,
    websockets: AtomicU64,
}

/// Lower-case `[a-z0-9-]` with single dashes, at most `max` bytes, no leading/trailing dash.
fn dns_part(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(max);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// 128 random bits as 26 lower-case RFC 4648 base32 characters (no padding).
pub fn random_label() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let b: [u8; 16] = rand::random();
    let n = u128::from_be_bytes(b);
    // 26 × 5 = 130 bits: the top two bits of the first character are always zero.
    (0..26)
        .rev()
        .map(|i| ALPHABET[((n >> (i * 5)) & 31) as usize] as char)
        .collect()
}

/// A fresh, unguessable preview hostname: `<handle>-<26 base32 chars>.vibeke.localhost` (one
/// DNS label of at most 43 bytes). Every call differs; [`Proxy::register`] picks one per open.
pub fn hostname_for(handle: &str) -> String {
    let mut label = dns_part(handle, 16);
    if label.is_empty() {
        label.push('p');
    }
    format!("{label}-{}.{DOMAIN}", random_label())
}

/// A cookie name the proxy reserves (`vk_*`, also behind `__Host-`/`__Secure-` prefixes).
pub fn reserved_cookie(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    let n = n
        .strip_prefix("__host-")
        .or_else(|| n.strip_prefix("__secure-"))
        .unwrap_or(&n);
    n.starts_with("vk_")
}

/// An upstream `Set-Cookie` as forwarded: `None` for reserved names; otherwise every `Domain`
/// attribute removed (host-only for the preview's hostname).
pub fn rewrite_set_cookie(v: &str) -> Option<String> {
    let mut parts = v.split(';');
    let first = parts.next()?.trim();
    let name = first.split_once('=').map(|(n, _)| n).unwrap_or(first);
    if name.trim().is_empty() || reserved_cookie(name) {
        return None;
    }
    let mut out = vec![first.to_string()];
    for a in parts {
        let a = a.trim();
        if a.is_empty() {
            continue;
        }
        let key = a.split_once('=').map(|(k, _)| k).unwrap_or(a).trim();
        if key.eq_ignore_ascii_case("domain") {
            continue;
        }
        out.push(a.to_string());
    }
    Some(out.join("; "))
}

/// `Cookie` header without reserved cookies; `None` if nothing is left.
pub fn strip_reserved_cookies(v: &str) -> Option<String> {
    let kept: Vec<&str> = v
        .split(';')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .filter(|c| !reserved_cookie(c.split_once('=').map(|(n, _)| n).unwrap_or(c)))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

/// Remove `name` from a query string: (its first value, the remaining query).
pub fn take_param(query: &str, name: &str) -> (Option<String>, String) {
    let mut found = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|kv| {
            let k = kv.split_once('=').map(|(k, _)| k).unwrap_or(kv);
            if k == name {
                if found.is_none() {
                    found = Some(kv.split_once('=').map(|(_, v)| v).unwrap_or("").to_string());
                }
                false
            } else {
                !kv.is_empty()
            }
        })
        .collect();
    (found, rest.join("&"))
}

fn session_cookies(headers: &HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    for v in headers.get_all(header::COOKIE) {
        let Ok(v) = v.to_str() else { continue };
        for c in v.split(';') {
            if let Some((n, val)) = c.trim().split_once('=')
                && n == COOKIE
            {
                out.push(val.to_string());
            }
        }
    }
    out
}

/// Host header → (lower-case host, port).
fn split_host(h: &str) -> Option<(String, Option<u16>)> {
    let h = h.trim();
    if h.starts_with('[') {
        let (a, rest) = h.split_once(']')?;
        let port = rest.strip_prefix(':').and_then(|p| p.parse().ok());
        return Some((format!("{a}]").to_ascii_lowercase(), port));
    }
    match h.rsplit_once(':') {
        Some((a, p)) => Some((
            a.trim_end_matches('.').to_ascii_lowercase(),
            Some(p.parse().ok()?),
        )),
        None => Some((h.trim_end_matches('.').to_ascii_lowercase(), None)),
    }
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Drop hop-by-hop headers (and those named by `Connection`).
fn strip_hop_by_hop(h: &mut HeaderMap) {
    let named: Vec<String> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|t| t.trim().to_ascii_lowercase()))
        .filter(|t| !t.is_empty())
        .collect();
    for n in HOP_BY_HOP.iter().map(|s| s.to_string()).chain(named) {
        if let Ok(name) = HeaderName::from_bytes(n.as_bytes()) {
            h.remove(name);
        }
    }
}

fn is_upgrade(h: &HeaderMap) -> bool {
    h.contains_key(header::UPGRADE)
        && h.get_all(header::CONNECTION).iter().any(|v| {
            v.to_str().is_ok_and(|v| {
                v.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
        })
}

/// Why a request is refused as cross-origin, if it is (see the module docs).
pub fn foreign_request(
    method: &Method,
    headers: &HeaderMap,
    own_origin: &str,
    upgrade: bool,
) -> Option<&'static str> {
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    match get("sec-fetch-site") {
        // Another `*.vibeke.localhost` origin: a sibling preview (of this or another
        // session). Never allowed, not even as a navigation (a sibling could frame or
        // navigate to this preview to drive GET requests with its cookie).
        Some("same-site") => return Some("request from another preview origin"),
        // Only a top-level document navigation (a link from elsewhere); iframes, workers and
        // subresources are refused.
        Some("cross-site")
            if !(get("sec-fetch-mode") == Some("navigate")
                && get("sec-fetch-dest") == Some("document")) =>
        {
            return Some("cross-origin subresource, frame or fetch");
        }
        _ => {}
    }
    if let Some(origin) = get("origin")
        && !origin.eq_ignore_ascii_case(own_origin)
        && (upgrade || !matches!(*method, Method::GET | Method::HEAD))
    {
        return Some(if upgrade {
            "cross-origin WebSocket"
        } else {
            "cross-origin request"
        });
    }
    None
}

fn upstream_origins(route: &Route) -> Vec<String> {
    let mut v = Vec::new();
    for sch in ["http", "https"] {
        for h in ["localhost", "127.0.0.1", "[::1]"] {
            v.push(format!("{sch}://{h}:{}", route.port));
        }
    }
    v
}

/// Map an upstream absolute URL (`http://localhost:5173/x`) to the preview origin.
fn map_back(v: &str, route: &Route, own_origin: &str) -> Option<String> {
    for o in upstream_origins(route) {
        if let Some(rest) = v.strip_prefix(&o)
            && (rest.is_empty() || rest.starts_with(['/', '?', '#']))
        {
            return Some(format!("{own_origin}{rest}"));
        }
    }
    None
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn full(b: impl Into<Bytes>) -> Body {
    Full::new(b.into()).map_err(|never| match never {}).boxed()
}

fn page(status: StatusCode, title: &str, text: &str) -> Response<Body> {
    let html = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{t}</title></head><body><h1>{t}</h1><p>{x}</p><p><small>Vibeke preview proxy</small></p></body></html>\n",
        t = html_escape(title),
        x = html_escape(text)
    );
    let mut r = Response::new(full(html));
    *r.status_mut() = status;
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    secure_headers(h);
    r
}

fn secure_headers(h: &mut HeaderMap) {
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Proxy pages (login required, link expired, …) and the token exchange are never framed.
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// The upstream connection a downstream connection reuses (HTTP/1: sequential requests).
type Slot = Arc<tokio::sync::Mutex<Option<(String, SendRequest<Body>)>>>;

impl Proxy {
    pub fn new(upstream: Arc<dyn Upstream>) -> Arc<Self> {
        Arc::new(Proxy {
            state: Mutex::default(),
            key: rand::random(),
            port: AtomicU16::new(0),
            upstream,
            tls: Mutex::new(None),
            requests: AtomicU64::new(0),
            denied: AtomicU64::new(0),
            websockets: AtomicU64::new(0),
        })
    }

    /// The listener port (0 until [`Proxy::serve`]).
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> ProxyStats {
        ProxyStats {
            requests: self.requests.load(Ordering::Relaxed),
            denied: self.denied.load(Ordering::Relaxed),
            websockets: self.websockets.load(Ordering::Relaxed),
        }
    }

    /// Serve `tls: true` routes over TLS on the same port: each accepted connection whose first
    /// byte is a TLS handshake record (`0x16`; an HTTP request never starts with it) is handed
    /// to this config, anything else is plain HTTP as before.
    pub fn set_tls(&self, config: Arc<rustls::ServerConfig>) {
        *self.tls.lock().unwrap() = Some(tokio_rustls::TlsAcceptor::from(config));
    }

    pub fn tls_enabled(&self) -> bool {
        self.tls.lock().unwrap().is_some()
    }

    /// Whether `host` is the hostname of a registered `tls` route (the SNI allow-list).
    pub fn is_tls_host(&self, host: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .by_host
            .get(host)
            .is_some_and(|e| e.route.tls)
    }

    /// `http://<host>:<port>`, or `https://…` for a `tls` route.
    pub fn origin(&self, host: &str) -> String {
        let tls = self.is_tls_host(host);
        format!(
            "{}://{host}:{}",
            if tls { "https" } else { "http" },
            self.port()
        )
    }

    /// Register a preview's origin for an open by a full-scope client (see
    /// [`Proxy::register_for`]).
    pub fn register(&self, route: Route) -> Route {
        self.register_for(route, None)
    }

    /// Register a preview's origin for one open: always a fresh random hostname (see
    /// [`hostname_for`]; `route.host` is ignored). A preview opened before loses its previous
    /// origin together with its tokens and sessions (re-open = rotation). `opened_by` is the
    /// pane that opened it (`None` = a full-scope client): [`Proxy::routes_visible_to`] shows
    /// the hostname to nobody else.
    pub fn register_for(&self, mut route: Route, opened_by: Option<String>) -> Route {
        let mut st = self.state.lock().unwrap();
        let key = (route.machine.clone(), route.preview.clone());
        if let Some(old) = st.by_preview.remove(&key) {
            st.by_host.remove(&old);
        }
        let mut host = hostname_for(&route.handle);
        while st.by_host.contains_key(&host) {
            host = hostname_for(&route.handle);
        }
        route.host = host.clone();
        st.by_preview.insert(key, host.clone());
        st.by_host.insert(
            host,
            Entry {
                route: route.clone(),
                opened_by,
                tokens: vec![],
                sessions: vec![],
            },
        );
        route
    }

    /// Routes whose hostname `viewer` may learn: every route for a full-scope client
    /// (`None`), only the routes it opened itself for a pane.
    pub fn routes_visible_to(&self, viewer: Option<&str>) -> Vec<Route> {
        let st = self.state.lock().unwrap();
        let mut v: Vec<Route> = st
            .by_host
            .values()
            .filter(|e| viewer.is_none() || e.opened_by.as_deref() == viewer)
            .map(|e| e.route.clone())
            .collect();
        v.sort_by(|a, b| a.host.cmp(&b.host));
        v
    }

    /// Forget a preview's origin (and its sessions).
    pub fn remove_preview(&self, machine: &str, preview: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(h) = st
            .by_preview
            .remove(&(machine.to_string(), preview.to_string()))
        {
            st.by_host.remove(&h);
        }
    }

    /// Forget every origin of `machine` whose preview id or handle is `target` (a remote
    /// `preview.forget` names the preview by handle or id).
    pub fn remove_matching(&self, machine: &str, target: &str) -> usize {
        let mut st = self.state.lock().unwrap();
        let hosts: Vec<String> = st
            .by_host
            .iter()
            .filter(|(_, e)| {
                e.route.machine == machine
                    && (e.route.preview == target || e.route.handle == target)
            })
            .map(|(h, _)| h.clone())
            .collect();
        for h in &hosts {
            if let Some(e) = st.by_host.remove(h) {
                st.by_preview
                    .remove(&(e.route.machine.clone(), e.route.preview.clone()));
            }
        }
        hosts.len()
    }

    fn remove_host(&self, host: &str) {
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.by_host.remove(host) {
            st.by_preview.remove(&(e.route.machine, e.route.preview));
        }
    }

    pub fn routes(&self) -> Vec<Route> {
        let st = self.state.lock().unwrap();
        let mut v: Vec<Route> = st.by_host.values().map(|e| e.route.clone()).collect();
        v.sort_by(|a, b| a.host.cmp(&b.host));
        v
    }

    /// A one-time token for `host`, valid [`TOKEN_TTL`].
    pub fn mint_token(&self, host: &str) -> Option<String> {
        let mut st = self.state.lock().unwrap();
        let e = st.by_host.get_mut(host)?;
        let now = Instant::now();
        e.tokens.retain(|(_, exp)| *exp > now);
        if e.tokens.len() >= MAX_TOKENS {
            e.tokens.remove(0);
        }
        let t = random_hex();
        e.tokens.push((digest(&t), now + TOKEN_TTL));
        Some(t)
    }

    /// `http(s)://<host>:<port><path>` (+ `?vk_token=…`).
    pub fn url(&self, host: &str, path: &str, token: Option<&str>) -> String {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        // The token goes into the query, before any fragment (a browser never sends the
        // fragment; it keeps it across the exchange redirect).
        let (path, frag) = match path.find('#') {
            Some(i) => (path[..i].to_string(), &path[i..]),
            None => (path.clone(), ""),
        };
        let mut u = format!("{}{path}", self.origin(host));
        if let Some(t) = token {
            u.push(if path.contains('?') { '&' } else { '?' });
            u.push_str(&format!("{TOKEN_PARAM}={t}"));
        }
        u.push_str(frag);
        u
    }

    fn take_token(&self, host: &str, token: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        let Some(e) = st.by_host.get_mut(host) else {
            return false;
        };
        let now = Instant::now();
        e.tokens.retain(|(_, exp)| *exp > now);
        let d = digest(token);
        match e.tokens.iter().position(|(t, _)| *t == d) {
            Some(i) => {
                e.tokens.remove(i);
                true
            }
            None => false,
        }
    }

    /// The MAC binding a session cookie value to the route (host) and the scheme it was
    /// issued over.
    fn session_mac(&self, secure: bool, host: &str, value: &str) -> Digest32 {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("any key length");
        m.update(if secure { b"https\0" } else { b"http\0\0" });
        m.update(host.as_bytes());
        m.update(b"\0");
        m.update(value.as_bytes());
        m.finalize().into_bytes().into()
    }

    /// A new session for `host`, bound to the scheme of the connection that exchanged the token.
    fn new_session(&self, host: &str, secure: bool) -> Option<String> {
        let v = random_hex();
        let mac = self.session_mac(secure, host, &v);
        let mut st = self.state.lock().unwrap();
        let e = st.by_host.get_mut(host)?;
        let now = Instant::now();
        e.sessions
            .retain(|s| s.expires > now && now.duration_since(s.last_used) < SESSION_IDLE);
        if e.sessions.len() >= MAX_SESSIONS {
            e.sessions.remove(0);
        }
        e.sessions.push(Session {
            mac,
            expires: now + SESSION_TTL,
            last_used: now,
        });
        Some(v)
    }

    /// Whether one of `values` is a live session of `host` issued over this scheme (refreshes
    /// its idle timer).
    pub(crate) fn session_ok(&self, host: &str, secure: bool, values: &[String]) -> bool {
        let macs: Vec<Digest32> = values
            .iter()
            .map(|v| self.session_mac(secure, host, v))
            .collect();
        let mut st = self.state.lock().unwrap();
        let Some(e) = st.by_host.get_mut(host) else {
            return false;
        };
        let now = Instant::now();
        e.sessions
            .retain(|s| s.expires > now && now.duration_since(s.last_used) < SESSION_IDLE);
        match e.sessions.iter_mut().find(|s| macs.contains(&s.mac)) {
            Some(s) => {
                s.last_used = now;
                true
            }
            None => false,
        }
    }

    /// Age every session of `host` by `by` (tests: expiry without waiting).
    #[cfg(test)]
    pub(crate) fn age_sessions(&self, host: &str, by: Duration) {
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.by_host.get_mut(host) {
            for s in &mut e.sessions {
                s.last_used = s.last_used.checked_sub(by).unwrap_or(s.last_used);
                s.expires = s.expires.checked_sub(by).unwrap_or(s.expires);
            }
        }
    }

    fn route(&self, host: &str) -> Option<Route> {
        self.state
            .lock()
            .unwrap()
            .by_host
            .get(host)
            .map(|e| e.route.clone())
    }

    /// Bind `127.0.0.1:<port>` (0 = ephemeral) and `[::1]` on the same port. Never a
    /// wildcard address (09 §7). `[::1]` is skipped only when the machine has no IPv6
    /// loopback; when another process already listens there the bind fails with
    /// `AddrInUse` (a browser resolving `*.localhost` to `::1` would reach that process, cookie
    /// included). For an ephemeral port a few candidates are tried.
    pub async fn bind(port: u16) -> std::io::Result<Vec<TcpListener>> {
        let mut last = None;
        for _ in 0..if port == 0 { 8 } else { 1 } {
            let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
            let p = v4.local_addr()?.port();
            match TcpListener::bind((Ipv6Addr::LOCALHOST, p)).await {
                Ok(v6) => return Ok(vec![v4, v6]),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => last = Some(e),
                // No IPv6 loopback on this machine: browsers can't reach `::1` either.
                Err(_) => return Ok(vec![v4]),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::AddrInUse)))
    }

    /// Serve on the listeners from [`Proxy::bind`] (spawns the accept loops).
    pub fn serve(self: &Arc<Self>, listeners: Vec<TcpListener>) {
        if let Some(p) = listeners.first().and_then(|l| l.local_addr().ok()) {
            self.port.store(p.port(), Ordering::Relaxed);
        }
        for l in listeners {
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((s, peer)) = l.accept().await else {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    if !peer.ip().to_canonical().is_loopback() {
                        continue; // unreachable for a loopback bind; defensive
                    }
                    let _ = s.set_nodelay(true);
                    tokio::spawn(me.clone().accept_conn(s, peer));
                }
            });
        }
    }

    /// Plain HTTP, or TLS when a TLS acceptor is set and the client starts a handshake.
    async fn accept_conn(self: Arc<Self>, s: tokio::net::TcpStream, peer: SocketAddr) {
        let acceptor = self.tls.lock().unwrap().clone();
        let Some(acceptor) = acceptor else {
            return self.serve_conn(s, peer, false).await;
        };
        let mut first = [0u8; 1];
        let peeked = tokio::time::timeout(Duration::from_secs(30), s.peek(&mut first)).await;
        if !matches!(peeked, Ok(Ok(1))) {
            return;
        }
        if first[0] != 0x16 {
            return self.serve_conn(s, peer, false).await;
        }
        match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(s)).await {
            Ok(Ok(t)) => self.serve_conn(t, peer, true).await,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "preview proxy: TLS handshake failed");
            }
            Err(_) => {}
        }
    }

    async fn serve_conn<S>(self: Arc<Self>, s: S, _peer: SocketAddr, secure: bool)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let slot: Slot = Arc::new(tokio::sync::Mutex::new(None));
        let me = self.clone();
        let svc = hyper::service::service_fn(move |req| {
            let me = me.clone();
            let slot = slot.clone();
            async move { Ok::<_, std::convert::Infallible>(me.handle(req, slot, secure).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(30))
            .serve_connection(TokioIo::new(s), svc)
            .with_upgrades()
            .await;
    }

    fn deny(&self, status: StatusCode, title: &str, text: &str) -> Response<Body> {
        self.denied.fetch_add(1, Ordering::Relaxed);
        page(status, title, text)
    }

    async fn handle(
        self: Arc<Self>,
        mut req: Request<Incoming>,
        slot: Slot,
        secure: bool,
    ) -> Response<Body> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let host_hdr = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri().authority().map(|a| a.to_string()));
        let Some((host, port)) = host_hdr.as_deref().and_then(split_host) else {
            return self.deny(
                StatusCode::BAD_REQUEST,
                "Bad request",
                "missing Host header",
            );
        };
        let route = match self.route(&host) {
            Some(r) if port == Some(self.port()) && r.tls == secure => r,
            _ => {
                return self.deny(
                    StatusCode::MISDIRECTED_REQUEST,
                    "Unknown preview host",
                    if secure {
                        "This host is not an https preview origin of this Vibeke proxy."
                    } else {
                        "This host is not a preview origin of this Vibeke proxy (a tls_origin preview is served over https only)."
                    },
                );
            }
        };
        let own_origin = format!(
            "{}://{host}:{}",
            if route.tls { "https" } else { "http" },
            self.port()
        );
        let how_to = format!(
            "Open it with `vibeke preview open {}{} --proxy`.",
            if route.machine.is_empty() || route.machine == "local" {
                String::new()
            } else {
                format!("{}/", route.machine)
            },
            route.handle
        );
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let (token, rest) = take_param(&query, TOKEN_PARAM);
        let target = if rest.is_empty() {
            path.clone()
        } else {
            format!("{path}?{rest}")
        };
        if let Some(t) = token {
            // Exchange: one-time token → session cookie; the token never reaches upstream and
            // leaves the address bar (and Referer) at once.
            if !self.take_token(&host, &t) {
                return self.deny(
                    StatusCode::UNAUTHORIZED,
                    "Preview link expired",
                    &format!("This link was already used or is older than 60 seconds. {how_to}"),
                );
            }
            let Some(sess) = self.new_session(&host, secure) else {
                return self.deny(StatusCode::UNAUTHORIZED, "Preview closed", &how_to);
            };
            let mut r = Response::new(full(Bytes::new()));
            *r.status_mut() = StatusCode::SEE_OTHER;
            let h = r.headers_mut();
            // Absolute, on the authenticated origin: a path like `//elsewhere/` stays a path
            // of this preview instead of becoming a network-path reference.
            if let Ok(v) = HeaderValue::from_str(&format!("{own_origin}{target}")) {
                h.insert(header::LOCATION, v);
            }
            let c = format!(
                "{COOKIE}={sess}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict; Secure",
                SESSION_TTL.as_secs()
            );
            if let Ok(v) = HeaderValue::from_str(&c) {
                h.append(header::SET_COOKIE, v);
            }
            secure_headers(h);
            return r;
        }
        if !self.session_ok(&host, secure, &session_cookies(req.headers())) {
            return self.deny(
                StatusCode::UNAUTHORIZED,
                "Preview login required",
                &format!(
                    "This preview needs a Vibeke credential (or your browser blocked its cookie). {how_to}"
                ),
            );
        }
        let upgrade = is_upgrade(req.headers());
        if let Some(why) = foreign_request(req.method(), req.headers(), &own_origin, upgrade) {
            tracing::warn!(host = %host, why, "preview proxy: refused a cross-origin request");
            return self.deny(StatusCode::FORBIDDEN, "Cross-origin request refused", why);
        }

        // Revocation: the preview must still exist (not forgotten/retired) on the same port.
        match self.upstream.check(&route).await {
            RouteCheck::Live => {}
            RouteCheck::Gone => {
                self.remove_host(&host);
                return self.deny(
                    StatusCode::GONE,
                    "Preview closed",
                    &format!("This preview was closed or moved; its sign-in was revoked. {how_to}"),
                );
            }
            RouteCheck::Unavailable => {
                return self.deny(
                    StatusCode::BAD_GATEWAY,
                    "Preview machine not reachable",
                    &format!("Could not confirm the preview with {}.", route.machine),
                );
            }
        }

        // ---- forward ----
        let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));
        let upgrade_proto = req.headers().get(header::UPGRADE).cloned();
        let (parts, body) = req.into_parts();
        let mut headers = parts.headers;
        strip_hop_by_hop(&mut headers);
        let upstream_origin = format!("{}://localhost:{}", route.scheme, route.port);
        if let Ok(v) = HeaderValue::from_str(&format!("localhost:{}", route.port)) {
            headers.insert(header::HOST, v);
        }
        if let Some(o) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
            && o.eq_ignore_ascii_case(&own_origin)
            && let Ok(v) = HeaderValue::from_str(&upstream_origin)
        {
            headers.insert(header::ORIGIN, v);
        }
        if let Some(r) = headers.get(header::REFERER).and_then(|v| v.to_str().ok())
            && let Some(rest) = r.strip_prefix(&own_origin)
            && (rest.is_empty() || rest.starts_with(['/', '?', '#']))
            && let Ok(v) = HeaderValue::from_str(&format!("{upstream_origin}{rest}"))
        {
            headers.insert(header::REFERER, v);
        }
        let cookies: Vec<String> = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(strip_reserved_cookies)
            .collect();
        headers.remove(header::COOKIE);
        if !cookies.is_empty()
            && let Ok(v) = HeaderValue::from_str(&cookies.join("; "))
        {
            headers.insert(header::COOKIE, v);
        }
        if let Some(p) = upgrade_proto {
            headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
            headers.insert(header::UPGRADE, p);
        }
        let mut up = Request::new(body.boxed());
        *up.method_mut() = parts.method;
        *up.version_mut() = Version::HTTP_11;
        *up.uri_mut() = match target.parse() {
            Ok(u) => u,
            Err(_) => return self.deny(StatusCode::BAD_REQUEST, "Bad request", "invalid path"),
        };
        *up.headers_mut() = headers;

        let resp = if upgrade {
            // An upgraded connection is consumed: never reuse it.
            match self.sender(&route).await {
                Ok(mut s) => s.send_request(up).await,
                Err(e) => return self.unreachable(&route, e),
            }
        } else {
            let mut g = slot.lock().await;
            let mut reuse = None;
            if let Some((h, mut s)) = g.take()
                && h == host
                && s.ready().await.is_ok()
            {
                reuse = Some(s);
            }
            let mut sender = match reuse {
                Some(s) => s,
                None => match self.sender(&route).await {
                    Ok(s) => s,
                    Err(e) => return self.unreachable(&route, e),
                },
            };
            let r = sender.send_request(up).await;
            *g = Some((host.clone(), sender));
            r
        };
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => return self.unreachable(&route, io_err(e)),
        };
        let switching = resp.status() == StatusCode::SWITCHING_PROTOCOLS;
        let server_upgrade = switching.then(|| hyper::upgrade::on(&mut resp));
        let (mut parts, body) = resp.into_parts();
        let upgrade_hdrs = (
            parts.headers.get(header::UPGRADE).cloned(),
            parts.headers.get(header::CONNECTION).cloned(),
        );
        strip_hop_by_hop(&mut parts.headers);
        if switching {
            if let Some(u) = upgrade_hdrs.0 {
                parts.headers.insert(header::UPGRADE, u);
            }
            if let Some(c) = upgrade_hdrs.1 {
                parts.headers.insert(header::CONNECTION, c);
            }
        }
        let set_cookies: Vec<String> = parts
            .headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(rewrite_set_cookie)
            .collect();
        parts.headers.remove(header::SET_COOKIE);
        for c in set_cookies {
            if let Ok(v) = HeaderValue::from_str(&c) {
                parts.headers.append(header::SET_COOKIE, v);
            }
        }
        for name in [header::LOCATION, header::CONTENT_LOCATION] {
            if let Some(m) = parts
                .headers
                .get(&name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| map_back(v, &route, &own_origin))
                && let Ok(v) = HeaderValue::from_str(&m)
            {
                parts.headers.insert(name, v);
            }
        }
        if let Some(a) = parts
            .headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok())
            && upstream_origins(&route)
                .iter()
                .any(|o| o.eq_ignore_ascii_case(a))
            && let Ok(v) = HeaderValue::from_str(&own_origin)
        {
            parts.headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        if let (Some(cu), Some(su)) = (client_upgrade, server_upgrade) {
            self.websockets.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let (a, b) = tokio::join!(cu, su);
                if let (Ok(a), Ok(b)) = (a, b) {
                    let mut a = TokioIo::new(a);
                    let mut b = TokioIo::new(b);
                    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                }
            });
        }
        Response::from_parts(parts, body.boxed())
    }

    fn unreachable(&self, route: &Route, e: std::io::Error) -> Response<Body> {
        tracing::debug!(host = %route.host, machine = %route.machine, port = route.port, error = %e, "preview proxy: upstream failed");
        page(
            StatusCode::BAD_GATEWAY,
            "Preview not reachable",
            &format!(
                "Nothing answered on port {} of {}.",
                route.port,
                if route.machine.is_empty() {
                    "local"
                } else {
                    &route.machine
                }
            ),
        )
    }

    async fn sender(&self, route: &Route) -> std::io::Result<SendRequest<Body>> {
        let s = tokio::time::timeout(CONNECT_TIMEOUT, self.upstream.connect(route))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")
            })??;
        let io: Box<dyn Stream> = if route.scheme == "https" {
            Box::new(crate::tls::connect_dev_server(s, CONNECT_TIMEOUT).await?)
        } else {
            s
        };
        let (sender, conn) = hyper::client::conn::http1::Builder::new()
            .handshake(TokioIo::new(io))
            .await
            .map_err(io_err)?;
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        Ok(sender)
    }
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
