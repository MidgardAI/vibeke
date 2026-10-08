//! Pairing (spec 16 §4): the device's claim inside the encrypted channel, and the operator's
//! `vibeke-gateway pair` command that shows the QR and confirms the device fingerprint.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use vk_e2e::{PairingLink, Session, b64, keys};

use crate::Gateway;
use crate::session::{Ws, spawn_writer};
use crate::state::{Device, GitUser, Pairing, PairingStatus, PeerInfo, Scope, StateDir, now_s};

static HANDSHAKES: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

/// Global pairing-handshake budget (spec 16 §6.4: ≤ 4/min).
pub fn handshake_allowed() -> bool {
    let mut h = HANDSHAKES.lock().unwrap();
    h.retain(|t| t.elapsed() < Duration::from_secs(60));
    if h.len() >= 4 {
        return false;
    }
    h.push(Instant::now());
    true
}

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

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
    let name: String = p
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("device")
        .chars()
        .take(64)
        .collect();
    let platform: String = p
        .get("platform")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(32)
        .collect();
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
        id: ulid::Ulid::new().to_string().to_lowercase(),
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
        gw.state.audit(&json!({"ts": now_s(), "event": "device.paired", "device": device.id, "name": name, "fingerprint": fingerprint, "kind": device.kind}));
        out.notify("pair.done", json!({"device_id": device.id, "host_name": gw.host_name, "host_id": gw.keys.host_id(), "scope": device.scope, "kind": device.kind})).await;
    } else {
        // Back to pending: a hijacked claim must not lock the owner out (spec 16 §4.3).
        if let Ok(_lock) = gw.state.lock()
            && let Ok(Some(mut p)) = gw.state.pairing(&pairing.pid)
            && matches!(&p.status, PairingStatus::Claimed { claim_id: c, .. } if *c == claim_id)
        {
            p.status = PairingStatus::Pending;
            p.confirmed = None;
            p.confirmed_claim = None;
            let _ = gw.state.save_pairing(&p);
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
    };
    state.save_pairing(&pairing)?;
    let share_json = share.map(|sh| {
        serde_json::json!({"kind": sh.kind, "scope": scope.as_str(), "until": sh.until,
                           "label": sh.label, "limit": sh.limit})
    });
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
    };
    Ok((pairing, link))
}

/// The `peer` object of a host's `pair.claim`: `{host_name, user?: {name, email}}`, trimmed.
fn peer_identity(v: &Value) -> PeerInfo {
    let text = |v: Option<&Value>, max: usize| -> Option<String> {
        v.and_then(|x| x.as_str())
            .map(|x| x.trim().chars().take(max).collect::<String>())
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
        Some(k) => (k.to_string(), None),
        None => ("device".into(), None),
    }
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
                let prompt = format!(
                    "Pair \"{name}\" ({platform}) with fingerprint {fingerprint}? Check it matches the phone. [y/N] "
                );
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
