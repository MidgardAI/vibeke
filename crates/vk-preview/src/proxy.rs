//! Authenticated reverse proxy for the user's normal browser (06 B4, 09 §8).
//!
//! One HTTP/1.1 listener on `127.0.0.1:<port>` (and `[::1]` when available). Each preview gets
//! its own origin `http://<handle>-…​.vibeke.localhost:<port>` (`*.localhost` resolves to
//! loopback in browsers and is a secure context), selected by the `Host` header; unknown hosts
//! get `421` (DNS-rebinding defence).
//!
//! **Capability.** Opening a preview mints a one-time `vk_token` (60 s). The first navigation
//! carrying it is answered with a `303` to the same URL without the token and an unguessable
//! per-preview session cookie (`__Host-vk_preview`, HttpOnly, SameSite=Strict, Secure,
//! host-only, `Path=/`; plus a non-`Secure` twin `vk_preview` with the same attributes for
//! browsers that refuse `Secure` cookies over `http://*.localhost`). Every request — including
//! WebSocket upgrades — must carry a session of *that* host. Only SHA-256 digests of tokens and
//! sessions are kept, in memory (a server restart revokes everything).
//!
//! **Not leakable cross-origin.** All previews share the site `vibeke.localhost`, so SameSite
//! does not separate them and is not relied on: requests whose `Sec-Fetch-Site` is
//! `same-site`/`cross-site` are refused unless they are top-level navigations, and requests
//! with a foreign `Origin` are refused for WebSocket upgrades and non-GET/HEAD methods. The
//! proxy strips `vk_token` and every reserved `vk_` cookie before forwarding, drops upstream
//! `Set-Cookie` for reserved names and strips `Domain=` from the rest (host-only cookies).
//!
//! **Rewriting** (only what B4 lists): `Host` → `localhost:<port>`; `Origin`/`Referer` →
//! the upstream origin when they are the preview's own origin; response `Location` and
//! `Access-Control-Allow-Origin` mapped back. Bodies stream unbuffered (SSE), `Upgrade`
//! requests (WebSocket/HMR) are tunnelled after the `101`.

use crate::socks::{BoxFuture, Stream};
use bytes::Bytes;
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

/// The session cookie (spec name). `__Host-` = Secure, host-only, `Path=/`.
pub const COOKIE: &str = "__Host-vk_preview";
/// Same value without `Secure`, for browsers that drop `Secure` cookies on `http://*.localhost`.
pub const COOKIE_COMPAT: &str = "vk_preview";
/// The one-time query parameter.
pub const TOKEN_PARAM: &str = "vk_token";
/// Parent domain of every preview origin.
pub const DOMAIN: &str = "vibeke.localhost";
pub const TOKEN_TTL: Duration = Duration::from_secs(60);
const MAX_TOKENS: usize = 16;
const MAX_SESSIONS: usize = 32;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One preview origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Route {
    /// `v4-web.vibeke.localhost` (no port).
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
}

/// How the proxy reaches a route's upstream (a direct loopback connection, or a bridge `tcp:`
/// channel to the preview's machine).
pub trait Upstream: Send + Sync + 'static {
    fn connect(&self, route: &Route) -> BoxFuture<std::io::Result<Box<dyn Stream>>>;
}

type Digest32 = [u8; 32];

fn digest(s: &str) -> Digest32 {
    Sha256::digest(s.as_bytes()).into()
}

fn random_hex() -> String {
    let b: [u8; 32] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Entry {
    route: Route,
    tokens: Vec<(Digest32, Instant)>,
    sessions: Vec<Digest32>,
}

#[derive(Default)]
struct State {
    by_host: HashMap<String, Entry>,
    /// (machine, preview id) → host: re-opening a preview keeps its origin (and its cookies).
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
    port: AtomicU16,
    upstream: Arc<dyn Upstream>,
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

/// `<handle>[-<machine>][-<slug>].vibeke.localhost` (one DNS label ≤ 63 bytes). Uniqueness is
/// guaranteed by [`Proxy::register`], not by this function.
pub fn hostname_for(handle: &str, machine: Option<&str>, slug: Option<&str>) -> String {
    let mut label = dns_part(handle, 16);
    if label.is_empty() {
        label.push('p');
    }
    for (part, max) in [(machine, 20usize), (slug, 63)] {
        if let Some(p) = part {
            let room = 63usize.saturating_sub(label.len() + 1).min(max);
            let p = dns_part(p, room);
            if !p.is_empty() {
                label.push('-');
                label.push_str(&p);
            }
        }
    }
    format!("{label}.{DOMAIN}")
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
                && (n == COOKIE || n == COOKIE_COMPAT)
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
    if let Some(site) = get("sec-fetch-site")
        && matches!(site, "same-site" | "cross-site")
        && get("sec-fetch-mode") != Some("navigate")
    {
        return Some("cross-origin subresource or fetch");
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
            port: AtomicU16::new(0),
            upstream,
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

    /// `http://<host>:<port>`.
    pub fn origin(&self, host: &str) -> String {
        format!("http://{host}:{}", self.port())
    }

    /// Register (or refresh) a preview's origin. `route.host` is the wanted hostname; the
    /// returned route carries the host actually used — the existing one for a preview opened
    /// before, a `-2`, `-3` … variant when another preview holds the name.
    pub fn register(&self, mut route: Route) -> Route {
        let mut st = self.state.lock().unwrap();
        let key = (route.machine.clone(), route.preview.clone());
        if let Some(h) = st.by_preview.get(&key).cloned()
            && let Some(e) = st.by_host.get_mut(&h)
        {
            if e.route.port != route.port || e.route.scheme != route.scheme {
                // The preview moved: old sessions don't carry over.
                e.sessions.clear();
                e.tokens.clear();
            }
            route.host = h;
            e.route = route.clone();
            return route;
        }
        let base = route.host.to_ascii_lowercase();
        let (stem, suffix) = base
            .split_once('.')
            .map(|(a, b)| (a.to_string(), format!(".{b}")))
            .unwrap_or((base.clone(), String::new()));
        let mut host = base.clone();
        let mut n = 2;
        while st.by_host.contains_key(&host) {
            let tail = format!("-{n}");
            let mut s = stem.clone();
            s.truncate(63 - tail.len());
            host = format!("{s}{tail}{suffix}");
            n += 1;
        }
        route.host = host.clone();
        st.by_preview.insert(key, host.clone());
        st.by_host.insert(
            host,
            Entry {
                route: route.clone(),
                tokens: vec![],
                sessions: vec![],
            },
        );
        route
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

    /// `http://<host>:<port><path>` (+ `?vk_token=…`).
    pub fn url(&self, host: &str, path: &str, token: Option<&str>) -> String {
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let mut u = format!("{}{path}", self.origin(host));
        if let Some(t) = token {
            u.push(if u.contains('?') { '&' } else { '?' });
            u.push_str(&format!("{TOKEN_PARAM}={t}"));
        }
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

    fn new_session(&self, host: &str) -> Option<String> {
        let mut st = self.state.lock().unwrap();
        let e = st.by_host.get_mut(host)?;
        if e.sessions.len() >= MAX_SESSIONS {
            e.sessions.remove(0);
        }
        let s = random_hex();
        e.sessions.push(digest(&s));
        Some(s)
    }

    fn session_ok(&self, host: &str, values: &[String]) -> bool {
        let st = self.state.lock().unwrap();
        let Some(e) = st.by_host.get(host) else {
            return false;
        };
        values.iter().any(|v| e.sessions.contains(&digest(v)))
    }

    fn route(&self, host: &str) -> Option<Route> {
        self.state
            .lock()
            .unwrap()
            .by_host
            .get(host)
            .map(|e| e.route.clone())
    }

    /// Bind `127.0.0.1:<port>` (0 = ephemeral) and, best effort, `[::1]` on the same port.
    /// Never a wildcard address (09 §7).
    pub async fn bind(port: u16) -> std::io::Result<Vec<TcpListener>> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let p = v4.local_addr()?.port();
        let mut v = vec![v4];
        if let Ok(v6) = TcpListener::bind((Ipv6Addr::LOCALHOST, p)).await {
            v.push(v6);
        }
        Ok(v)
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
                    tokio::spawn(me.clone().serve_conn(s, peer));
                }
            });
        }
    }

    async fn serve_conn(self: Arc<Self>, s: tokio::net::TcpStream, _peer: SocketAddr) {
        let slot: Slot = Arc::new(tokio::sync::Mutex::new(None));
        let me = self.clone();
        let svc = hyper::service::service_fn(move |req| {
            let me = me.clone();
            let slot = slot.clone();
            async move { Ok::<_, std::convert::Infallible>(me.handle(req, slot).await) }
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

    async fn handle(self: Arc<Self>, mut req: Request<Incoming>, slot: Slot) -> Response<Body> {
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
            Some(r) if port == Some(self.port()) => r,
            _ => {
                return self.deny(
                    StatusCode::MISDIRECTED_REQUEST,
                    "Unknown preview host",
                    "This host is not a preview origin of this Vibeke proxy.",
                );
            }
        };
        let own_origin = self.origin(&host);
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
            let Some(sess) = self.new_session(&host) else {
                return self.deny(StatusCode::UNAUTHORIZED, "Preview closed", &how_to);
            };
            let mut r = Response::new(full(Bytes::new()));
            *r.status_mut() = StatusCode::SEE_OTHER;
            let h = r.headers_mut();
            if let Ok(v) = HeaderValue::from_str(&target) {
                h.insert(header::LOCATION, v);
            }
            for c in [
                format!("{COOKIE}={sess}; Path=/; HttpOnly; SameSite=Strict; Secure"),
                format!("{COOKIE_COMPAT}={sess}; Path=/; HttpOnly; SameSite=Strict"),
            ] {
                if let Ok(v) = HeaderValue::from_str(&c) {
                    h.append(header::SET_COOKIE, v);
                }
            }
            secure_headers(h);
            return r;
        }
        if !self.session_ok(&host, &session_cookies(req.headers())) {
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
