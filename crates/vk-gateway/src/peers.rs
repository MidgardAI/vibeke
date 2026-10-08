//! Host-to-host trust (spec 16 §15.3) and invitation management (§15.4).
//!
//! - Destination side: `peer.invite` makes a bearer pairing of kind `peer` (the owner's own
//!   hosts, no expiry); `share.list` / `share.revoke` show and cancel pending invitations and the
//!   share, handoff and peer devices they produced.
//! - Source side: `peer.redeem` pairs this host with another gateway through [`PeerClient`] and
//!   keeps the result in `peers.json`; `peer.list` / `peer.remove` manage those records.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use vk_e2e::PairingLink;

use crate::Gateway;
use crate::api::{ApiError, ApiResult, internal};
use crate::peer_client::{Identity, PeerClient};
use crate::state::{
    Device, GitUser, Pairing, PairingStatus, PeerRecord, Scope, ShareSpec, StateDir, now_s,
};

/// How long a `peer.invite` link stays open by default.
pub const INVITE_TTL: Duration = Duration::from_secs(15 * 60);

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

/// Create a `peer` invitation on this host: whoever redeems it becomes one of the owner's own
/// hosts (kind `peer`, owner `self`, no expiry). Returns the pairing and its link; the link's
/// relay is `local:<socket>` when no relay is configured (both gateways on this machine).
pub fn invite(
    state: &StateDir,
    relay: Option<&str>,
    host_name: &str,
    ttl: Duration,
) -> Result<(Pairing, PairingLink)> {
    let spec = ShareSpec {
        kind: "peer".into(),
        ttl_s: 0,
        until: 0,
        limit: None,
        label: None,
        owner: Some("self".into()),
    };
    let (p, mut link) = crate::pair::create_with(
        state,
        relay.unwrap_or("local"),
        host_name,
        Scope::Full,
        true,
        ttl,
        Some(spec),
    )?;
    if relay.is_none() {
        link.relay = format!("local:{}", crate::local::socket_path(&state.dir).display());
    }
    Ok((p, link))
}

/// The link as text: the app URL when this host has one, else the bare `d` value (both parse).
pub fn link_text(link: &PairingLink, app_url: Option<&str>) -> String {
    match app_url {
        Some(app) => link.to_url(app),
        None => vk_e2e::b64::encode(serde_json::to_vec(link).expect("link serializes")),
    }
}

/// `git config --global user.name/email`, when set.
pub async fn git_user() -> Option<GitUser> {
    async fn get(key: &str) -> Option<String> {
        let out = tokio::process::Command::new("git")
            .args(["config", "--global", "--get", key])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !v.is_empty()).then_some(v)
    }
    let user = GitUser {
        name: get("user.name").await,
        email: get("user.email").await,
    };
    (user.name.is_some() || user.email.is_some()).then_some(user)
}

/// Redeem a peer or handoff invitation as this host and remember the peer. A second record for
/// the same host replaces the first (re-pairing). The invitation's relay must be this host's own
/// unless `allow_other_relay` (see [`crate::peer_client::check_invitation_relay`]).
pub async fn redeem(
    state: &StateDir,
    our_host_id: &str,
    us: &Identity,
    link: &str,
    allow_other_relay: bool,
) -> Result<PeerRecord> {
    let link = PairingLink::parse(link.trim()).context("not a Vibeke invitation link")?;
    if link.host == our_host_id {
        bail!("this invitation is for this host itself");
    }
    let configured = state.config()?.relay;
    crate::peer_client::check_invitation_relay(
        &link.relay,
        configured.as_deref(),
        allow_other_relay,
    )?;
    let rec = PeerClient::pair(&link, us).await?;
    let _lock = state.lock()?;
    let mut all = state.peers()?;
    all.retain(|p| p.host != rec.host);
    all.push(rec.clone());
    state.save_peers(&all)?;
    state.audit(&json!({"ts": now_s(), "event": "peer.added", "peer": rec.id, "host": rec.host, "name": rec.name, "owner": rec.owner}));
    Ok(rec)
}

/// Forget a peer by id or name. Returns the ids removed.
pub fn remove(state: &StateDir, id_or_name: &str) -> Result<Vec<String>> {
    let _lock = state.lock()?;
    let mut all = state.peers()?;
    let gone: Vec<String> = all
        .iter()
        .filter(|p| p.id == id_or_name || p.name == id_or_name)
        .map(|p| p.id.clone())
        .collect();
    if gone.is_empty() {
        bail!("no peer {id_or_name}");
    }
    all.retain(|p| !gone.contains(&p.id));
    state.save_peers(&all)?;
    for id in &gone {
        state.audit(&json!({"ts": now_s(), "event": "peer.removed", "peer": id}));
    }
    Ok(gone)
}

/// One pending invitation for listings.
pub fn invitation_json(p: &Pairing) -> Value {
    let sh = p.share.as_ref();
    json!({
        "id": p.pid,
        "kind": sh.map_or("device", |s| s.kind.as_str()),
        "scope": p.scope,
        "label": sh.and_then(|s| s.label.clone()),
        "limit": sh.and_then(|s| s.limit.clone()),
        "created": (p.created_at > 0).then_some(p.created_at),
        "link_expires_at": p.exp,
        "device_expires_at": sh.and_then(|s| match (s.until, s.ttl_s) {
            (0, 0) => None,
            (0, ttl) => Some(p.created_at.max(now_s()) + ttl),
            (until, _) => Some(until),
        }),
    })
}

/// One share, handoff or peer device for listings.
pub fn device_json(d: &Device) -> Value {
    json!({
        "id": d.id,
        "kind": d.kind,
        "name": d.name,
        "scope": d.scope,
        "paired_at": d.paired_at,
        "owner": d.peer.as_ref().map(|p| p.owner.clone()),
        "sender": d.peer.as_ref().map(|p| json!({"host_name": p.host_name, "user": p.user})),
        "expires_at": d.expires_at,
        "limit": d.limit,
    })
}

/// Pending (unclaimed, unexpired) invitations, oldest first.
pub fn pending(state: &StateDir) -> Vec<Pairing> {
    let now = now_s();
    state
        .pairings()
        .into_iter()
        .filter(|p| {
            p.exp > now
                && !matches!(
                    p.status,
                    PairingStatus::Done { .. } | PairingStatus::Rejected
                )
        })
        .collect()
}

/// Devices that came from invitations (everything but the owner's own `device`s).
pub fn invited_devices(devices: &[Device]) -> Vec<&Device> {
    devices
        .iter()
        .filter(|d| d.kind != "device" && !d.expired())
        .collect()
}

/// `share.list`: pending invitations plus the devices invitations produced.
pub fn share_list(state: &StateDir, devices: &[Device]) -> Value {
    json!({
        "invitations": pending(state).iter().map(invitation_json).collect::<Vec<_>>(),
        "devices": invited_devices(devices).into_iter().map(device_json).collect::<Vec<_>>(),
    })
}

/// Cancel a pending invitation by pairing id. `Ok(false)` when there is none (or it was used).
pub fn cancel_invitation(state: &StateDir, pid: &str, by: &str) -> Result<bool> {
    let _lock = state.lock()?;
    let Ok(Some(p)) = state.pairing(pid) else {
        return Ok(false);
    };
    if matches!(p.status, PairingStatus::Done { .. }) {
        return Ok(false);
    }
    state.remove_pairing(pid)?;
    state.audit(
        &json!({"ts": now_s(), "event": "invitation.cancelled", "pid": pid, "by": by,
                        "kind": p.share.as_ref().map(|s| s.kind.clone())}),
    );
    Ok(true)
}

/// The API methods (Full scope, own devices only; `kind_allows` keeps them from every other
/// kind).
pub async fn dispatch(gw: &Arc<Gateway>, device: &Device, method: &str, p: &Value) -> ApiResult {
    match method {
        "peer.invite" => {
            let ttl = p
                .get("ttl_s")
                .and_then(|v| v.as_u64())
                .map_or(INVITE_TTL, |t| Duration::from_secs(t.clamp(60, 3600)));
            let (pairing, link) =
                invite(&gw.state, gw.cfg.relay.as_deref(), &gw.host_name, ttl).map_err(internal)?;
            gw.state.audit(&json!({"ts": now_s(), "event": "share.created", "by": device.id, "kind": "peer", "pid": pairing.pid}));
            Ok(
                json!({"link": link_text(&link, gw.cfg.app_url.as_deref()), "pid": pairing.pid, "open_by": pairing.exp}),
            )
        }
        "peer.redeem" => {
            let link = s(p, "link")
                .filter(|l| !l.trim().is_empty())
                .ok_or_else(|| ApiError::invalid("link is required"))?;
            let user = if p.get("share_user").and_then(|v| v.as_bool()) == Some(true) {
                git_user().await
            } else {
                None
            };
            let us = Identity {
                host_name: gw.host_name.clone(),
                user,
            };
            let allow_other = p.get("allow_other_relay").and_then(|v| v.as_bool()) == Some(true);
            let rec = redeem(&gw.state, &gw.keys.host_id(), &us, link, allow_other)
                .await
                .map_err(|e| {
                    let m = format!("{e:#}");
                    if m.contains("not a peer")
                        || m.contains("not a Vibeke")
                        || m.contains("itself")
                        || m.contains("invitation relay")
                        || m.contains("invitation uses relay")
                        || m.contains("invitation points at")
                        || m.contains("invalid relay address")
                    {
                        ApiError::invalid(m)
                    } else if m.contains("expired")
                        || m.contains("unauthorized")
                        || m.contains("not available")
                    {
                        ApiError::new("forbidden", m)
                    } else {
                        ApiError::unavailable(m)
                    }
                })?;
            crate::handoff_send::publish_peers(gw, false).await;
            Ok(json!({"peer": rec.public_json()}))
        }
        "peer.list" => {
            let peers = gw.state.peers().map_err(internal)?;
            Ok(json!({"peers": peers.iter().map(PeerRecord::public_json).collect::<Vec<_>>()}))
        }
        "peer.remove" => {
            let id = s(p, "id")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| ApiError::invalid("id is required"))?;
            remove(&gw.state, id).map_err(|e| ApiError::new("not_found", e.to_string()))?;
            crate::handoff_send::publish_peers(gw, false).await;
            Ok(json!({}))
        }
        "share.list" => {
            let _ = gw.reload_devices();
            Ok(share_list(&gw.state, &gw.devices()))
        }
        "share.revoke" => {
            let id = s(p, "id")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| ApiError::invalid("id is required"))?;
            if cancel_invitation(&gw.state, id, &device.id).map_err(internal)? {
                return Ok(json!({"cancelled": "invitation"}));
            }
            match gw.device(id) {
                Some(d) if d.kind != "device" => {
                    gw.revoke(&d.id).await.map_err(internal)?;
                    Ok(json!({"cancelled": "device"}))
                }
                _ => Err(ApiError::new(
                    "not_found",
                    "no pending invitation or invited device with that id",
                )),
            }
        }
        _ => Err(ApiError::new(
            "method_not_found",
            format!("unknown method {method}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pair::device_kind_for;
    use crate::state::PeerInfo;

    fn spec(kind: &str, owner: Option<&str>) -> ShareSpec {
        ShareSpec {
            kind: kind.into(),
            ttl_s: 3600,
            until: now_s() + 3600,
            limit: None,
            label: None,
            owner: owner.map(str::to_string),
        }
    }

    #[test]
    fn claim_kinds() {
        let sender = PeerInfo {
            owner: String::new(),
            host_name: Some("laptop".into()),
            user: None,
        };
        let (k, info) = device_kind_for(None, None);
        assert_eq!(k, "device");
        assert!(info.is_none());
        let (k, info) = device_kind_for(Some(&spec("peer", Some("self"))), Some(sender.clone()));
        assert_eq!(k, "peer");
        let info = info.unwrap();
        assert_eq!(info.owner, "self");
        assert_eq!(info.host_name.as_deref(), Some("laptop"));
        // A handoff invitation redeemed by a host makes a teammate peer (an app cannot claim one).
        let (k, info) = device_kind_for(Some(&spec("handoff", None)), Some(sender));
        assert_eq!(
            (k.as_str(), info.unwrap().owner.as_str()),
            ("peer", "teammate")
        );
        let (k, info) = device_kind_for(Some(&spec("share", None)), None);
        assert_eq!(k, "share");
        assert!(info.is_none());
    }

    #[test]
    fn invitations_list_and_cancel() {
        let t = tempfile::tempdir().unwrap();
        let st = StateDir::open(t.path().join("gw")).unwrap();
        let (p, link) = invite(&st, Some("wss://relay.example"), "devbox", INVITE_TTL).unwrap();
        assert_eq!(link.share.as_ref().unwrap()["kind"], "peer");
        assert_eq!(link.relay, "wss://relay.example");
        let (_, local) = invite(&st, None, "devbox", INVITE_TTL).unwrap();
        assert!(local.relay.starts_with("local:/"), "{}", local.relay);
        let list = share_list(&st, &[]);
        let inv = list["invitations"].as_array().unwrap();
        assert_eq!(inv.len(), 2);
        let mine = inv.iter().find(|i| i["id"] == p.pid.as_str()).unwrap();
        assert_eq!(mine["kind"], "peer");
        assert_eq!(mine["scope"], "full");
        assert!(
            mine["device_expires_at"].is_null(),
            "own hosts never expire"
        );
        assert_eq!(mine["link_expires_at"], p.exp);
        assert!(cancel_invitation(&st, &p.pid, "test").unwrap());
        assert!(!cancel_invitation(&st, &p.pid, "test").unwrap());
        assert_eq!(
            share_list(&st, &[])["invitations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let audit = std::fs::read_to_string(t.path().join("gw/audit.log")).unwrap();
        assert!(audit.contains("invitation.cancelled"));
        // A link parses back from either text form.
        assert_eq!(PairingLink::parse(&link_text(&link, None)).unwrap(), link);
        assert_eq!(
            PairingLink::parse(&link_text(&link, Some("https://app.example"))).unwrap(),
            link
        );
    }

    #[test]
    fn peer_records_remove_by_id_or_name() {
        let t = tempfile::tempdir().unwrap();
        let st = StateDir::open(t.path().join("gw")).unwrap();
        let rec = |id: &str, name: &str| PeerRecord {
            id: id.into(),
            name: name.into(),
            relay: "wss://r".into(),
            host: format!("h-{id}"),
            host_key: "hk".into(),
            device_key: "k".into(),
            device_id: "d".into(),
            owner: "self".into(),
            added_at: 0,
            expires_at: None,
        };
        st.save_peers(&[rec("a", "devbox"), rec("b", "laptop")])
            .unwrap();
        assert_eq!(remove(&st, "laptop").unwrap(), ["b"]);
        assert!(remove(&st, "nope").is_err());
        assert_eq!(remove(&st, "a").unwrap(), ["a"]);
        assert!(st.peers().unwrap().is_empty());
    }
}
