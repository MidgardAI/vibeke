//! Pairing (spec 16 §4): the device's claim inside the encrypted channel, and the operator's
//! `vibeke-gateway pair` command that shows the QR and confirms the device fingerprint.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use vk_e2e::{PairingLink, Session, b64, keys};

use crate::Gateway;
use crate::api::ApiError;
use crate::session::{Ws, spawn_writer};
use crate::state::{
    Device, GitUser, GithubId, Pairing, PairingStatus, PeerInfo, Redemption, Scope, ShareSpec,
    StateDir, now_s,
};
use vk_account::attest::{self, AttestClaims, AttestError, AttestKey};

/// Pairing handshakes per pairing id per minute (spec 16 §4.3).
pub const PER_PAIRING_PER_MIN: usize = 10;
/// Last-resort ceiling on pairing handshakes across all pairing ids, well above one pairing's
/// budget so a busy pairing can't starve the others.
const GLOBAL_PER_MIN: usize = 60;
/// Pair hellos naming an unknown, expired or already claimed pairing id. They cost nothing of a
/// real pairing's budget; past this they are dropped without an answer.
const UNKNOWN_PER_MIN: usize = 30;
/// A pairing the operator rejected this many claims for is withdrawn: its link has leaked.
pub const MAX_REJECTED_CLAIMS: u32 = 5;
const WINDOW: Duration = Duration::from_secs(60);

/// Sliding-window counters for pairing handshakes (spec 16 §4.3, §6.4). A per-pairing budget,
/// so one known host id or one leaked pairing id can't block every other pairing.
#[derive(Default)]
pub struct PairLimiter {
    per_pid: HashMap<String, Vec<Instant>>,
    global: Vec<Instant>,
    unknown: Vec<Instant>,
    rejected: HashMap<String, u32>,
}

fn take(window: &mut Vec<Instant>, max: usize, now: Instant) -> bool {
    window.retain(|t| now.saturating_duration_since(*t) < WINDOW);
    if window.len() >= max {
        return false;
    }
    window.push(now);
    true
}

impl PairLimiter {
    /// A handshake for a known, pending pairing: within its own budget and the global ceiling.
    pub fn allow(&mut self, pid: &str, per_pid: usize, now: Instant) -> bool {
        self.per_pid.retain(|_, w| {
            w.retain(|t| now.saturating_duration_since(*t) < WINDOW);
            !w.is_empty()
        });
        let w = self.per_pid.entry(pid.to_string()).or_default();
        if w.len() >= per_pid {
            return false;
        }
        if !take(&mut self.global, GLOBAL_PER_MIN, now) {
            return false;
        }
        w.push(now);
        true
    }

    /// A pair hello whose pairing id is unknown or no longer claimable.
    pub fn allow_unknown(&mut self, now: Instant) -> bool {
        take(&mut self.unknown, UNKNOWN_PER_MIN, now)
    }

    /// Count an operator rejection for `pid`; true once the pairing should be withdrawn.
    pub fn rejected(&mut self, pid: &str) -> bool {
        let n = self.rejected.entry(pid.to_string()).or_default();
        *n += 1;
        *n >= MAX_REJECTED_CLAIMS
    }

    pub fn forget(&mut self, pid: &str) {
        self.per_pid.remove(pid);
        self.rejected.remove(pid);
    }
}

static LIMITER: LazyLock<Mutex<PairLimiter>> = LazyLock::new(Default::default);

/// Pairing-handshake budget for one known, pending pairing id.
pub fn handshake_allowed(pid: &str, per_pid: usize) -> bool {
    LIMITER.lock().unwrap().allow(pid, per_pid, Instant::now())
}

/// Budget for pair hellos naming no claimable pairing (probing, stale links).
pub fn unknown_allowed() -> bool {
    LIMITER.lock().unwrap().allow_unknown(Instant::now())
}

/// Device-supplied text shown to the operator (terminal prompt, TUI, device list): control
/// characters become spaces and bidirectional overrides/isolates/marks are dropped, so a claim
/// can't move the cursor, clear the prompt or reorder the fingerprint line.
pub fn sanitize_label(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.trim().chars().take(max).collect()
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
    )
}

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a device's relay ticket lasts (spec 16 §6.6); apps renew with `relay.ticket`.
pub const DEVICE_TICKET_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// A relay ticket for device `id` (`dev:<id>`), valid for [`DEVICE_TICKET_TTL`] but never past the
/// device's own expiry. Returns the ticket and its expiry (unix seconds).
pub fn device_ticket(keys: &vk_e2e::HostKeys, id: &str, expires_at: Option<u64>) -> (String, u64) {
    let exp = (now_s() + DEVICE_TICKET_TTL.as_secs()).min(expires_at.unwrap_or(u64::MAX));
    (
        vk_e2e::relay::sign_ticket(keys, &format!("dev:{id}"), exp),
        exp,
    )
}

/// Runs after the IKpsk2 handshake. The device proves psk possession with its first transport
/// message (`pair.claim`); only then is anything recorded (spec 16 §4.2–§4.3).
pub async fn claim(
    gw: Arc<Gateway>,
    ws: impl Ws,
    session: Session,
    pairing: Pairing,
    remote: [u8; 32],
) {
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let (sink, mut stream) = ws.split();
    let (out, writer) = spawn_writer(sink, session.clone());

    let first = tokio::time::timeout(Duration::from_secs(30), stream.next()).await;
    let Ok(Some(Ok(Message::Binary(frame)))) = first else {
        return;
    };
    let Ok(Some(plain)) = session.lock().await.decrypt(&frame) else {
        return;
    };
    let Ok(req) = serde_json::from_slice::<Value>(&plain) else {
        return;
    };
    if req.get("method").and_then(|m| m.as_str()) != Some("pair.claim") {
        return;
    }
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let p = req.get("params").cloned().unwrap_or_default();
    let mut name = sanitize_label(p.get("name").and_then(|v| v.as_str()).unwrap_or(""), 64);
    if name.is_empty() {
        name = "device".into();
    }
    let platform = sanitize_label(p.get("platform").and_then(|v| v.as_str()).unwrap_or(""), 32);
    // A host redeeming a peer or handoff invitation introduces itself (spec 16 §15.3).
    let sender = p.get("peer").filter(|v| v.is_object()).map(peer_identity);
    // A handoff invitation is claimed by one of the claimer's own hosts, never by an app: the
    // work lands on a host, and an app device would only be a courier.
    if pairing.share.as_ref().is_some_and(|s| s.kind == "handoff")
        && (sender.is_none() || platform != "host")
    {
        out.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32003, "message": "this is a handoff invitation; open it on one of your hosts", "data": {"kind": "forbidden"}}})).await;
        return;
    }
    // Watch and review shares: devices come from redemptions, idempotent per device key, with a
    // GitHub attestation when the share asks for one (spec 20 §2.3–§2.4).
    if pairing.redeemable() {
        let attest = p.get("attest").and_then(|v| v.as_str());
        let r = redeem_claim(&gw, &pairing, &remote, &name, &platform, attest).await;
        match r {
            Ok((device, new)) => {
                let fingerprint = keys::fingerprint(&remote);
                out.send(json!({"jsonrpc": "2.0", "id": id, "result": {"status": "pending", "fingerprint": fingerprint}})).await;
                if new {
                    gw.state.audit(&json!({"ts": now_s(), "event": "device.paired", "device": device.id, "name": name, "fingerprint": fingerprint, "kind": device.kind,
                                           "invitation": pairing.pid, "github": device.github.as_ref().map(|g| g.id)}));
                }
                let (ticket, ticket_exp) = device_ticket(&gw.keys, &device.id, device.expires_at);
                out.notify("pair.done", json!({"device_id": device.id, "host_name": gw.host_name, "host_id": gw.keys.host_id(), "scope": device.scope, "kind": device.kind,
                                               "ticket": ticket, "ticket_exp": ticket_exp, "github": device.github,
                                               "review": device.review.as_ref().map(|r| json!({"preview": r.preview, "sid": r.sid}))})).await;
            }
            Err(e) => {
                gw.state.audit(&json!({"ts": now_s(), "event": "share.refused", "invitation": pairing.pid, "reason": e.kind}));
                out.send(json!({"jsonrpc": "2.0", "id": id, "error": refusal_json(&e)}))
                    .await;
            }
        }
        drop(out);
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        return;
    }
    let fingerprint = keys::fingerprint(&remote);

    // Reserve atomically under the registry lock: still pending and unexpired, else refuse.
    let claim_id = b64::encode(keys::random_bytes::<12>());
    let device_public = b64::encode(remote);
    let reserved = (|| -> anyhow::Result<Option<Pairing>> {
        let _lock = gw.state.lock()?;
        let Some(mut current) = gw.state.pairing(&pairing.pid)? else {
            return Ok(None);
        };
        if current.status != PairingStatus::Pending || current.exp <= now_s() {
            return Ok(None);
        }
        current.status = PairingStatus::Claimed {
            claim_id: claim_id.clone(),
            device_public: device_public.clone(),
            fingerprint: fingerprint.clone(),
            device_name: name.clone(),
            platform: platform.clone(),
        };
        current.confirmed = None;
        current.confirmed_claim = None;
        gw.state.save_pairing(&current)?;
        Ok(Some(current))
    })();
    let Ok(Some(current)) = reserved else {
        out.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32003, "message": "pairing is not available", "data": {"kind": "forbidden"}}})).await;
        return;
    };
    out.send(json!({"jsonrpc": "2.0", "id": id, "result": {"status": "pending", "fingerprint": fingerprint}})).await;

    // Wait for the operator (or accept immediately in bearer mode), noticing if the device leaves.
    // Only a confirmation bound to *this* claim counts.
    let deadline = Instant::now() + CONFIRM_TIMEOUT;
    // Ask in the TUI too (server X5 overlay, out of band of any PTY); the first answer wins. Without
    // an attached TUI the call fails `unsupported` and the terminal prompt remains.
    let tui = if current.no_confirm {
        None
    } else {
        let (gw2, pid2, claim2, title, body) = (
            gw.clone(),
            pairing.pid.clone(),
            claim_id.clone(),
            format!("Pair \"{name}\" with {}?", gw.host_name),
            format!(
                "{platform} · fingerprint {fingerprint}\nCheck it matches the phone before pairing."
            ),
        );
        Some(tokio::spawn(async move {
            let r = gw2
                .server
                .call_as(
                    "gateway:pairing",
                    "client.confirm",
                    json!({"title": title, "body": body, "timeout_ms": CONFIRM_TIMEOUT.as_millis() as u64,
                           "options": [{"id": "pair", "label": "Pair"}, {"id": "reject", "label": "Reject"}]}),
                )
                .await;
            if let Ok(v) = r
                && let Some(choice) = v.get("choice").and_then(|c| c.as_str())
            {
                // Same single, claim-bound, first-answer-wins record as the terminal prompt.
                let _ = record_answer(&gw2.state, &pid2, &claim2, choice == "pair");
            }
        }))
    };
    let confirmed = if current.no_confirm {
        true
    } else {
        loop {
            if Instant::now() > deadline {
                break false;
            }
            match gw.state.pairing(&pairing.pid) {
                Ok(Some(p))
                    if p.confirmed.is_some()
                        && p.confirmed_claim.as_deref() == Some(claim_id.as_str()) =>
                {
                    break p.confirmed == Some(true);
                }
                Ok(Some(_)) => {}
                _ => break false,
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                m = stream.next() => if matches!(m, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { break false },
            }
        }
    };

    if let Some(t) = tui {
        t.abort();
    }
    // Consume atomically: the pairing must still be ours, unexpired, then the device is persisted.
    let (kind, peer) = device_kind_for(current.share.as_ref(), sender);
    let device = Device {
        id: ulid::Ulid::generate().to_string().to_lowercase(),
        name: name.clone(),
        platform,
        public: device_public,
        scope: current.scope,
        paired_at: now_s(),
        vapid_private: None,
        push: Vec::new(),
        prefs: Default::default(),
        push_failures: 0,
        kind,
        expires_at: current.share.as_ref().and_then(|sh| sh.device_expiry()),
        limit: current.share.as_ref().and_then(|sh| sh.limit.clone()),
        peer,
        supports_clear: false,
        github: None,
        invitation: None,
        review: None,
    };
    let consumed = confirmed
        && (|| -> anyhow::Result<bool> {
            let lock = gw.state.lock()?;
            let Some(p) = gw.state.pairing(&pairing.pid)? else {
                return Ok(false);
            };
            let ours =
                matches!(&p.status, PairingStatus::Claimed { claim_id: c, .. } if *c == claim_id);
            // The recorded decision is authoritative (an earlier rejection always wins).
            let approved = current.no_confirm
                || (p.confirmed == Some(true)
                    && p.confirmed_claim.as_deref() == Some(claim_id.as_str()));
            if !ours || !approved || p.exp <= now_s() {
                return Ok(false);
            }
            gw.add_device_locked(&lock, device.clone())?;
            gw.state.save_pairing(&Pairing {
                status: PairingStatus::Done {
                    device_id: device.id.clone(),
                },
                ..p
            })?;
            Ok(true)
        })()
        .unwrap_or(false);
    if consumed {
        LIMITER.lock().unwrap().forget(&pairing.pid);
        gw.state.audit(&json!({"ts": now_s(), "event": "device.paired", "device": device.id, "name": name, "fingerprint": fingerprint, "kind": device.kind}));
        let (ticket, ticket_exp) = device_ticket(&gw.keys, &device.id, device.expires_at);
        out.notify("pair.done", json!({"device_id": device.id, "host_name": gw.host_name, "host_id": gw.keys.host_id(), "scope": device.scope, "kind": device.kind,
                                       "ticket": ticket, "ticket_exp": ticket_exp})).await;
    } else {
        // Back to pending: a hijacked claim must not lock the owner out (spec 16 §4.3). But a
        // pairing whose claims the operator keeps rejecting has leaked; withdraw it.
        if let Ok(_lock) = gw.state.lock()
            && let Ok(Some(mut p)) = gw.state.pairing(&pairing.pid)
            && matches!(&p.status, PairingStatus::Claimed { claim_id: c, .. } if *c == claim_id)
        {
            let rejected = p.confirmed == Some(false)
                && p.confirmed_claim.as_deref() == Some(claim_id.as_str());
            if rejected && LIMITER.lock().unwrap().rejected(&pairing.pid) {
                tracing::warn!(pid = %pairing.pid, "pairing withdrawn after repeated rejected claims");
                gw.state.audit(&json!({"ts": now_s(), "event": "pairing.withdrawn", "reason": "rejected_claims"}));
                let _ = gw.state.remove_pairing(&pairing.pid);
            } else {
                p.status = PairingStatus::Pending;
                p.confirmed = None;
                p.confirmed_claim = None;
                let _ = gw.state.save_pairing(&p);
            }
        }
        out.notify("pair.rejected", json!({})).await;
    }
    drop(out);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}

/// Create a pending pairing and its link.
pub fn create(
    state: &StateDir,
    relay: &str,
    host_name: &str,
    scope: Scope,
    no_confirm: bool,
    ttl: Duration,
) -> Result<(Pairing, PairingLink)> {
    create_with(state, relay, host_name, scope, no_confirm, ttl, None)
}

/// Create a share or handoff invitation (spec 16 §15): a bearer pairing whose device is limited
/// and expires.
pub fn create_with(
    state: &StateDir,
    relay: &str,
    host_name: &str,
    scope: Scope,
    no_confirm: bool,
    ttl: Duration,
    share: Option<crate::state::ShareSpec>,
) -> Result<(Pairing, PairingLink)> {
    create_invitation(state, relay, host_name, scope, no_confirm, ttl, share, 1)
}

/// [`create_with`] for an invitation `uses` devices may redeem (spec 20 §2.4). The link carries
/// the share's `access`, `uses` and, for a review, `notes`, `preview` and `sid`.
#[allow(clippy::too_many_arguments)]
pub fn create_invitation(
    state: &StateDir,
    relay: &str,
    host_name: &str,
    scope: Scope,
    no_confirm: bool,
    ttl: Duration,
    share: Option<crate::state::ShareSpec>,
    uses: u32,
) -> Result<(Pairing, PairingLink)> {
    let uses = uses.clamp(1, MAX_USES);
    let keys = state.host_keys()?;
    let pid = b64::encode(keys::random_bytes::<9>());
    let psk = b64::encode(keys::random_bytes::<32>());
    let exp = now_s() + ttl.as_secs();
    let pairing = Pairing {
        pid: pid.clone(),
        psk: psk.clone(),
        exp,
        scope,
        name: None,
        no_confirm,
        status: PairingStatus::Pending,
        confirmed: None,
        confirmed_claim: None,
        share: share.clone(),
        created_at: now_s(),
        uses,
        redemptions: Vec::new(),
    };
    state.save_pairing(&pairing)?;
    let share_json = share.map(|sh| {
        let mut j = serde_json::json!({"kind": sh.kind, "scope": scope.as_str(), "until": sh.until,
                                       "label": sh.label, "limit": sh.limit});
        if sh.redeemable() {
            j["access"] = json!(sh.access.as_deref().unwrap_or("link"));
            j["uses"] = json!(uses);
        }
        if let Some(r) = &sh.review {
            j["notes"] = json!(r.notes);
            j["preview"] = json!({"id": r.preview, "label": r.label});
            j["sid"] = json!(r.sid);
        }
        j
    });
    // Admits the claim on relays that require tickets (spec 16 §6.6); harmless elsewhere.
    let tk = vk_e2e::relay::sign_ticket(&keys, &format!("pid:{pid}"), exp);
    let link = PairingLink {
        v: 1,
        relay: crate::relay_client::ws_base(relay),
        host: keys.host_id(),
        hk: b64::encode(keys.noise_public()),
        pid,
        psk,
        exp,
        name: host_name.into(),
        share: share_json,
        tk: Some(tk),
    };
    Ok((pairing, link))
}

/// The `peer` object of a host's `pair.claim`: `{host_name, user?: {name, email}}`, trimmed.
fn peer_identity(v: &Value) -> PeerInfo {
    let text = |v: Option<&Value>, max: usize| -> Option<String> {
        v.and_then(|x| x.as_str())
            .map(|x| sanitize_label(x, max))
            .filter(|x| !x.is_empty())
    };
    let user = v.get("user").filter(|u| u.is_object()).map(|u| GitUser {
        name: text(u.get("name"), 128),
        email: text(u.get("email"), 254),
    });
    PeerInfo {
        owner: String::new(),
        host_name: text(v.get("host_name"), 64),
        user: user.filter(|u| u.name.is_some() || u.email.is_some()),
    }
}

/// The device kind a claim produces: a `peer` invitation makes one of the owner's own hosts; a
/// handoff invitation (only a host may claim one) makes a teammate's host; anything else keeps the
/// invitation's kind (or `device` for a plain pairing).
pub fn device_kind_for(
    share: Option<&crate::state::ShareSpec>,
    sender: Option<PeerInfo>,
) -> (String, Option<PeerInfo>) {
    match share.map(|s| s.kind.as_str()) {
        Some("peer") => {
            let owner = share
                .and_then(|s| s.owner.clone())
                .unwrap_or_else(|| "self".into());
            let info = PeerInfo {
                owner,
                ..sender.unwrap_or_default()
            };
            ("peer".into(), Some(info))
        }
        Some("handoff") => {
            let info = PeerInfo {
                owner: "teammate".into(),
                ..sender.unwrap_or_default()
            };
            ("peer".into(), Some(info))
        }
        // A review device is a share device with `review` set (spec 20 §3.2).
        Some("review") => ("share".into(), None),
        Some(k) => (k.to_string(), None),
        None => ("device".into(), None),
    }
}

/// Most devices one invitation may make (spec 20 §2.2).
pub const MAX_USES: u32 = 100;
/// How long the account server's attestation keys are cached.
pub const KEYS_TTL: Duration = Duration::from_secs(3600);
/// Least time between key fetches caused by an unknown `kid`.
pub const REFETCH_MIN: Duration = Duration::from_secs(60);

/// A definitive or transient refusal of a `pair.claim`, with the kind the app acts on
/// (spec 20 §2.4: it keeps the invitation key unless the refusal is definitive).
pub fn refuse(kind: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(kind, message)
}

/// The JSON-RPC error of a refused claim (code -32003 like every pairing refusal).
pub fn refusal_json(e: &ApiError) -> Value {
    json!({"code": -32003, "message": e.message, "data": {"kind": e.kind, "details": e.details}})
}

/// The account server's attestation keys (spec 20 §2.3): cached for [`KEYS_TTL`], fetched again
/// at most once per [`REFETCH_MIN`] when a token names an unknown key.
#[derive(Default)]
pub struct AttestKeyCache {
    st: tokio::sync::Mutex<KeyState>,
}

#[derive(Default)]
struct KeyState {
    server: String,
    keys: Vec<AttestKey>,
    fetched: Option<Instant>,
    /// The last fetch, of any cause; an unknown `kid` refetches only when it is old enough.
    last_fetch: Option<Instant>,
}

impl AttestKeyCache {
    /// The keys for `server`, fetched with `fetch` when the cache is cold or older than
    /// [`KEYS_TTL`], or when `unknown_kid` and the last fetch is older than [`REFETCH_MIN`]. A
    /// failed refetch keeps the cached keys; a failed first fetch is an error.
    pub async fn keys<F, Fut>(
        &self,
        server: &str,
        unknown_kid: bool,
        now: Instant,
        fetch: F,
    ) -> Result<Vec<AttestKey>, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = vk_account::Result<Vec<AttestKey>>>,
    {
        let mut st = self.st.lock().await;
        let fresh = st.server == server
            && st
                .fetched
                .is_some_and(|t| now.saturating_duration_since(t) < KEYS_TTL);
        let refetch = unknown_kid
            && st
                .last_fetch
                .is_none_or(|t| now.saturating_duration_since(t) >= REFETCH_MIN);
        if fresh && !refetch {
            return Ok(st.keys.clone());
        }
        st.last_fetch = Some(now);
        match fetch().await {
            Ok(k) => {
                st.server = server.to_string();
                st.keys = k;
                st.fetched = Some(now);
                Ok(st.keys.clone())
            }
            Err(_) if fresh => Ok(st.keys.clone()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Check the claims of a verified attestation against this invitation (spec 20 §2.3): `aud` is
/// this host's relay id, `nonce` binds the pid and the claiming device's static key, and the
/// GitHub id is on the share's list (or the list is `["*"]`).
pub fn check_claims(
    claims: &AttestClaims,
    host_id: &str,
    pid: &str,
    remote: &[u8; 32],
    share: &ShareSpec,
) -> Result<GithubId, ApiError> {
    if claims.aud != host_id {
        return Err(refuse(
            "attest_invalid",
            "the GitHub sign-in was made for another host",
        ));
    }
    if claims.nonce != attest::nonce(pid, remote) {
        return Err(refuse(
            "attest_invalid",
            "the GitHub sign-in was made for another link or device",
        ));
    }
    if !(share.github_any || share.github.iter().any(|g| g.id == claims.gh_id)) {
        let mut e = refuse(
            "github_not_allowed",
            "this link is for other GitHub accounts",
        );
        e.details =
            json!({"allowed": share.github.iter().map(|g| g.login.clone()).collect::<Vec<_>>()});
        return Err(e);
    }
    Ok(GithubId {
        id: claims.gh_id,
        login: sanitize_label(&claims.login, 39),
    })
}

/// Verify the attestation a claim carries for a `github` share: signature by a published key,
/// times, then [`check_claims`]. No attestation is `upgrade_required` (an older app); the claim
/// never falls back to an unauthenticated one.
pub async fn check_attest(
    gw: &Gateway,
    pairing: &Pairing,
    share: &ShareSpec,
    remote: &[u8; 32],
    token: Option<&str>,
) -> Result<GithubId, ApiError> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Err(refuse(
            "upgrade_required",
            "This link needs a newer Vibeke app",
        ));
    };
    let server = crate::account::account_server(&gw.cfg);
    let client = vk_account::Client::new(&server)
        .map_err(|e| refuse("unavailable", format!("account server: {e}")))?;
    let keys = gw
        .attest_keys
        .keys(&server, false, Instant::now(), || client.attest_keys())
        .await
        .map_err(|e| refuse("unavailable", format!("account server keys: {e}")))?;
    let claims = match attest::verify(token, &keys, now_s()) {
        Err(AttestError::UnknownKid(_)) => {
            let keys = gw
                .attest_keys
                .keys(&server, true, Instant::now(), || client.attest_keys())
                .await
                .map_err(|e| refuse("unavailable", format!("account server keys: {e}")))?;
            attest::verify(token, &keys, now_s())
        }
        r => r,
    }
    .map_err(|e| refuse("attest_invalid", e.to_string()))?;
    check_claims(&claims, &gw.keys.host_id(), &pairing.pid, remote, share)
}

/// A claim on a watch or review share: an earlier redemption by the same device key gets its
/// device back without a new attestation; otherwise a `github` share needs a valid one first.
async fn redeem_claim(
    gw: &Arc<Gateway>,
    pairing: &Pairing,
    remote: &[u8; 32],
    name: &str,
    platform: &str,
    attest: Option<&str>,
) -> Result<(Device, bool), ApiError> {
    let current = gw
        .state
        .pairing(&pairing.pid)
        .ok()
        .flatten()
        .ok_or_else(|| refuse("revoked", "this link was revoked"))?;
    let share = current
        .share
        .clone()
        .ok_or_else(|| refuse("forbidden", "pairing is not available"))?;
    let known = current.redemption_by(&b64::encode(remote)).is_some();
    let github = if share.github_access() && !known {
        Some(check_attest(gw, &current, &share, remote, attest).await?)
    } else {
        None
    };
    let (gw2, pid, remote, name, platform) = (
        gw.clone(),
        pairing.pid.clone(),
        *remote,
        name.to_string(),
        platform.to_string(),
    );
    // The registry lock is a blocking flock.
    tokio::task::spawn_blocking(move || redeem(&gw2, &pid, &remote, &name, &platform, github))
        .await
        .map_err(|_| refuse("unavailable", "the claim was interrupted"))?
}

/// Redeem invitation `pid` for device key `remote` under the registry lock (spec 20 §2.4):
///
/// 1. An existing redemption by this key returns its device again (no second device). A
///    redemption whose device was revoked stays refused.
/// 2. Otherwise the use count is checked, the device is created, the redemption recorded and
///    the pairing written, in that order (see `StateDir::reconcile_redemptions`).
///
/// Returns the device and whether it is new.
pub fn redeem(
    gw: &Gateway,
    pid: &str,
    remote: &[u8; 32],
    name: &str,
    platform: &str,
    github: Option<GithubId>,
) -> Result<(Device, bool), ApiError> {
    let internal = |e: anyhow::Error| crate::api::internal(e);
    let lock = gw.state.lock().map_err(internal)?;
    let Some(mut p) = gw.state.pairing(pid).ok().flatten() else {
        return Err(refuse("revoked", "this link was revoked"));
    };
    let share = match &p.share {
        Some(sh) if p.redeemable() => sh.clone(),
        _ => return Err(refuse("forbidden", "pairing is not available")),
    };
    if p.exp <= now_s() {
        return Err(refuse("expired", "this link expired"));
    }
    let key = b64::encode(remote);
    let devices = gw.state.devices().map_err(internal)?;
    if let Some(r) = p.redemption_by(&key) {
        if r.revoked {
            return Err(refuse("revoked", "this device was removed from the share"));
        }
        return devices
            .iter()
            .find(|d| d.id == r.device_id && !d.expired())
            .cloned()
            .map(|d| (d, false))
            .ok_or_else(|| refuse("revoked", "this device was removed from the share"));
    }
    if let Some(d) = devices.iter().find(|d| d.public == key) {
        // Written before a crash cut off the pairing write: record its redemption now.
        if d.invitation.as_deref() != Some(pid) || d.expired() {
            return Err(refuse(
                "forbidden",
                "this device key is already paired with this host",
            ));
        }
        p.redemptions.push(Redemption {
            device_static: key,
            device_id: d.id.clone(),
            at: d.paired_at,
            revoked: false,
        });
        if p.exhausted() {
            p.status = PairingStatus::Done {
                device_id: d.id.clone(),
            };
        }
        gw.state.save_pairing(&p).map_err(internal)?;
        return Ok((d.clone(), false));
    }
    if p.exhausted() || p.status != PairingStatus::Pending {
        return Err(refuse("used", "this link has been used"));
    }
    if share.github_access() && github.is_none() {
        return Err(refuse(
            "upgrade_required",
            "This link needs a newer Vibeke app",
        ));
    }
    let (kind, _) = device_kind_for(Some(&share), None);
    let device = Device {
        id: ulid::Ulid::generate().to_string().to_lowercase(),
        name: match &github {
            Some(g) => format!("@{}", g.login),
            None => name.to_string(),
        },
        platform: platform.to_string(),
        public: key.clone(),
        scope: p.scope,
        paired_at: now_s(),
        vapid_private: None,
        push: Vec::new(),
        prefs: Default::default(),
        push_failures: 0,
        kind,
        expires_at: share.device_expiry(),
        limit: share.limit.clone(),
        peer: None,
        supports_clear: false,
        github,
        invitation: Some(pid.to_string()),
        review: share.review.clone(),
    };
    gw.add_device_locked(&lock, device.clone())
        .map_err(internal)?;
    p.redemptions.push(Redemption {
        device_static: key,
        device_id: device.id.clone(),
        at: device.paired_at,
        revoked: false,
    });
    if p.exhausted() {
        p.status = PairingStatus::Done {
            device_id: device.id.clone(),
        };
    }
    gw.state.save_pairing(&p).map_err(internal)?;
    Ok((device, true))
}

pub fn render_qr(text: &str) -> String {
    match qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L) {
        Ok(code) => code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
        Err(_) => String::from("(link too long for a QR code; use the URL)"),
    }
}

/// Record the operator's answer for one claim: only if that claim is still the current one and
/// nobody answered yet (terminal and TUI prompts race; the first recorded answer wins).
pub fn record_answer(state: &StateDir, pid: &str, claim: &str, yes: bool) -> Result<bool> {
    let _lock = state.lock()?;
    match state.pairing(pid)? {
        Some(mut p)
            if p.confirmed.is_none()
                && matches!(&p.status, PairingStatus::Claimed { claim_id, .. } if claim_id == claim) =>
        {
            p.confirmed = Some(yes);
            p.confirmed_claim = Some(claim.to_string());
            state.save_pairing(&p)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// The terminal question for a claim. Device-supplied text is sanitized again (the pairing file
/// may come from an older gateway), and the fingerprint sits alone on the last line, where
/// nothing the device sent can precede it on the same line.
fn confirm_prompt(name: &str, platform: &str, fingerprint: &str) -> String {
    let name = sanitize_label(name, 64);
    let platform = sanitize_label(platform, 32);
    let fingerprint = sanitize_label(fingerprint, 128);
    format!(
        "Pair \"{name}\" ({platform})? Check this fingerprint matches the phone:\n  {fingerprint}\nPair? [y/N] "
    )
}

/// Interactive side of `vibeke-gateway pair`.
pub async fn wait_and_confirm(state: &StateDir, pid: &str, exp: u64) -> Result<()> {
    // The claim the operator was last asked about; answers are bound to it.
    let mut asked: Option<String> = None;
    loop {
        if now_s() > exp {
            let _ = state.remove_pairing(pid);
            bail!("pairing expired");
        }
        let Some(p) = state.pairing(pid)? else {
            bail!("pairing was removed")
        };
        match &p.status {
            PairingStatus::Done { device_id } => {
                let _ = state.remove_pairing(pid);
                println!("Paired ✓ (device id {device_id})");
                return Ok(());
            }
            PairingStatus::Claimed {
                claim_id,
                fingerprint,
                device_name: name,
                platform,
                ..
            } if asked.as_deref() != Some(claim_id.as_str()) => {
                let shown = claim_id.clone();
                asked = Some(shown.clone());
                let prompt = confirm_prompt(name, platform, fingerprint);
                let answer = tokio::task::spawn_blocking(move || {
                    print!("{prompt}");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                    line
                })
                .await?;
                let yes = matches!(answer.trim(), "y" | "Y" | "yes");
                let applied = record_answer(state, pid, &shown, yes)?;
                if !applied {
                    println!("Already answered (in the TUI?) or the device went away; waiting.");
                    continue;
                }
                if !yes {
                    println!(
                        "Rejected. The code stays valid until it expires; run `pair` again for a new one."
                    );
                }
            }
            PairingStatus::Pending => asked = None,
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[cfg(test)]
#[path = "pair_share_tests.rs"]
mod share_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_pairing_budget_is_independent() {
        let mut l = PairLimiter::default();
        let now = Instant::now();
        for _ in 0..PER_PAIRING_PER_MIN {
            assert!(l.allow("a", PER_PAIRING_PER_MIN, now));
        }
        assert!(!l.allow("a", PER_PAIRING_PER_MIN, now));
        // Another pairing is unaffected by the exhausted one.
        assert!(l.allow("b", PER_PAIRING_PER_MIN, now));
        // The window slides.
        assert!(l.allow(
            "a",
            PER_PAIRING_PER_MIN,
            now + WINDOW + Duration::from_secs(1)
        ));
    }

    #[test]
    fn unknown_pairing_ids_do_not_spend_real_budgets() {
        let mut l = PairLimiter::default();
        let now = Instant::now();
        while l.allow_unknown(now) {}
        assert!(!l.allow_unknown(now));
        assert!(l.allow("real", PER_PAIRING_PER_MIN, now));
    }

    #[test]
    fn global_ceiling_holds_across_pairings() {
        let mut l = PairLimiter::default();
        let now = Instant::now();
        let mut ok = 0;
        for i in 0..GLOBAL_PER_MIN + 10 {
            if l.allow(&format!("p{i}"), PER_PAIRING_PER_MIN, now) {
                ok += 1;
            }
        }
        assert_eq!(ok, GLOBAL_PER_MIN);
        // A refused handshake does not count against the pairing itself.
        assert!(
            l.per_pid
                .get(&format!("p{}", GLOBAL_PER_MIN + 1))
                .is_none_or(|w| w.is_empty())
        );
    }

    #[test]
    fn repeated_rejections_withdraw_the_pairing() {
        let mut l = PairLimiter::default();
        for _ in 1..MAX_REJECTED_CLAIMS {
            assert!(!l.rejected("x"));
        }
        assert!(l.rejected("x"));
        assert!(!l.rejected("y"));
        l.forget("x");
        assert!(!l.rejected("x"));
    }

    #[test]
    fn labels_lose_control_and_bidi_characters() {
        assert_eq!(sanitize_label("Phone\x1b[2J\x1b[H", 64), "Phone [2J [H");
        assert_eq!(sanitize_label("a\rb\nc\u{7}", 64), "a b c");
        assert_eq!(
            sanitize_label(
                "x\u{202E}evil\u{2066}\u{2069}\u{200E}\u{200F}\u{061C}\u{202A}",
                64
            ),
            "xevil"
        );
        assert_eq!(sanitize_label("  ok  ", 64), "ok");
        assert_eq!(sanitize_label(&"é".repeat(100), 64).chars().count(), 64);
        // C1 controls too (CSI is U+009B).
        assert_eq!(sanitize_label("a\u{9b}2Jb", 64), "a 2Jb");
    }

    #[test]
    fn prompt_puts_the_fingerprint_on_its_own_line() {
        let p = confirm_prompt("Phone\r\x1b[KFake", "ios\u{202E}", "AB12 CD34");
        assert!(!p.chars().any(|c| c.is_control() && c != '\n'));
        let lines: Vec<&str> = p.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].trim(), "AB12 CD34");
        assert!(!lines[0].contains('\u{202E}'));
    }
}
