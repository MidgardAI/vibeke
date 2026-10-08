//! The Vibeke relay (spec 16 §6).
//!
//! Hosts keep an authenticated control socket (`/v1/host`). Devices connect to `/v1/connect`; the
//! relay announces them to the host, which opens a data socket (`/v1/accept`) that the relay splices
//! with the device. The relay forwards WebSocket messages verbatim and never sees plaintext.

pub mod cli;
mod limits;
mod splice;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::future::BoxFuture;
use rand::RngCore;
use serde_json::json;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use vk_e2e::relay::{
    Ctrl, Ticket, accept_message, canonical_origin, close, host_auth_message, parse_ticket,
    valid_ticket_subject, verify_ticket,
};
use vk_e2e::{b64, keys};

pub use limits::Limits;
use limits::RateMap;

/// Outcome of an [`Authorizer`] check (spec 16 §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refuse with a WebSocket close `code` (e.g. [`close::UNAUTHORIZED`]) and a short machine
    /// `reason` (`token_invalid`, `account_required`, …) that the peer sees before the close.
    Deny {
        code: u16,
        reason: &'static str,
    },
}

impl Decision {
    pub fn deny(code: u16, reason: &'static str) -> Decision {
        Decision::Deny { code, reason }
    }
}

/// A host that proved ownership of `public` on `/v1/host` (signature already verified).
#[derive(Debug, Clone, Copy)]
pub struct HostConnect<'a> {
    pub host_id: &'a str,
    pub public: &'a [u8; 32],
    /// `?token=` from the request, if any.
    pub token: Option<&'a str>,
    pub ip: IpAddr,
}

/// A device dialing an online host on `/v1/connect`.
#[derive(Debug, Clone, Copy)]
pub struct ClientConnect<'a> {
    pub host_id: &'a str,
    /// The device's ticket, already verified against the host's key, unexpired and not revoked.
    /// `None` when the device sent no ticket (only possible without `require_tickets`).
    pub ticket: Option<&'a Ticket>,
    pub ip: IpAddr,
}

/// Decides who may use the relay (spec 16 §6.6): open, static host tokens, or accounts (implemented
/// outside this crate). Checks are bounded by `Limits::auth_timeout`; a slow check is denied with
/// `4503 auth_unavailable`.
pub trait Authorizer: Send + Sync + 'static {
    /// Called after the host's ownership signature checks out, before it is registered.
    fn host_connect<'a>(&'a self, c: HostConnect<'a>) -> BoxFuture<'a, Decision>;
    /// Called after ticket checks, before admission to the host's queue.
    fn client_connect<'a>(&'a self, _c: ClientConnect<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async { Decision::Allow })
    }
    /// Bytes forwarded by one finished splice (device → host, host → device).
    fn usage(&self, _host_id: &str, _bytes_in: u64, _bytes_out: u64) {}
    /// The host's registration ended (socket closed, failed, refused for capacity after an
    /// `Allow`, or drained). Not called when a newer registration of the same host replaces it, so
    /// calls do not pair one-to-one with `host_connect`; after this call the host is offline.
    fn host_disconnect(&self, _host_id: &str) {}
}

/// Anyone may register a host; only limits apply.
pub struct Open;
impl Authorizer for Open {
    fn host_connect<'a>(&'a self, _: HostConnect<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async { Decision::Allow })
    }
}

/// Hosts must present one of these tokens (`Authorization: Bearer` on `/v1/host`; the older
/// `?token=` query parameter is still accepted for older gateways).
pub struct StaticTokens(pub Vec<String>);
impl Authorizer for StaticTokens {
    fn host_connect<'a>(&'a self, c: HostConnect<'a>) -> BoxFuture<'a, Decision> {
        let ok = c.token.is_some_and(|t| {
            self.0
                .iter()
                .any(|k| constant_eq(k.as_bytes(), t.as_bytes()))
        });
        Box::pin(async move {
            if ok {
                Decision::Allow
            } else {
                Decision::deny(close::UNAUTHORIZED, "token_invalid")
            }
        })
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub struct Config {
    /// Canonical public origins the relay answers for (hosts sign over one of these).
    pub public_origins: Vec<String>,
    pub app_dir: Option<PathBuf>,
    /// Use `X-Forwarded-For` (rightmost entry) as the client IP.
    pub trust_proxy: bool,
    /// Log raw client IPs instead of daily-keyed hashes.
    pub log_ip_raw: bool,
    pub limits: Limits,
    /// Refuse `/v1/connect` without a host-signed ticket (spec 16 §6.6). Tickets that are present
    /// are always verified.
    pub require_tickets: bool,
    /// Where users create an account for this relay; advertised on `/v1/status`.
    pub account_url: Option<String>,
}

impl Default for Config {
    /// No public origins (set at least one), no app, defaults for everything else.
    fn default() -> Self {
        Config {
            public_origins: Vec::new(),
            app_dir: None,
            trust_proxy: false,
            log_ip_raw: false,
            limits: Limits::default(),
            require_tickets: false,
            account_url: None,
        }
    }
}

/// Most revoked subjects remembered per host; the oldest is forgotten first.
const MAX_REVOKED: usize = 1024;

/// Ticket subjects the host revoked (`Ctrl::Revoke`), bounded FIFO.
#[derive(Default)]
struct Revoked {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl Revoked {
    fn insert(&mut self, sub: String) {
        if !self.set.insert(sub.clone()) {
            return;
        }
        self.order.push_back(sub);
        if self.order.len() > MAX_REVOKED
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
    }
    fn contains(&self, sub: &str) -> bool {
        self.set.contains(sub)
    }
}

struct HostEntry {
    generation: u64,
    public: [u8; 32],
    tx: mpsc::Sender<Outbound>,
    announces: limits::Bucket,
}

/// Most hosts whose revocations are remembered; the least recently revoking host is forgotten
/// first.
const MAX_REVOKING_HOSTS: usize = 4096;

/// Revoked ticket subjects per host id. Kept outside the registration so a host that drops and
/// reconnects (same key, same id) does not lose them; tickets last up to 30 days.
#[derive(Default)]
struct Revocations {
    by_host: HashMap<String, Revoked>,
    order: VecDeque<String>,
}

impl Revocations {
    fn insert(&mut self, host: &str, sub: String) {
        if !self.by_host.contains_key(host) {
            self.order.push_back(host.to_string());
            if self.order.len() > MAX_REVOKING_HOSTS
                && let Some(old) = self.order.pop_front()
            {
                self.by_host.remove(&old);
            }
        }
        self.by_host
            .entry(host.to_string())
            .or_default()
            .insert(sub);
    }
    fn contains(&self, host: &str, sub: &str) -> bool {
        self.by_host.get(host).is_some_and(|r| r.contains(sub))
    }
}

enum Outbound {
    Ctrl(Ctrl),
    Close(u16, &'static str),
}

struct Pending {
    host: String,
    generation: u64,
    public: [u8; 32],
    deliver: oneshot::Sender<(WebSocket, IpGuard, splice::Counted)>,
}

pub struct Relay {
    cfg: Config,
    auth: Box<dyn Authorizer>,
    hosts: Mutex<HashMap<String, HostEntry>>,
    revoked: std::sync::Mutex<Revocations>,
    pending: Mutex<HashMap<String, Pending>>,
    next_gen: AtomicU64,
    ip_rate: RateMap,
    /// `/v1/connect` announces per (client IP, host): one address can't flood one host's
    /// gateway with handshakes it must dial back for.
    announce_rate: RateMap<(IpAddr, String)>,
    ip_conns: std::sync::Mutex<HashMap<IpAddr, usize>>,
    /// Control and accept sockets per IP that have not authenticated yet (spec 16 §6.4: ≤ 8).
    ip_unauth: std::sync::Mutex<HashMap<IpAddr, usize>>,
    pub(crate) per_host: std::sync::Mutex<HashMap<String, usize>>,
    pub(crate) spliced: AtomicUsize,
    /// `(host, sub)` revocations, so live splices for that subject can close (spec 16 §6.6).
    pub(crate) revocations: broadcast::Sender<(String, String)>,
    draining: AtomicBool,
    ip_salt: [u8; 32],
    started: Instant,
}

pub type Shared = Arc<Relay>;

impl Relay {
    pub fn new(cfg: Config, auth: Box<dyn Authorizer>) -> anyhow::Result<Shared> {
        let mut public_origins = Vec::new();
        for o in &cfg.public_origins {
            public_origins.push(canonical_origin(o)?);
        }
        anyhow::ensure!(
            !public_origins.is_empty(),
            "at least one --public-url is required"
        );
        let ip_rate = RateMap::new(cfg.limits.ip_new_per_min);
        let announce_rate = RateMap::new(cfg.limits.ip_host_announces_per_min);
        Ok(Arc::new(Relay {
            cfg: Config {
                public_origins,
                ..cfg
            },
            auth,
            hosts: Mutex::new(HashMap::new()),
            revoked: std::sync::Mutex::new(Revocations::default()),
            pending: Mutex::new(HashMap::new()),
            next_gen: AtomicU64::new(1),
            ip_rate,
            announce_rate,
            ip_conns: std::sync::Mutex::new(HashMap::new()),
            ip_unauth: std::sync::Mutex::new(HashMap::new()),
            per_host: std::sync::Mutex::new(HashMap::new()),
            spliced: AtomicUsize::new(0),
            revocations: broadcast::channel(1024).0,
            draining: AtomicBool::new(false),
            ip_salt: keys::random_bytes(),
            started: Instant::now(),
        }))
    }

    pub fn router(self: &Shared) -> Router {
        let mut r = Router::new()
            .route("/v1/host", get(host_ws))
            .route("/v1/accept", get(accept_ws))
            .route("/v1/connect", get(connect_ws))
            .route("/v1/status", get(status))
            .route("/healthz", get(|| async { "ok" }));
        if let Some(dir) = &self.cfg.app_dir {
            let index = dir.join("index.html");
            r = r.fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .fallback(tower_http::services::ServeFile::new(index)),
            );
        }
        r.with_state(self.clone())
    }

    /// Stop accepting, tell hosts to reconnect elsewhere, wait for splices to finish (≤ `grace`).
    pub async fn drain(&self, grace: Duration) {
        self.draining.store(true, Ordering::SeqCst);
        let drained: Vec<_> = self.hosts.lock().await.drain().collect();
        for (host, h) in drained {
            let _ = h.tx.try_send(Outbound::Close(close::DRAINING, "draining"));
            self.auth.host_disconnect(&host);
        }
        let deadline = Instant::now() + grace;
        while self.spliced.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn ip_label(&self, ip: IpAddr) -> String {
        if self.cfg.log_ip_raw {
            return ip.to_string();
        }
        let day = (self.started.elapsed().as_secs() / 86_400).to_le_bytes();
        let h = blake3::keyed_hash(&self.ip_salt, &[ip.to_string().as_bytes(), &day].concat());
        h.to_hex()[..12].to_string()
    }

    fn client_ip(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        if self.cfg.trust_proxy
            && let Some(ip) = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
        peer.ip()
    }

    /// Per-IP admission: new-socket rate and concurrent cap. Returns a guard that releases the slot.
    fn admit(self: &Shared, ip: IpAddr) -> Result<IpGuard, StatusCode> {
        if self.draining.load(Ordering::SeqCst) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        if !self.ip_rate.allow(ip) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        let mut m = self.ip_conns.lock().unwrap();
        let n = m.entry(ip).or_default();
        if *n >= self.cfg.limits.ip_concurrent {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        *n += 1;
        Ok(IpGuard {
            relay: self.clone(),
            ip,
        })
    }

    /// Reserve one of the IP's unauthenticated control/accept slots; released when the socket
    /// authenticates or closes.
    fn admit_unauth(self: &Shared, ip: IpAddr) -> Result<UnauthGuard, StatusCode> {
        let mut m = self.ip_unauth.lock().unwrap();
        let n = m.entry(ip).or_default();
        if *n >= self.cfg.limits.ip_unauthenticated {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        *n += 1;
        Ok(UnauthGuard {
            relay: self.clone(),
            ip,
        })
    }

    fn host_spliced(&self, host: &str) -> usize {
        self.per_host
            .lock()
            .unwrap()
            .get(host)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn auth_usage(&self, host: &str, up: u64, down: u64) {
        self.auth.usage(host, up, down);
    }

    /// Whether `host` revoked ticket subject `sub`.
    pub(crate) fn is_revoked(&self, host: &str, sub: &str) -> bool {
        self.revoked.lock().unwrap().contains(host, sub)
    }

    /// Bound an authorizer check by `auth_timeout`.
    async fn check(&self, f: BoxFuture<'_, Decision>) -> Decision {
        tokio::time::timeout(self.cfg.limits.auth_timeout, f)
            .await
            .unwrap_or(Decision::deny(close::DRAINING, "auth_unavailable"))
    }

    pub fn origin_ok(&self, origin: &str) -> bool {
        self.cfg.public_origins.iter().any(|o| o == origin)
    }
}

struct IpGuard {
    relay: Shared,
    ip: IpAddr,
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        let mut m = self.relay.ip_conns.lock().unwrap();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

struct UnauthGuard {
    relay: Shared,
    ip: IpAddr,
}

impl Drop for UnauthGuard {
    fn drop(&mut self) {
        let mut m = self.relay.ip_unauth.lock().unwrap();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

fn reject(code: StatusCode) -> Response {
    (code, code.canonical_reason().unwrap_or("")).into_response()
}

fn upgrade(ws: WebSocketUpgrade, limits: &Limits) -> WebSocketUpgrade {
    ws.max_message_size(limits.max_message)
        .max_frame_size(limits.max_message)
}

async fn close_ws(mut ws: WebSocket, code: u16, reason: &'static str) {
    let _ = ws
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// Read the next text message within `wait`.
async fn read_text(ws: &mut WebSocket, wait: Duration) -> Option<String> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match tokio::time::timeout(left, ws.recv()).await {
            Ok(Some(Ok(Message::Text(t)))) => return Some(t.to_string()),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            _ => return None,
        }
    }
}

/// Refuse a device visibly: browsers ignore close codes, so send
/// `{"error":"unauthorized","reason":…}` as plaintext first, then close with `code`.
async fn deny_client(mut ws: WebSocket, code: u16, reason: &'static str) {
    let body = json!({ "error": "unauthorized", "reason": reason }).to_string();
    let _ = ws.send(Message::Text(body.into())).await;
    close_ws(ws, code, reason).await;
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random_id() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b64::encode(b)
}

// ---------------------------------------------------------------------------------------------
// /v1/host

#[derive(serde::Deserialize)]
struct HostQuery {
    /// Deprecated: tokens in URLs end up in proxy and access logs. Gateways after 0.2.0 send
    /// `Authorization: Bearer`; drop this once 0.2.0 and older gateways are gone.
    token: Option<String>,
}

/// The host token: `Authorization: Bearer <token>`, else the deprecated `?token=`.
fn host_token(headers: &HeaderMap, query: Option<String>) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.trim().split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then(|| rest.trim().to_string())
        })
        .filter(|t| !t.is_empty())
        .or(query)
}

async fn host_ws(
    State(relay): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<HostQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let ip = relay.client_ip(peer, &headers);
    let token = host_token(&headers, q.token);
    let guard = match relay.admit(ip) {
        Ok(g) => g,
        Err(c) => return reject(c),
    };
    let unauth = match relay.admit_unauth(ip) {
        Ok(g) => g,
        Err(c) => return reject(c),
    };
    let limits = relay.cfg.limits.clone();
    upgrade(ws, &limits).on_upgrade(move |socket| async move {
        let _guard = guard;
        host_session(relay, socket, token, ip, unauth).await;
    })
}

async fn host_session(
    relay: Shared,
    mut ws: WebSocket,
    token: Option<String>,
    ip: IpAddr,
    unauth: UnauthGuard,
) {
    let nonce = keys::random_bytes::<32>();
    let origin = relay.cfg.public_origins[0].clone();
    let challenge = Ctrl::Challenge {
        nonce: b64::encode(nonce),
        origin: origin.clone(),
    };
    if ws
        .send(Message::Text(challenge.to_text().into()))
        .await
        .is_err()
    {
        return;
    }
    let Some(text) = read_text(&mut ws, relay.cfg.limits.auth_timeout).await else {
        return close_ws(ws, close::UNAUTHORIZED, "auth timeout").await;
    };
    let (host, public) = match verify_host_auth(&relay, &text, &nonce) {
        Some(h) => h,
        None => return close_ws(ws, close::UNAUTHORIZED, "bad auth").await,
    };
    let decision = relay
        .check(relay.auth.host_connect(HostConnect {
            host_id: &host,
            public: &public,
            token: token.as_deref(),
            ip,
        }))
        .await;
    if let Decision::Deny { code, reason } = decision {
        let err = Ctrl::Error {
            code,
            reason: reason.into(),
        };
        let _ = ws.send(Message::Text(err.to_text().into())).await;
        return close_ws(ws, code, reason).await;
    }
    drop(unauth);
    let generation = relay.next_gen.fetch_add(1, Ordering::SeqCst);
    let (tx, mut rx) = mpsc::channel::<Outbound>(64);
    {
        let mut hosts = relay.hosts.lock().await;
        if !hosts.contains_key(&host) && hosts.len() >= relay.cfg.limits.max_hosts {
            drop(hosts);
            relay.auth.host_disconnect(&host);
            return close_ws(ws, close::RATE_LIMITED, "relay full").await;
        }
        let entry = HostEntry {
            generation,
            public,
            tx: tx.clone(),
            announces: limits::Bucket::per_minute(relay.cfg.limits.host_announces_per_min),
        };
        if let Some(old) = hosts.remove(&host) {
            let _ = old
                .tx
                .try_send(Outbound::Close(close::REPLACED, "replaced"));
        }
        hosts.insert(host.clone(), entry);
    }
    // Re-announce connections still waiting for this host (a replaced control may have dropped them).
    {
        let mut pending = relay.pending.lock().await;
        for (conn, p) in pending.iter_mut().filter(|(_, p)| p.host == host) {
            p.generation = generation;
            p.public = public;
            let _ = tx.try_send(Outbound::Ctrl(Ctrl::Incoming {
                conn: conn.clone(),
                generation,
            }));
        }
    }
    tracing::info!(
        host = &host[..8],
        generation,
        ip = relay.ip_label(ip),
        "host registered"
    );
    let ok = Ctrl::Ok {
        host: host.clone(),
        generation,
    };
    if ws.send(Message::Text(ok.to_text().into())).await.is_err() {
        unregister(&relay, &host, generation).await;
        return;
    }
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(Outbound::Ctrl(c)) => {
                    if ws.send(Message::Text(c.to_text().into())).await.is_err() { break; }
                }
                Some(Outbound::Close(code, reason)) => {
                    close_ws(ws, code, reason).await;
                    unregister(&relay, &host, generation).await;
                    return;
                }
                None => break,
            },
            msg = ws.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Text(t))) => {
                    if let Ok(Ctrl::Revoke { sub }) = Ctrl::parse(&t) {
                        revoke(&relay, &host, sub).await;
                    }
                }
                Some(Ok(_)) => {} // pings are answered by axum
            },
        }
    }
    unregister(&relay, &host, generation).await;
}

/// Remove the registration only if it is still ours (generation fence, spec 16 §6.2).
async fn unregister(relay: &Shared, host: &str, generation: u64) {
    let removed = {
        let mut hosts = relay.hosts.lock().await;
        let ours = hosts.get(host).is_some_and(|h| h.generation == generation);
        ours && hosts.remove(host).is_some()
    };
    if removed {
        tracing::info!(host = &host[..8], generation, "host unregistered");
        relay.auth.host_disconnect(host);
    }
}

/// `Ctrl::Revoke`: refuse further tickets for `sub` and close its live splices (spec 16 §6.6).
async fn revoke(relay: &Shared, host: &str, sub: String) {
    if !valid_ticket_subject(&sub) {
        return;
    }
    relay.revoked.lock().unwrap().insert(host, sub.clone());
    tracing::info!(host = &host[..8], "ticket subject revoked");
    let _ = relay.revocations.send((host.to_string(), sub));
}

fn verify_host_auth(relay: &Relay, text: &str, nonce: &[u8; 32]) -> Option<(String, [u8; 32])> {
    let Ctrl::Auth { host, public, sig } = Ctrl::parse(text).ok()? else {
        return None;
    };
    let public: [u8; 32] = b64::decode_array(&public).ok()?;
    let sig: [u8; 64] = b64::decode_array(&sig).ok()?;
    if keys::host_id(&public) != host {
        return None;
    }
    // The host signs over the origin it dialed; accept any of our configured public origins.
    relay
        .cfg
        .public_origins
        .iter()
        .any(|o| keys::verify(&public, &host_auth_message(o, nonce), &sig))
        .then_some((host, public))
}

// ---------------------------------------------------------------------------------------------
// /v1/connect

#[derive(serde::Deserialize)]
struct ConnectQuery {
    host: String,
    ticket: Option<String>,
}

async fn connect_ws(
    State(relay): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<ConnectQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let ip = relay.client_ip(peer, &headers);
    if q.host.len() != 26
        || !q
            .host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    {
        return reject(StatusCode::BAD_REQUEST);
    }
    // Each announce makes the host dial back and wait for a handshake: bound them per address.
    if !relay.announce_rate.allow((ip, q.host.clone())) {
        return reject(StatusCode::TOO_MANY_REQUESTS);
    }
    // Admission first so ticket checks cannot be used to burn CPU without limits.
    let guard = match relay.admit(ip) {
        Ok(g) => g,
        Err(c) => return reject(c),
    };
    let limits = relay.cfg.limits.clone();
    upgrade(ws, &limits).on_upgrade(move |socket| async move {
        let _guard = guard;
        let ConnectQuery { host, ticket } = q;
        let Some((ticket, watch)) = check_client(&relay, &host, ticket.as_deref(), ip).await else {
            return close_ws(socket, close::HOST_OFFLINE, "host offline").await;
        };
        match ticket {
            Ok(_) => client_session(relay, socket, host, watch).await,
            Err((code, reason)) => deny_client(socket, code, reason).await,
        }
    })
}

/// A splice's subject and its revocation feed (subscribed before the revoked check, so no
/// revocation is missed in between).
pub(crate) type Watch = Option<(String, broadcast::Receiver<(String, String)>)>;

type Admitted = Result<(), (u16, &'static str)>;

/// Ticket and authorizer checks for `/v1/connect` (spec 16 §6.6). `None`: host offline.
async fn check_client(
    relay: &Shared,
    host: &str,
    ticket: Option<&str>,
    ip: IpAddr,
) -> Option<(Admitted, Watch)> {
    let claimed = ticket
        .and_then(|t| parse_ticket(t).ok())
        .map(|(t, _)| t.sub);
    let rx = claimed.as_ref().map(|_| relay.revocations.subscribe());
    let public = relay.hosts.lock().await.get(host)?.public;
    let revoked = claimed
        .as_deref()
        .is_some_and(|s| relay.is_revoked(host, s));
    let deny = |reason| Some((Err((close::UNAUTHORIZED, reason)), None));
    let verified = match ticket.map(|t| verify_ticket(&public, host, t, unix_now())) {
        Some(Err(e)) => return deny(e.reason()),
        Some(Ok(_)) if revoked => return deny("ticket_revoked"),
        Some(Ok(t)) => Some(t),
        None if relay.cfg.require_tickets => return deny("ticket_missing"),
        None => None,
    };
    let decision = relay
        .check(relay.auth.client_connect(ClientConnect {
            host_id: host,
            ticket: verified.as_ref(),
            ip,
        }))
        .await;
    if let Decision::Deny { code, reason } = decision {
        return Some((Err((code, reason)), None));
    }
    let watch = verified.zip(rx).map(|(t, rx)| (t.sub, rx));
    Some((Ok(()), watch))
}

async fn client_session(relay: Shared, ws: WebSocket, host: String, watch: Watch) {
    let conn = random_id();
    let (deliver, delivered) = oneshot::channel();
    // Register pending and announce under the hosts lock so a concurrent replacement sees it.
    let announced = {
        let mut hosts = relay.hosts.lock().await;
        let Some(h) = hosts.get_mut(&host) else {
            drop(hosts);
            return close_ws(ws, close::HOST_OFFLINE, "host offline").await;
        };
        let mut pending = relay.pending.lock().await;
        let waiting = pending.values().filter(|p| p.host == host).count();
        // Pending connections are reservations: count them against the global and per-host caps.
        let busy = waiting >= relay.cfg.limits.host_pending
            || relay.spliced.load(Ordering::SeqCst) + pending.len() >= relay.cfg.limits.max_conns
            || relay.host_spliced(&host) + waiting >= relay.cfg.limits.host_spliced;
        if busy || !h.announces.take(1.0) {
            None
        } else {
            pending.insert(
                conn.clone(),
                Pending {
                    host: host.clone(),
                    generation: h.generation,
                    public: h.public,
                    deliver,
                },
            );
            Some(
                h.tx.try_send(Outbound::Ctrl(Ctrl::Incoming {
                    conn: conn.clone(),
                    generation: h.generation,
                }))
                .is_ok(),
            )
        }
    };
    match announced {
        None => return close_ws(ws, close::RATE_LIMITED, "busy").await,
        Some(false) => {
            relay.pending.lock().await.remove(&conn);
            return close_ws(ws, close::HOST_OFFLINE, "host offline").await;
        }
        Some(true) => {}
    }
    match tokio::time::timeout(relay.cfg.limits.accept_timeout, delivered).await {
        Ok(Ok((host_ws, host_guard, slot))) => {
            // The host's accept socket keeps its admission slot for the life of the splice.
            let _host_guard = host_guard;
            splice::run(relay.clone(), ws, host_ws, host, slot, watch).await;
        }
        _ => {
            // Expire atomically: a racing accept either took the entry (and owns the splice) or finds nothing.
            relay.pending.lock().await.remove(&conn);
            close_ws(ws, close::ACCEPT_TIMEOUT, "accept timeout").await;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// /v1/accept

async fn accept_ws(
    State(relay): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let ip = relay.client_ip(peer, &headers);
    let guard = match relay.admit(ip) {
        Ok(g) => g,
        Err(c) => return reject(c),
    };
    let unauth = match relay.admit_unauth(ip) {
        Ok(g) => g,
        Err(c) => return reject(c),
    };
    let limits = relay.cfg.limits.clone();
    upgrade(ws, &limits).on_upgrade(move |socket| async move {
        accept_session(relay, socket, guard, unauth).await;
    })
}

async fn accept_session(relay: Shared, mut ws: WebSocket, guard: IpGuard, unauth: UnauthGuard) {
    let Some(text) = read_text(&mut ws, relay.cfg.limits.auth_timeout).await else {
        return close_ws(ws, close::UNAUTHORIZED, "accept timeout").await;
    };
    let Ok(Ctrl::Accept {
        host,
        conn,
        generation,
        sig,
    }) = Ctrl::parse(&text)
    else {
        return close_ws(ws, close::BAD_REQUEST, "expected accept").await;
    };
    let Ok(sig) = b64::decode_array::<64>(&sig) else {
        return close_ws(ws, close::BAD_REQUEST, "bad sig").await;
    };
    let taken = {
        let mut pending = relay.pending.lock().await;
        match pending.get(&conn) {
            Some(p)
                if p.host == host
                    && p.generation == generation
                    && relay.cfg.public_origins.iter().any(|o| {
                        keys::verify(
                            &p.public,
                            &accept_message(o, &host, generation, &conn),
                            &sig,
                        )
                    }) =>
            {
                // Reservation → active slot in one step, under the pending lock.
                pending.remove(&conn).map(|p| {
                    let slot = splice::Counted::take(&relay, &p.host);
                    (p, slot)
                })
            }
            _ => None,
        }
    };
    let Some(p) = taken else {
        return close_ws(ws, close::UNAUTHORIZED, "unknown or expired conn").await;
    };
    drop(unauth);
    let (p, slot) = p;
    if p.deliver.send((ws, guard, slot)).is_err() {
        tracing::debug!("client left before accept");
    }
}

// ---------------------------------------------------------------------------------------------
// /v1/status

#[derive(serde::Deserialize)]
struct StatusQuery {
    host: String,
}

async fn status(
    State(relay): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<StatusQuery>,
) -> Response {
    let ip = relay.client_ip(peer, &headers);
    if !relay.ip_rate.allow(ip) {
        return reject(StatusCode::TOO_MANY_REQUESTS);
    }
    let online = relay.hosts.lock().await.contains_key(&q.host);
    (
        [
            ("access-control-allow-origin", "*"),
            ("cache-control", "no-store"),
        ],
        axum::Json(json!({
            "online": online,
            "auth": if relay.cfg.require_tickets { "tickets" } else { "open" },
            "account_url": relay.cfg.account_url,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_token_prefers_the_header() {
        let mut h = HeaderMap::new();
        assert_eq!(host_token(&h, None), None);
        assert_eq!(host_token(&h, Some("q".into())).as_deref(), Some("q"));
        h.insert("authorization", "Bearer secret".parse().unwrap());
        assert_eq!(host_token(&h, Some("q".into())).as_deref(), Some("secret"));
        h.insert("authorization", "bearer  other ".parse().unwrap());
        assert_eq!(host_token(&h, None).as_deref(), Some("other"));
        h.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(host_token(&h, Some("q".into())).as_deref(), Some("q"));
    }
}
