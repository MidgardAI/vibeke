//! The gateway as a client of another gateway (spec 16 §15.3): pairs with a peer or handoff
//! invitation (`hello {mode:"pair"}` + Noise IKpsk2, then `pair.claim`), then opens authenticated
//! IK connections and makes JSON-RPC calls over the encrypted channel, exactly as an app does.
//!
//! Transport: the relay's `/v1/connect`, or the gateway's local socket when the link's relay is
//! `local:<path>` (both gateways on this machine).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use vk_e2e::{DeviceKey, Hello, Initiator, PairingLink, Session, b64};

use crate::api::ApiError;
use crate::session::Ws;
use crate::state::{GitUser, PeerRecord, now_s};

/// Opening the socket and finishing the Noise handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// One call's answer (the session's idle limit is 60 s).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The host's confirmation of a pairing (its side gives up after 120 s).
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(150);
/// WebSocket frame/message cap: the channel splits messages into ≤ 64 KiB Noise frames, as the
/// gateway's own sockets expect (local.rs).
const MAX_WS: usize = 128 * 1024;

/// How this host introduces itself when it redeems an invitation.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    /// Shown on the other host as the device name.
    pub host_name: String,
    /// The git identity to show, only when the user chose to share it.
    pub user: Option<GitUser>,
}

pub struct PeerClient;

impl PeerClient {
    /// Redeem `link` (a `peer` or handoff invitation) as this host: returns the record to keep in
    /// `peers.json`. Plain pairing links and share invitations are refused: a host only ever
    /// pairs as a peer.
    pub async fn pair(link: &PairingLink, us: &Identity) -> Result<PeerRecord> {
        let kind = link
            .share
            .as_ref()
            .and_then(|s| s.get("kind"))
            .and_then(|k| k.as_str())
            .unwrap_or("");
        let owner = match kind {
            "peer" => "self",
            "handoff" => "teammate",
            _ => bail!("not a peer or handoff invitation"),
        };
        Self::pair_link(link, us, owner).await
    }

    /// Development tools only (examples/devclient): redeem any pairing link, including a plain
    /// one (the result is an ordinary device there, confirmed by the operator).
    pub async fn pair_any(link: &PairingLink, us: &Identity) -> Result<PeerRecord> {
        let kind = link
            .share
            .as_ref()
            .and_then(|s| s.get("kind"))
            .and_then(|k| k.as_str());
        let owner = if kind == Some("handoff") {
            "teammate"
        } else {
            "self"
        };
        Self::pair_link(link, us, owner).await
    }

    async fn pair_link(link: &PairingLink, us: &Identity, owner: &str) -> Result<PeerRecord> {
        if link.exp <= now_s() {
            bail!("this invitation has expired");
        }
        let key = DeviceKey::generate();
        let hk = link.host_key().context("link host key")?;
        let psk = link.psk_bytes().context("link secret")?;
        let mut c = Conn::open(
            &link.relay,
            &link.host,
            Hello::pair(&link.pid),
            &key,
            &hk,
            Some(&psk),
        )
        .await?;
        let mut peer = json!({"host_name": us.host_name});
        if let Some(u) = &us.user {
            peer["user"] = json!({"name": u.name, "email": u.email});
        }
        let r = c
            .request(
                "pair.claim",
                json!({"name": us.host_name, "platform": "host", "peer": peer}),
            )
            .await
            .map_err(|e| anyhow::anyhow!("pair.claim: {}", e.message))?;
        if r.get("status").and_then(|s| s.as_str()) != Some("pending") {
            bail!("unexpected pair.claim answer: {r}");
        }
        let done = tokio::time::timeout(PAIR_TIMEOUT, c.pair_outcome())
            .await
            .context("the host did not confirm in time")??;
        let device_id = done
            .get("device_id")
            .and_then(|d| d.as_str())
            .context("malformed pair.done")?
            .to_string();
        let name = done
            .get("host_name")
            .and_then(|n| n.as_str())
            .unwrap_or(&link.name)
            .to_string();
        let expires_at = match owner {
            "teammate" => link
                .share
                .as_ref()
                .and_then(|s| s.get("until"))
                .and_then(|u| u.as_u64())
                .filter(|u| *u > 0),
            _ => None,
        };
        c.close().await;
        Ok(PeerRecord {
            id: ulid::Ulid::new().to_string().to_lowercase(),
            name,
            relay: link.relay.clone(),
            host: link.host.clone(),
            host_key: link.hk.clone(),
            device_key: b64::encode(key.private),
            device_id,
            owner: owner.into(),
            added_at: now_s(),
            expires_at,
        })
    }

    /// An authenticated connection to a paired peer.
    pub async fn connect(rec: &PeerRecord) -> Result<Conn> {
        if rec.expired() {
            bail!("access to {} has expired", rec.name);
        }
        let key = DeviceKey {
            private: b64::decode_array(&rec.device_key).context("peer key")?,
        };
        let hk = b64::decode_array(&rec.host_key).context("peer host key")?;
        Conn::open(&rec.relay, &rec.host, Hello::device(), &key, &hk, None).await
    }

    /// [`connect`](Self::connect) with retries (full-jitter exponential backoff) until `deadline`
    /// passes. A refusal (`unauthorized`: revoked or expired) is final and not retried.
    pub async fn connect_with_backoff(rec: &PeerRecord, deadline: Duration) -> Result<Conn> {
        let until = tokio::time::Instant::now() + deadline;
        let mut b = Backoff::default();
        loop {
            match Self::connect(rec).await {
                Ok(c) => return Ok(c),
                Err(e) if is_refusal(&e) => return Err(e),
                Err(e) => {
                    let wait = b.next_delay();
                    if tokio::time::Instant::now() + wait > until {
                        return Err(e);
                    }
                    tracing::debug!(peer = %rec.name, "connect failed, retrying in {wait:?}: {e:#}");
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

/// The host answered the handshake with a plaintext refusal (revoked, expired or unknown key).
pub fn is_refusal(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("unauthorized") || s.contains("has expired")
}

/// Exponential backoff with full jitter: 0.5 s, 1 s, 2 s, … capped at `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
    attempt: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            min: Duration::from_millis(500),
            max: Duration::from_secs(30),
            attempt: 0,
        }
    }
}

impl Backoff {
    /// The ceiling for the next wait (before jitter).
    pub fn ceiling(&self) -> Duration {
        self.min
            .saturating_mul(1u32 << self.attempt.min(16))
            .min(self.max)
    }

    pub fn next_delay(&mut self) -> Duration {
        let c = self.ceiling();
        self.attempt += 1;
        c.mul_f64(rand::random::<f64>().max(0.1))
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// One encrypted JSON-RPC connection to another gateway.
pub struct Conn {
    ws: Box<dyn Ws>,
    session: Session,
    next: u64,
    /// The host's handshake payload: `{v, host_name, host_id, gateway_version, server_version}`.
    pub info: Value,
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WS))
        .max_frame_size(Some(MAX_WS))
}

/// `local:<socket path>` → the path.
pub fn local_socket(relay: &str) -> Option<PathBuf> {
    relay
        .strip_prefix("local:")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

async fn dial(relay: &str, host: &str) -> Result<Box<dyn Ws>> {
    if let Some(path) = local_socket(relay) {
        let stream = tokio::net::UnixStream::connect(&path)
            .await
            .with_context(|| format!("connect {}", path.display()))?;
        let (ws, _) = tokio_tungstenite::client_async_with_config(
            "ws://localhost/",
            stream,
            Some(ws_config()),
        )
        .await?;
        return Ok(Box::new(ws));
    }
    if relay.starts_with("local") {
        bail!("local transport link without a socket path");
    }
    let url = format!(
        "{}/v1/connect?host={host}",
        crate::relay_client::ws_base(relay)
    );
    let (ws, _) =
        tokio_tungstenite::connect_async_with_config(url.as_str(), Some(ws_config()), false)
            .await
            .with_context(|| format!("connect {}", crate::relay_client::ws_base(relay)))?;
    Ok(Box::new(ws))
}

impl Conn {
    async fn open(
        relay: &str,
        host: &str,
        hello: Hello,
        key: &DeviceKey,
        hk: &[u8; 32],
        psk: Option<&[u8; 32]>,
    ) -> Result<Conn> {
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            Self::handshake(relay, host, hello, key, hk, psk),
        )
        .await
        .context("handshake timed out")?
    }

    async fn handshake(
        relay: &str,
        host: &str,
        hello: Hello,
        key: &DeviceKey,
        hk: &[u8; 32],
        psk: Option<&[u8; 32]>,
    ) -> Result<Conn> {
        let mut ws = dial(relay, host).await?;
        let hb = hello.to_bytes();
        ws.send(Message::Text(String::from_utf8(hb.clone())?.into()))
            .await?;
        let mut i = Initiator::new(&hb, &key.private, hk, psk)?;
        ws.send(Message::Binary(i.write_first(b"")?.into())).await?;
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(m2))) => {
                    let (payload, session) = i.read_second(&m2)?;
                    let info: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                    if let Some(id) = info.get("host_id").and_then(|h| h.as_str())
                        && id != host
                    {
                        bail!("connected to host {id}, expected {host}");
                    }
                    return Ok(Conn {
                        ws,
                        session,
                        next: 1,
                        info,
                    });
                }
                Some(Ok(Message::Text(t))) => bail!("refused: {}", t.as_str()),
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                other => bail!("handshake failed: {other:?}"),
            }
        }
    }

    /// After `pair.claim`: wait for `pair.done` (its params) or `pair.rejected`.
    async fn pair_outcome(&mut self) -> Result<Value> {
        loop {
            let m = self.recv().await?;
            match m.get("method").and_then(|m| m.as_str()) {
                Some("pair.done") => return Ok(m["params"].clone()),
                Some("pair.rejected") => bail!("the host rejected the pairing"),
                _ => {}
            }
        }
    }

    /// The next decrypted message (a response or a notification).
    pub async fn recv(&mut self) -> Result<Value> {
        loop {
            match tokio::time::timeout(CALL_TIMEOUT, self.ws.next())
                .await
                .context("peer timed out")?
            {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(m) = self.session.decrypt(&b)? {
                        return Ok(serde_json::from_slice(&m)?);
                    }
                }
                Some(Ok(Message::Close(f))) => bail!("closed: {f:?}"),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => bail!("closed"),
            }
        }
    }

    async fn send(&mut self, v: &Value) -> Result<()> {
        for f in self.session.encrypt(v.to_string().as_bytes())? {
            self.ws.send(Message::Binary(f.into())).await?;
        }
        Ok(())
    }

    /// Send one request and wait for its response, skipping notifications.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, ApiError> {
        let id = self.next;
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&msg)
            .await
            .map_err(|e| ApiError::unavailable(format!("{e:#}")))?;
        loop {
            let m = self
                .recv()
                .await
                .map_err(|e| ApiError::unavailable(format!("{e:#}")))?;
            if m.get("method").and_then(|x| x.as_str()) == Some("device.revoked") {
                return Err(ApiError::new(
                    "forbidden",
                    "this host's access was revoked there",
                ));
            }
            if m.get("id").and_then(|x| x.as_u64()) != Some(id) {
                continue;
            }
            if let Some(e) = m.get("error") {
                return Err(ApiError {
                    kind: e
                        .pointer("/data/kind")
                        .and_then(|k| k.as_str())
                        .unwrap_or("internal")
                        .into(),
                    message: e
                        .get("message")
                        .and_then(|k| k.as_str())
                        .unwrap_or("error")
                        .into(),
                    details: e.pointer("/data/details").cloned().unwrap_or(Value::Null),
                });
            }
            return Ok(m.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Call an app-API method on the peer. Object params get an `op_id` when they have none
    /// (mutating methods need one; the others ignore it).
    pub async fn call(&mut self, method: &str, mut params: Value) -> Result<Value, ApiError> {
        if let Some(m) = params.as_object_mut() {
            m.entry("op_id")
                .or_insert_with(|| ulid::Ulid::new().to_string().into());
        }
        self.request(method, params).await
    }

    /// Like [`call`](Self::call) without adding an `op_id` (devclient's raw mode).
    pub async fn call_raw(&mut self, method: &str, params: Value) -> Result<Value, ApiError> {
        self.request(method, params).await
    }

    pub async fn close(mut self) {
        let _ = self.ws.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut b = Backoff::default();
        assert_eq!(b.ceiling(), Duration::from_millis(500));
        for _ in 0..3 {
            let d = b.next_delay();
            assert!(d <= Duration::from_secs(4));
        }
        assert_eq!(b.ceiling(), Duration::from_secs(4));
        for _ in 0..40 {
            assert!(b.next_delay() <= Duration::from_secs(30));
        }
        assert_eq!(b.ceiling(), Duration::from_secs(30));
        b.reset();
        assert_eq!(b.ceiling(), Duration::from_millis(500));
    }

    #[test]
    fn local_links() {
        assert_eq!(
            local_socket("local:/tmp/g.sock"),
            Some(PathBuf::from("/tmp/g.sock"))
        );
        assert_eq!(local_socket("local:"), None);
        assert_eq!(local_socket("wss://relay"), None);
    }

    #[tokio::test]
    async fn refuses_non_peer_links() {
        let link = PairingLink {
            v: 1,
            relay: "ws://127.0.0.1:1".into(),
            host: "h".into(),
            hk: b64::encode([1u8; 32]),
            pid: "p".into(),
            psk: b64::encode([2u8; 32]),
            exp: now_s() + 60,
            name: "x".into(),
            share: Some(json!({"kind": "share", "scope": "view", "until": 1})),
        };
        let e = PeerClient::pair(&link, &Identity::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not a peer"), "{e}");
        let mut plain = link.clone();
        plain.share = None;
        assert!(
            PeerClient::pair(&plain, &Identity::default())
                .await
                .is_err()
        );
        let mut old = link;
        old.share = Some(json!({"kind": "peer", "scope": "full", "until": 0}));
        old.exp = now_s() - 1;
        let e = PeerClient::pair(&old, &Identity::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("expired"), "{e}");
    }
}
