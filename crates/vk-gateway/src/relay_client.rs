//! The gateway's side of the relay protocol (spec 16 §6.2–§6.3): an authenticated control socket
//! and one data socket per announced device connection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use vk_e2e::b64;
use vk_e2e::relay::{Ctrl, accept_message, canonical_origin, host_auth_message};

use crate::Gateway;

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
        .map(|c| (c.code, c.reason.as_str()));
    token_refused(http, close)
}

pub async fn run(gw: Arc<Gateway>, relay: &str) -> Result<()> {
    let base = ws_base(relay);
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        gw.status.set_state("connecting", None);
        let mode = token_in(&base);
        let mut r = control(&gw, &base, mode).await;
        if let Err(e) = &r
            && retry_with_query(gw.cfg.relay_token.is_some(), mode, e)
        {
            tracing::info!(
                "relay refused the token in the Authorization header; retrying with ?token="
            );
            r = control(&gw, &base, TokenIn::Query).await;
        }
        match r {
            Ok(()) => {
                tracing::info!("relay control closed");
                gw.status
                    .set_state("offline", Some("relay closed the connection".into()));
            }
            Err(e) => {
                tracing::warn!("relay: {e:#}");
                gw.status.set_state("offline", Some(format!("{e:#}")));
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
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

async fn control(gw: &Arc<Gateway>, base: &str, mode: TokenIn) -> Result<()> {
    let req = host_request(base, gw.cfg.relay_token.as_deref(), mode)?;
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
    let Ctrl::Ok { generation, .. } = Ctrl::parse(&next_text(&mut ws).await?)? else {
        bail!("relay refused registration")
    };
    if mode == TokenIn::Query && gw.cfg.relay_token.is_some() {
        remember_query_relay(base);
    }
    tracing::info!(host = %gw.keys.host_id(), generation, "online at {base}");
    gw.status.set_state("online", None);
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
                    Ok(other) => tracing::debug!("relay: {other:?}"),
                    Err(e) => tracing::debug!("relay: {e}"),
                },
                Some(Ok(Message::Close(f))) => { tracing::info!("relay closed control: {f:?}"); return Ok(()); }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            _ = ping.tick() => { ws.send(Message::Ping(Vec::new().into())).await?; }
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
        url.push_str(&format!("?token={t}"));
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
