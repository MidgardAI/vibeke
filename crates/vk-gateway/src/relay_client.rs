//! The gateway's side of the relay protocol (spec 16 §6.2–§6.3): an authenticated control socket
//! and one data socket per announced device connection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use vk_e2e::b64;
use vk_e2e::relay::{Ctrl, accept_message, canonical_origin, close, host_auth_message};

use crate::Gateway;
use crate::state::now_s;

/// How long a relay's `/v1/status` answer about tickets is trusted.
const STATUS_TTL: Duration = Duration::from_secs(300);
/// While waiting for `vibeke login`: how often to look for a credential, and the longest wait.
const LOGIN_POLL: Duration = Duration::from_secs(5);
const LOGIN_RETRY: Duration = Duration::from_secs(30);

/// The relay refused this host on `/v1/host` (`{"t":"error"}` and/or close 4401, spec 16 §6.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub code: u16,
    pub reason: String,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "relay refused this host ({}: {})",
            self.code, self.reason
        )
    }
}

impl std::error::Error for Denied {}

#[derive(Debug, Clone)]
struct Cached {
    token: String,
    issued: u64,
    exp: u64,
}

/// Where the `?token=` on `/v1/host` comes from (spec 16 §6.6): the static `relay_token` of a
/// private relay, else an account host token when the relay requires tickets, else nothing.
pub struct TokenSource {
    static_token: Option<String>,
    account: Option<vk_account::Account>,
    cached: Option<Cached>,
    /// The relay's last `/v1/status` answer: needs an account token?
    tickets: Option<(Instant, bool)>,
    /// The relay asked for an account token even though its status did not say so.
    force_account: bool,
    /// The last token handed out was an account host token.
    last_account: bool,
}

impl TokenSource {
    pub fn new(static_token: Option<String>, account: Option<vk_account::Account>) -> Self {
        TokenSource {
            static_token,
            account,
            cached: None,
            tickets: None,
            force_account: false,
            last_account: false,
        }
    }

    /// A host token issued at `issued`, expiring at `exp`, is replaced once less than a quarter
    /// of its lifetime remains at `now`.
    pub fn stale(issued: u64, exp: u64, now: u64) -> bool {
        let life = exp.saturating_sub(issued).max(1);
        exp.saturating_sub(now) * 4 < life
    }

    /// The last dial used an account token (a refused one is worth one immediate retry).
    pub fn uses_account(&self) -> bool {
        self.last_account
    }

    /// The token to dial with, given whether the relay requires accounts (`tickets`).
    pub async fn token(
        &mut self,
        tickets: bool,
        keys: &vk_e2e::HostKeys,
        name: &str,
    ) -> vk_account::Result<Option<String>> {
        self.last_account = false;
        if let Some(t) = &self.static_token {
            return Ok(Some(t.clone()));
        }
        if !(tickets || self.force_account) {
            return Ok(None);
        }
        let Some(acct) = &self.account else {
            return Err(vk_account::Error::LoginRequired);
        };
        self.last_account = true;
        let now = now_s();
        if let Some(c) = &self.cached
            && !Self::stale(c.issued, c.exp, now)
        {
            return Ok(Some(c.token.clone()));
        }
        let t = acct.host_token(keys, name).await?;
        self.cached = Some(Cached {
            token: t.token.clone(),
            issued: now,
            exp: t.expires_at,
        });
        Ok(Some(t.token))
    }

    /// Whether `relay` requires tickets (and with them account tokens), cached for a few minutes.
    /// Unreachable or older relays count as open.
    async fn relay_tickets(&mut self, relay: &str, host_id: &str) -> bool {
        if let Some((at, t)) = self.tickets
            && at.elapsed() < STATUS_TTL
        {
            return t;
        }
        match crate::account::relay_auth(relay, host_id).await {
            Ok(a) => {
                let needs = a.needs_account();
                self.tickets = Some((Instant::now(), needs));
                needs
            }
            Err(e) => {
                tracing::debug!("relay status: {e:#}");
                false
            }
        }
    }

    /// The token for the next dial of `relay` by `gw`.
    pub async fn for_relay(
        &mut self,
        gw: &Gateway,
        relay: &str,
    ) -> vk_account::Result<Option<String>> {
        let host = gw.keys.host_id();
        let tickets = self.static_token.is_none()
            && (self.force_account || self.relay_tickets(relay, &host).await);
        self.token(tickets, &gw.keys, &gw.host_name).await
    }

    /// The relay refused us with `reason`: drop the cached token and look at its status again.
    pub fn denied(&mut self, reason: &str) {
        self.cached = None;
        self.tickets = None;
        if self.static_token.is_none()
            && matches!(
                reason,
                "account_required" | "token_expired" | "token_invalid"
            )
        {
            self.force_account = true;
        }
    }

    /// Wait up to `max` for a new credential (`vibeke login` in another process), or until
    /// `done` says a login from the TUI finished.
    async fn wait_for_login(&self, max: Duration, done: &tokio::sync::Notify) {
        let Some(acct) = self.account.clone() else {
            let _ = tokio::time::timeout(max, done.notified()).await;
            return;
        };
        let load = |a: vk_account::Account| async move {
            tokio::task::spawn_blocking(move || a.credential().ok().flatten())
                .await
                .ok()
                .flatten()
                .map(|c| c.refresh_token)
        };
        let before = load(acct.clone()).await;
        let until = Instant::now() + max;
        while Instant::now() < until {
            let poll = LOGIN_POLL.min(until - Instant::now());
            if tokio::time::timeout(poll, done.notified()).await.is_ok() {
                tracing::info!("account login finished; reconnecting");
                return;
            }
            let now = load(acct.clone()).await;
            if now.is_some() && now != before {
                tracing::info!("found a new account login; reconnecting");
                return;
            }
        }
    }
}

/// Percent-encode a query value.
fn query_escape(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Clears the gateway's relay command channel when the control socket ends.
struct CtlGuard<'a>(&'a Gateway);

impl Drop for CtlGuard<'_> {
    fn drop(&mut self) {
        self.0.set_relay_ctl(None);
    }
}

/// `https://x` / `wss://x` / `x` → `wss://x` (http → ws for local testing).
pub fn ws_base(relay: &str) -> String {
    let r = relay.trim_end_matches('/');
    if let Some(rest) = r.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = r.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if r.starts_with("wss://") || r.starts_with("ws://") {
        r.to_string()
    } else {
        format!("wss://{r}")
    }
}

/// Where the control socket carries a private relay's token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenIn {
    /// `Authorization: Bearer` (current relays).
    Header,
    /// `?token=` (relays from before the header was read).
    Query,
}

/// Relay URLs (ws base) that accepted the token only in the query string: this process keeps
/// using the query for them, so the fallback and its warning happen once per relay.
fn query_relays() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn token_in(base: &str) -> TokenIn {
    if query_relays().lock().unwrap().contains(base) {
        TokenIn::Query
    } else {
        TokenIn::Header
    }
}

/// The relay authenticated us with the token in the query string: remember it, and warn once.
fn remember_query_relay(base: &str) {
    if query_relays().lock().unwrap().insert(base.to_string()) {
        tracing::warn!(
            "relay {base} accepts the host token only in the URL (?token=), where proxies and access logs can keep it; update the relay"
        );
    }
}

/// The relay closed the control socket with this code and reason.
#[derive(Debug)]
struct RelayClosed {
    code: u16,
    reason: String,
}

impl std::fmt::Display for RelayClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "relay closed: {} {}", self.code, self.reason)
    }
}

impl std::error::Error for RelayClosed {}

/// Whether a refusal means the relay did not accept the token: HTTP 401/403 on the WebSocket
/// upgrade (a proxy or relay checking it up front), or the relay's `4401 "not allowed"` close
/// after the host signature verified (the token check; an older relay reads only `?token=`).
fn token_refused(http_status: Option<u16>, close: Option<(u16, &str)>) -> bool {
    matches!(http_status, Some(401 | 403))
        || close.is_some_and(|(code, reason)| {
            // Only the pre-header relay's reason. A current relay says `token_invalid`
            // after reading the header, so resending that token in the URL would only leak it.
            code == vk_e2e::relay::close::UNAUTHORIZED && reason == "not allowed"
        })
}

/// Whether to retry a failed control connection once with the token in the query string.
fn retry_with_query(has_token: bool, used: TokenIn, e: &anyhow::Error) -> bool {
    if !has_token || used != TokenIn::Header {
        return false;
    }
    let http = match e.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
        Some(tokio_tungstenite::tungstenite::Error::Http(r)) => Some(r.status().as_u16()),
        _ => None,
    };
    let close = e
        .downcast_ref::<RelayClosed>()
        .map(|c| (c.code, c.reason.as_str()))
        .or_else(|| {
            e.downcast_ref::<Denied>()
                .map(|d| (d.code, d.reason.as_str()))
        });
    token_refused(http, close)
}

pub async fn run(gw: Arc<Gateway>, relay: &str) -> Result<()> {
    let base = ws_base(relay);
    // A private relay's static token wins; otherwise an account may be needed (spec 16 §6.6).
    let account = if gw.cfg.relay_token.is_none() {
        let server = crate::account::account_server(&gw.cfg);
        crate::account::account(&server, &gw.state.dir)
            .inspect_err(|e| tracing::warn!("account server {server}: {e:#}"))
            .ok()
    } else {
        None
    };
    let mut tokens = TokenSource::new(gw.cfg.relay_token.clone(), account);
    let mut backoff = Duration::from_secs(1);
    // One immediate redial after a refused account token (expired between fetch and use).
    let mut fast_retry = true;
    loop {
        let started = Instant::now();
        gw.status.set_state("connecting", None);
        let token = match tokens.for_relay(&gw, relay).await {
            Ok(t) => Some(t),
            Err(vk_account::Error::LoginRequired) => {
                tracing::warn!("the relay requires an account for this host: run `vibeke login`");
                gw.status
                    .set_state("login_required", Some("run: vibeke login".into()));
                tokens.wait_for_login(LOGIN_RETRY, &gw.logins.done).await;
                continue;
            }
            Err(e) => {
                tracing::warn!("relay token: {e}");
                gw.status.set_state("offline", Some(e.to_string()));
                None
            }
        };
        if let Some(token) = token {
            let mode = token_in(&base);
            let mut r = control(&gw, &base, token.as_deref(), mode).await;
            // Only a static `relay_token` may fall back to the URL (old self-hosted relays).
            // Account host tokens never travel in a query string: the account relay reads the
            // header, and a URL would leave the token in proxy and access logs.
            if let Err(e) = &r
                && retry_with_query(token.is_some() && !tokens.uses_account(), mode, e)
            {
                tracing::info!(
                    "relay refused the token in the Authorization header; retrying with ?token="
                );
                r = control(&gw, &base, token.as_deref(), TokenIn::Query).await;
            }
            match r {
                Ok(()) => {
                    tracing::info!("relay control closed");
                    gw.status
                        .set_state("offline", Some("relay closed the connection".into()));
                }
                Err(e) => match e.downcast_ref::<Denied>() {
                    Some(d) => {
                        tracing::warn!("{d}");
                        gw.status
                            .set_state("offline", Some(format!("relay refused: {}", d.reason)));
                        if d.code == close::UNAUTHORIZED {
                            tokens.denied(&d.reason);
                            if fast_retry && tokens.uses_account() {
                                fast_retry = false;
                                continue;
                            }
                        }
                    }
                    None => {
                        tracing::warn!("relay: {e:#}");
                        gw.status.set_state("offline", Some(format!("{e:#}")));
                    }
                },
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
            fast_retry = true;
        }
        // Full jitter.
        let jitter = rand::random::<f64>();
        tokio::time::sleep(backoff.mul_f64(jitter.max(0.1))).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn next_text<S>(ws: &mut S) -> Result<String>
where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .context("relay timeout")?
        {
            Some(Ok(Message::Text(t))) => return Ok(t.to_string()),
            Some(Ok(Message::Close(Some(f)))) if u16::from(f.code) == close::UNAUTHORIZED => {
                return Err(Denied {
                    code: close::UNAUTHORIZED,
                    reason: f.reason.to_string(),
                }
                .into());
            }
            Some(Ok(Message::Close(f))) => {
                return Err(match f {
                    Some(f) => RelayClosed {
                        code: u16::from(f.code),
                        reason: f.reason.to_string(),
                    }
                    .into(),
                    None => anyhow::anyhow!("relay closed"),
                });
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => bail!("relay closed"),
        }
    }
}

async fn control(gw: &Arc<Gateway>, base: &str, token: Option<&str>, mode: TokenIn) -> Result<()> {
    let req = host_request(base, token, mode)?;
    let (mut ws, _) = connect_async(req)
        .await
        .with_context(|| format!("connect {base}"))?;
    let dialed = canonical_origin(base)?;
    let Ctrl::Challenge { nonce, origin } = Ctrl::parse(&next_text(&mut ws).await?)? else {
        bail!("expected challenge")
    };
    if origin != dialed {
        bail!("relay announced origin {origin}, but we dialed {dialed}; refusing to sign");
    }
    let sig = gw
        .keys
        .sign(&host_auth_message(&origin, &b64::decode(&nonce)?));
    let auth = Ctrl::Auth {
        host: gw.keys.host_id(),
        public: b64::encode(gw.keys.relay_public()),
        sig: b64::encode(sig),
    };
    ws.send(Message::Text(auth.to_text().into())).await?;
    let generation = match Ctrl::parse(&next_text(&mut ws).await?)? {
        Ctrl::Ok { generation, .. } => generation,
        Ctrl::Error { code, reason } => return Err(Denied { code, reason }.into()),
        _ => bail!("relay refused registration"),
    };
    if mode == TokenIn::Query && token.is_some() {
        remember_query_relay(base);
    }
    if mode == TokenIn::Query && gw.cfg.relay_token.is_some() {
        remember_query_relay(base);
    }
    tracing::info!(host = %gw.keys.host_id(), generation, "online at {base}");
    gw.status.set_state("online", None);
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<Ctrl>();
    gw.set_relay_ctl(Some(ctl_tx));
    let _ctl = CtlGuard(gw);
    let mut announces = Bucket::new(60);
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(t))) => match Ctrl::parse(&t) {
                    Ok(Ctrl::Incoming { conn, generation: g }) if g == generation => {
                        if !announces.take() {
                            tracing::warn!("dropping incoming connection: announce rate exceeded");
                            continue;
                        }
                        // Reserve capacity before dialing, not after the handshake.
                        if gw.live_connections() >= gw.limits.max_connections {
                            tracing::warn!("dropping incoming connection: connection limit reached");
                            continue;
                        }
                        let Some(slot) = crate::session::Dialing::try_reserve(gw) else {
                            tracing::warn!("dropping incoming connection: too many handshakes in progress");
                            continue;
                        };
                        let (gw, base, origin) = (gw.clone(), base.to_string(), origin.clone());
                        tokio::spawn(async move {
                            if let Err(e) = accept(gw, &base, &origin, &conn, generation, slot).await {
                                tracing::debug!("accept: {e:#}");
                            }
                        });
                    }
                    Ok(Ctrl::Error { code, reason }) => tracing::warn!(code, "relay error: {reason}"),
                    Ok(other) => tracing::debug!("relay: {other:?}"),
                    Err(e) => tracing::debug!("relay: {e}"),
                },
                Some(Ok(Message::Close(Some(f)))) if u16::from(f.code) == close::UNAUTHORIZED => {
                    return Err(Denied { code: close::UNAUTHORIZED, reason: f.reason.to_string() }.into());
                }
                Some(Ok(Message::Close(f))) => { tracing::info!("relay closed control: {f:?}"); return Ok(()); }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            _ = ping.tick() => { ws.send(Message::Ping(Vec::new().into())).await?; }
            Some(c) = ctl_rx.recv() => { ws.send(Message::Text(c.to_text().into())).await?; }
        }
    }
}

/// The control-socket request. A private relay's token travels in `Authorization: Bearer`, not
/// in the URL, where proxies and access logs would keep it; only a relay that refused the header
/// gets it as `?token=` ([`TokenIn::Query`]).
fn host_request(
    base: &str,
    token: Option<&str>,
    mode: TokenIn,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
    let mut url = format!("{base}/v1/host");
    if let (Some(t), TokenIn::Query) = (token, mode) {
        url.push_str(&format!("?token={}", query_escape(t)));
    }
    let mut req = url.into_client_request()?;
    if let (Some(t), TokenIn::Header) = (token, mode) {
        let v = HeaderValue::from_str(&format!("Bearer {t}"))
            .context("relay token is not a valid header value")?;
        req.headers_mut().insert(header::AUTHORIZATION, v);
    }
    Ok(req)
}

async fn accept(
    gw: Arc<Gateway>,
    base: &str,
    origin: &str,
    conn: &str,
    generation: u64,
    slot: crate::session::Dialing,
) -> Result<()> {
    // Bounded: a Noise frame is ≤ 64 KiB, so nothing legitimate is larger than this.
    let cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(128 * 1024))
        .max_frame_size(Some(128 * 1024));
    let (mut ws, _) = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async_with_config(format!("{base}/v1/accept"), Some(cfg), false),
    )
    .await
    .context("accept connect timed out")??;
    let host = gw.keys.host_id();
    let sig = gw
        .keys
        .sign(&accept_message(origin, &host, generation, conn));
    let a = Ctrl::Accept {
        host,
        conn: conn.into(),
        generation,
        sig: b64::encode(sig),
    };
    ws.send(Message::Text(a.to_text().into())).await?;
    crate::session::serve(gw, ws, slot).await;
    Ok(())
}

/// Per-minute counter for announcements (gateway-side limit, spec 16 §6.4).
struct Bucket {
    per_min: u32,
    window: Instant,
    used: u32,
}

impl Bucket {
    fn new(per_min: u32) -> Self {
        Bucket {
            per_min,
            window: Instant::now(),
            used: 0,
        }
    }
    fn take(&mut self) -> bool {
        if self.window.elapsed() > Duration::from_secs(60) {
            self.window = Instant::now();
            self.used = 0;
        }
        self.used += 1;
        self.used <= self.per_min
    }
}

#[cfg(test)]
mod tests {
    use super::TokenIn;

    #[test]
    fn relay_token_goes_in_a_header() {
        let r = super::host_request("wss://r.example", Some("s3cret"), TokenIn::Header).unwrap();
        assert_eq!(r.uri().to_string(), "wss://r.example/v1/host");
        assert!(!r.uri().to_string().contains("s3cret"));
        assert_eq!(r.headers()["authorization"], "Bearer s3cret");
        let r = super::host_request("wss://r.example", None, TokenIn::Header).unwrap();
        assert!(r.headers().get("authorization").is_none());
        // The fallback for older relays: the query string only.
        let r = super::host_request("wss://r.example", Some("s3cret"), TokenIn::Query).unwrap();
        assert_eq!(r.uri().to_string(), "wss://r.example/v1/host?token=s3cret");
        assert!(r.headers().get("authorization").is_none());
        let r = super::host_request("wss://r.example", None, TokenIn::Query).unwrap();
        assert_eq!(r.uri().to_string(), "wss://r.example/v1/host");
    }

    #[test]
    fn query_fallback_only_on_token_refusal() {
        use super::{RelayClosed, retry_with_query, token_refused};
        let unauthorized = vk_e2e::relay::close::UNAUTHORIZED;
        assert!(token_refused(Some(401), None));
        assert!(token_refused(Some(403), None));
        assert!(!token_refused(Some(404), None));
        assert!(token_refused(None, Some((unauthorized, "not allowed"))));
        // A bad host signature or a timeout is not about the token.
        assert!(!token_refused(None, Some((unauthorized, "bad auth"))));
        assert!(!token_refused(None, Some((unauthorized, "auth timeout"))));
        assert!(!token_refused(None, None));

        let refused = || -> anyhow::Error {
            anyhow::Error::new(RelayClosed {
                code: unauthorized,
                reason: "not allowed".into(),
            })
        };
        assert!(retry_with_query(true, TokenIn::Header, &refused()));
        assert!(retry_with_query(
            true,
            TokenIn::Header,
            &refused().context("handshake")
        ));
        // Once only, and only with a token.
        assert!(!retry_with_query(true, TokenIn::Query, &refused()));
        assert!(!retry_with_query(false, TokenIn::Header, &refused()));
        assert!(!retry_with_query(
            true,
            TokenIn::Header,
            &anyhow::anyhow!("relay timeout")
        ));
    }

    #[test]
    fn query_fallback_is_remembered_per_relay() {
        let base = "wss://old-relay.test";
        assert_eq!(super::token_in(base), TokenIn::Header);
        super::remember_query_relay(base);
        assert_eq!(super::token_in(base), TokenIn::Query);
        assert_eq!(super::token_in("wss://other-relay.test"), TokenIn::Header);
    }

    use super::TokenSource;
    use crate::state::now_s;
    use std::sync::Arc;
    use vk_account::fake::FakeServer;
    use vk_account::{Account, Client, CredentialStore, MemoryStore};

    #[test]
    fn host_tokens_go_stale_at_a_quarter() {
        assert!(!TokenSource::stale(1000, 2000, 1000));
        assert!(!TokenSource::stale(1000, 2000, 1750));
        assert!(TokenSource::stale(1000, 2000, 1751));
        assert!(TokenSource::stale(1000, 2000, 3000));
        assert!(TokenSource::stale(5, 5, 5));
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(super::query_escape("a.b-c_d~"), "a.b-c_d~");
        assert_eq!(super::query_escape("a b&c=/"), "a%20b%26c%3D%2F");
    }

    #[tokio::test]
    async fn token_source_fetches_caches_and_refreshes() {
        let f = FakeServer::start().await;
        let client = Client::new(&f.url).unwrap();
        // Log in against the fake to get real tokens.
        let cred = client.login(|_| {}).await.unwrap();
        let store = Arc::new(MemoryStore::default());
        store.save(&cred).unwrap();
        let acct = Account::new(client, store.clone());
        let mut src = TokenSource::new(None, Some(acct));
        let keys = vk_e2e::HostKeys::generate();

        // Open relay: no token at all.
        assert_eq!(src.token(false, &keys, "box").await.unwrap(), None);
        assert!(!src.uses_account());
        // Tickets: a host token, then the cached one.
        assert_eq!(
            src.token(true, &keys, "box").await.unwrap().as_deref(),
            Some("h1")
        );
        assert_eq!(
            src.token(true, &keys, "box").await.unwrap().as_deref(),
            Some("h1")
        );
        assert!(src.uses_account());
        assert_eq!(f.with(|s| s.host_token_calls), 1);
        // Less than a quarter of its life left: a new one.
        let now = now_s();
        let c = src.cached.as_mut().unwrap();
        (c.issued, c.exp) = (now - 3500, now + 100);
        assert_eq!(
            src.token(true, &keys, "box").await.unwrap().as_deref(),
            Some("h2")
        );
        // Refused by the relay (4401): dropped and fetched again; the account token is refreshed
        // once when the control plane refuses the access token.
        src.denied("token_expired");
        f.with(|s| s.access = Some("rotated-elsewhere".into()));
        f.with(|s| s.refresh = Some("r1".into()));
        assert_eq!(
            src.token(false, &keys, "box").await.unwrap().as_deref(),
            Some("h3")
        );
        assert_eq!(f.with(|s| s.refresh_calls), 1);
        assert_eq!(store.load(&f.url).unwrap().unwrap().refresh_token, "r2");
        // Credential revoked server-side: login required, credential forgotten.
        src.cached = None;
        f.with(|s| {
            s.access = None;
            s.refresh = None;
        });
        assert_eq!(
            src.token(true, &keys, "box").await.unwrap_err(),
            vk_account::Error::LoginRequired
        );
        assert!(store.load(&f.url).unwrap().is_none());
        // A static relay token always wins.
        let mut st = TokenSource::new(Some("t1".into()), None);
        assert_eq!(
            st.token(true, &keys, "box").await.unwrap().as_deref(),
            Some("t1")
        );
        assert!(!st.uses_account());
    }

    #[test]
    fn ws_base_forms() {
        assert_eq!(super::ws_base("https://r.example/"), "wss://r.example");
        assert_eq!(super::ws_base("r.example"), "wss://r.example");
        assert_eq!(
            super::ws_base("http://127.0.0.1:8787"),
            "ws://127.0.0.1:8787"
        );
    }
}
