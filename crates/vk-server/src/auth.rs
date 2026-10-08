//! Pane token revocation and elevation (09 §3.2).
//!
//! - `auth.revoke_token {pane}` (full scope): the pane's token hashes are dropped and the pane
//!   is marked `token_revoked` for its current process tree (its child pid). Every call from
//!   that pane — by token or by process ancestry, on new and on already open connections — is
//!   refused with `permission_denied: token_revoked`, while its agent keeps running. Restarting
//!   the pane (a new child process with a fresh token) lifts it. Elevated tokens issued to the
//!   pane are revoked with it.
//! - `auth.elevate {reason?}` from inside a pane opens a request the user must approve outside
//!   the pane (`auth.elevate.decide`, full scope, never from a pane or an elevated
//!   connection): the TUI chrome or a CLI outside any pane. On approval the caller receives a
//!   separate 256-bit token valid for 10 minutes; presenting it in `client.hello {token}` (the
//!   CLI sends `VIBEKE_ELEVATED_TOKEN`) from that pane's process tree gives full scope until it
//!   expires. Only its hash is kept, in memory: a server restart ends every elevation.
//! - Approved calls (`auth.approve`, one specific call instead of full scope) live in
//!   `crate::approve`; revocation withdraws their requests and ends their standing grants too.

use crate::Server;
use crate::api::{Ctx, R, err, invalid, not_found, req, s, u};
use crate::core::{Tx, ulid};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_store::now_ms;

/// Lifetime of an elevated token (09 §3.2).
pub const ELEVATION_TTL_MS: i64 = 10 * 60 * 1000;
/// How long `auth.elevate` waits for a decision by default.
const DEFAULT_WAIT_MS: u64 = 120_000;
/// Prefix of `Ctx::kind` for elevated connections: `elevated:<token hash prefix>`.
pub const ELEVATED_KIND: &str = "elevated:";
/// Open requests a pane may have at once, per kind (elevation, approved calls).
pub const MAX_OPEN_PER_PANE: usize = 3;

pub struct State {
    inner: Mutex<Inner>,
    /// Bumped on every revocation: long-lived authenticated streams (event subscriptions,
    /// render sessions) re-check their caller when it changes.
    epoch: watch::Sender<u64>,
}

impl Default for State {
    fn default() -> State {
        State {
            inner: Mutex::default(),
            epoch: watch::channel(0).0,
        }
    }
}

/// A receiver that changes whenever a pane token or elevation is revoked.
pub fn revocations(server: &Server) -> watch::Receiver<u64> {
    server.security.auth.epoch.subscribe()
}

/// Wake every open connection, subscription and render stream to re-check its caller (a
/// token was revoked).
pub fn bump_epoch(server: &Server) {
    server.security.auth.epoch.send_modify(|e| *e += 1);
}

/// When this connection's elevation ends (ms since the epoch), for elevated connections.
pub fn elevation_expiry(server: &Server, kind: &str) -> Option<i64> {
    let prefix = kind.strip_prefix(ELEVATED_KIND)?;
    let g = server.security.auth.inner.lock().unwrap();
    g.elevated
        .iter()
        .filter(|(h, _)| h.starts_with(prefix))
        .map(|(_, gr)| gr.expires_at_ms)
        .max()
}

#[derive(Default)]
struct Inner {
    /// pane → child pid at revocation (the revocation holds for that process tree).
    revoked: Option<HashMap<String, Option<u32>>>,
    requests: HashMap<String, Request>,
    /// token hash → grant.
    elevated: HashMap<String, Grant>,
}

#[derive(Clone)]
struct Request {
    id: String,
    pane: String,
    reason: String,
    created_at_ms: i64,
    /// `None` while pending; `Some(Ok(token))` approved (token handed out once), `Some(Err)`
    /// denied.
    decision: watch::Sender<Option<Result<String, String>>>,
}

#[derive(Clone)]
struct Grant {
    pane: String,
    request: String,
    expires_at_ms: i64,
}

fn token_hash(t: &str) -> String {
    blake3::hash(t.as_bytes()).to_hex().to_string()
}

fn denied(msg: impl Into<String>) -> RpcError {
    err(ErrorKind::PermissionDenied, msg).details(json!({"scope": "pane"}))
}

fn load_revoked(server: &Server, g: &mut Inner) {
    if g.revoked.is_none() {
        let m: HashMap<String, Option<u32>> = server
            .with_core(|c| c.store.kv_get("server", "revoked_panes").ok().flatten())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        g.revoked = Some(m);
    }
}

/// Is `pane`'s current process tree revoked?
pub fn is_revoked(server: &Server, pane: &str) -> bool {
    let rec = {
        let mut g = server.security.auth.inner.lock().unwrap();
        load_revoked(server, &mut g);
        g.revoked.as_ref().and_then(|m| m.get(pane).cloned())
    };
    let Some(pid_at_revoke) = rec else {
        return false;
    };
    // A restarted pane (new child process) is no longer revoked.
    let now = server.with_core(|c| c.pane(pane).map(|p| p.child_pid));
    match now {
        Some(cur) => cur == pid_at_revoke,
        None => true,
    }
}

/// Is this elevated connection's grant still valid?
fn elevated_valid(server: &Server, kind: &str) -> bool {
    let Some(prefix) = kind.strip_prefix(ELEVATED_KIND) else {
        return true;
    };
    let now = now_ms();
    let g = server.security.auth.inner.lock().unwrap();
    g.elevated
        .iter()
        .any(|(h, gr)| h.starts_with(prefix) && gr.expires_at_ms > now)
}

/// Per-call check (09 §3.2), before dispatch: revoked panes and expired elevations get
/// nothing, and an elevated connection can't decide elevation requests.
pub fn authorize(server: &Server, ctx: &Ctx, method: &str) -> Result<(), RpcError> {
    crate::plugin_native::authorize_live(server, ctx)?;
    if let Some(p) = &ctx.pane_scope
        && is_revoked(server, p)
    {
        return Err(denied(format!(
            "token_revoked: pane {p}'s API access was revoked; restart the pane to restore it"
        )));
    }
    if ctx.kind.starts_with(ELEVATED_KIND) {
        if !elevated_valid(server, &ctx.kind) {
            return Err(err(
                ErrorKind::PermissionDenied,
                "elevation_expired: the elevated token expired or was revoked",
            ));
        }
        if matches!(method, "auth.elevate.decide" | "auth.approve.decide") {
            return Err(err(
                ErrorKind::PermissionDenied,
                "an elevated connection cannot decide elevation or approval requests",
            ));
        }
    }
    Ok(())
}

/// The `client.hello` refusal for a token the server doesn't know: says `token_revoked` when
/// the caller's pane (by ancestry) was revoked.
pub fn unknown_token_message(server: &Server, ancestry: Option<&str>) -> String {
    match ancestry {
        Some(p) if is_revoked(server, p) => format!(
            "token_revoked: pane {p}'s API access was revoked; restart the pane to restore it"
        ),
        _ => "unknown pane token".into(),
    }
}

/// `client.hello {token}` with an elevated token: the connection kind to use (full scope),
/// when the token is valid and the caller is in the pane it was issued to (or outside every
/// pane).
pub fn elevated_hello(server: &Server, token: &str, ancestry: Option<&str>) -> Option<String> {
    let h = token_hash(token);
    let g = server.security.auth.inner.lock().unwrap();
    let gr = g.elevated.get(&h)?;
    if gr.expires_at_ms <= now_ms() || ancestry.is_some_and(|a| a != gr.pane) {
        return None;
    }
    Some(format!("{ELEVATED_KIND}{}", &h[..16]))
}

fn revoke(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let pane = crate::api::resolve_pane(server, ctx, Some(req(p, "pane")?))?;
    let removed = {
        let mut t = server.tokens.lock().unwrap();
        let before = t.len();
        t.retain(|_, v| v != &pane.id);
        before - t.len()
    };
    let elevated_removed = {
        let mut g = server.security.auth.inner.lock().unwrap();
        load_revoked(server, &mut g);
        if let Some(m) = g.revoked.as_mut() {
            m.insert(pane.id.clone(), pane.child_pid);
        }
        let before = g.elevated.len();
        g.elevated.retain(|_, gr| gr.pane != pane.id);
        before - g.elevated.len()
    };
    // Its approval requests are withdrawn and its standing grants end.
    let (approvals_withdrawn, grants_removed) =
        crate::approve::clear_pane(server, &pane.id, crate::audit::actor_of(ctx));
    let revoked_json = {
        let g = server.security.auth.inner.lock().unwrap();
        serde_json::to_string(g.revoked.as_ref().unwrap_or(&HashMap::new())).unwrap_or_default()
    };
    // Open subscriptions and render sessions of the pane (or its elevations) end now.
    server.security.auth.epoch.send_modify(|e| *e += 1);
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        server.persist_tokens(&mut tx);
        tx.m.kv("server", "revoked_panes", Some(revoked_json));
        tx.event_by(
            "auth.token_revoked",
            crate::core::subject_pane(&pane),
            crate::audit::actor_of(ctx),
            json!({"tokens_removed": removed, "elevations_removed": elevated_removed}),
        );
        server.commit(&mut c, tx).map_err(crate::api::internal)?;
    }
    crate::audit::record(
        server,
        "auth.token_revoked",
        crate::audit::actor_of(ctx),
        json!({"pane": pane.id}),
        json!({"tokens_removed": removed, "elevations_removed": elevated_removed,
               "approvals_withdrawn": approvals_withdrawn, "grants_removed": grants_removed}),
    );
    Ok(
        json!({"pane": pane.id, "revoked": true, "tokens_removed": removed, "elevations_removed": elevated_removed}),
    )
}

fn request_json(r: &Request) -> Value {
    let status = match &*r.decision.borrow() {
        None => "pending",
        Some(Ok(_)) => "approved",
        Some(Err(_)) => "denied",
    };
    json!({"request": r.id, "pane": r.pane, "reason": r.reason, "created_at_ms": r.created_at_ms, "status": status})
}

async fn elevate(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let Some(pane) = ctx.pane_scope.clone() else {
        return Err(invalid(
            "auth.elevate is for callers inside a pane; this connection already has full scope",
        ));
    };
    prune(server);
    let wait_ms = u(p, "timeout_ms").unwrap_or(DEFAULT_WAIT_MS).min(600_000);
    // Resume waiting on an earlier request of this pane, or open a new one.
    let (id, mut rx) = {
        let mut g = server.security.auth.inner.lock().unwrap();
        match s(p, "request") {
            Some(id) => {
                let r = g
                    .requests
                    .get(id)
                    .filter(|r| r.pane == pane)
                    .ok_or_else(|| not_found("elevation request", id))?;
                (r.id.clone(), r.decision.subscribe())
            }
            None => {
                if g.requests.values().filter(|r| r.pane == pane).count() >= MAX_OPEN_PER_PANE {
                    return Err(err(
                        ErrorKind::RateLimited,
                        "too many open elevation requests from this pane",
                    ));
                }
                let reason: String = s(p, "reason").unwrap_or("").chars().take(500).collect();
                let (tx, rx) = watch::channel(None);
                let r = Request {
                    id: format!("el-{}", &ulid()[16..]),
                    pane: pane.clone(),
                    reason,
                    created_at_ms: now_ms(),
                    decision: tx,
                };
                let id = r.id.clone();
                g.requests.insert(id.clone(), r);
                (id, rx)
            }
        }
    };
    if s(p, "request").is_none() {
        let (reason, title) = {
            let g = server.security.auth.inner.lock().unwrap();
            let r = &g.requests[&id];
            (
                r.reason.clone(),
                format!("Pane {pane} asks for elevated access"),
            )
        };
        {
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.event_by(
                "auth.elevate_requested",
                json!({"pane": pane, "request": id}),
                crate::audit::actor_of(ctx),
                json!({"reason": vk_redact::redact(&reason)}),
            );
            let _ = server.commit(&mut c, tx);
        }
        crate::audit::record(
            server,
            "auth.elevate_requested",
            crate::audit::actor_of(ctx),
            json!({"pane": pane, "request": id}),
            json!({"reason": reason}),
        );
        let body = format!(
            "{}Approve outside the pane: vibeke auth approve {id} (or deny {id}). Grants full API access for 10 minutes.",
            if reason.is_empty() {
                String::new()
            } else {
                format!("{reason}\n")
            }
        );
        server.notify("auth.elevate", Some(&pane), &title, &body, "high");
    }
    if b_false(p, "wait") {
        let g = server.security.auth.inner.lock().unwrap();
        return Ok(request_json(&g.requests[&id]));
    }
    let decided = tokio::time::timeout(Duration::from_millis(wait_ms), async {
        loop {
            if let Some(d) = rx.borrow_and_update().clone() {
                return d;
            }
            if rx.changed().await.is_err() {
                return Err("request withdrawn".to_string());
            }
        }
    })
    .await;
    match decided {
        Err(_) => Err(err(
            ErrorKind::Timeout,
            format!("no decision on elevation request {id} yet; call auth.elevate {{request: \"{id}\"}} to keep waiting"),
        )
        .details(json!({"request": id}))),
        Ok(Err(why)) => {
            server.security.auth.inner.lock().unwrap().requests.remove(&id);
            Err(denied(format!("elevation_denied: {why}")))
        }
        Ok(Ok(token)) => {
            // Hand the token out once; the request is done.
            let exp = {
                let mut g = server.security.auth.inner.lock().unwrap();
                g.requests.remove(&id);
                g.elevated
                    .get(&token_hash(&token))
                    .map(|gr| gr.expires_at_ms)
                    .unwrap_or(0)
            };
            Ok(json!({"request": id, "token": token, "expires_at_ms": exp, "ttl_s": ELEVATION_TTL_MS / 1000, "env": "VIBEKE_ELEVATED_TOKEN"}))
        }
    }
}

fn b_false(p: &Value, k: &str) -> bool {
    p.get(k).and_then(Value::as_bool) == Some(false)
}

fn decide(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "request")?;
    let approve = match s(p, "decision").unwrap_or("") {
        "approve" | "allow" | "approved" => true,
        "deny" | "denied" | "reject" => false,
        _ => return Err(invalid("decision must be approve or deny")),
    };
    let (pane, expires) = {
        let mut g = server.security.auth.inner.lock().unwrap();
        let r = g
            .requests
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("elevation request", id))?;
        if r.decision.borrow().is_some() {
            return Err(err(
                ErrorKind::Conflict,
                "elevation request already decided",
            ));
        }
        if approve {
            let token: String = (0..32)
                .map(|_| format!("{:02x}", rand::random::<u8>()))
                .collect();
            let token = format!("vke_{token}");
            let exp = now_ms() + ELEVATION_TTL_MS;
            g.elevated.insert(
                token_hash(&token),
                Grant {
                    pane: r.pane.clone(),
                    request: r.id.clone(),
                    expires_at_ms: exp,
                },
            );
            // `send_replace` stores the decision even when nobody is waiting right now.
            r.decision.send_replace(Some(Ok(token)));
            (r.pane, Some(exp))
        } else {
            r.decision
                .send_replace(Some(Err("denied by the user".into())));
            (r.pane, None)
        }
    };
    let kind = if approve {
        "auth.elevate_granted"
    } else {
        "auth.elevate_denied"
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event_by(
            kind,
            json!({"pane": pane, "request": id}),
            crate::audit::actor_of(ctx),
            json!({"expires_at_ms": expires}),
        );
        let _ = server.commit(&mut c, tx);
    }
    crate::audit::record(
        server,
        kind,
        crate::audit::actor_of(ctx),
        json!({"pane": pane, "request": id}),
        json!({"expires_at_ms": expires}),
    );
    Ok(
        json!({"request": id, "pane": pane, "decision": if approve { "approved" } else { "denied" }, "expires_at_ms": expires}),
    )
}

/// Tests: let every elevation run out now (no clock to advance).
#[cfg(test)]
pub fn expire_all_for_test(server: &Server) {
    let mut g = server.security.auth.inner.lock().unwrap();
    for gr in g.elevated.values_mut() {
        gr.expires_at_ms = now_ms() - 1;
    }
}

/// Forget expired grants and requests nobody collected within 30 minutes.
fn prune(server: &Server) {
    let now = now_ms();
    let mut g = server.security.auth.inner.lock().unwrap();
    g.elevated.retain(|_, gr| gr.expires_at_ms > now);
    g.requests
        .retain(|_, r| now - r.created_at_ms < 30 * 60 * 1000);
}

fn list(server: &Server) -> R {
    prune(server);
    let (approvals, grants) = crate::approve::list_json(server);
    let now = now_ms();
    let revoked: Vec<String> = {
        let mut g = server.security.auth.inner.lock().unwrap();
        load_revoked(server, &mut g);
        g.revoked
            .as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    let revoked: Vec<Value> = revoked
        .into_iter()
        .filter(|p| is_revoked(server, p))
        .map(|p| json!({"pane": p}))
        .collect();
    let g = server.security.auth.inner.lock().unwrap();
    let mut pending: Vec<Value> = g
        .requests
        .values()
        .filter(|r| r.decision.borrow().is_none())
        .map(request_json)
        .collect();
    pending.sort_by_key(|v| v["created_at_ms"].as_i64());
    let elevated: Vec<Value> = g
        .elevated
        .values()
        .filter(|gr| gr.expires_at_ms > now)
        .map(
            |gr| json!({"pane": gr.pane, "request": gr.request, "expires_at_ms": gr.expires_at_ms}),
        )
        .collect();
    Ok(
        json!({"pending": pending, "elevated": elevated, "revoked": revoked, "approvals": approvals, "grants": grants}),
    )
}

pub async fn api(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "auth.revoke_token" => revoke(server, ctx, p),
        "auth.elevate" => elevate(server, ctx, p).await,
        "auth.elevate.decide" => decide(server, ctx, p),
        "auth.list" => list(server),
        _ => return None,
    })
}
