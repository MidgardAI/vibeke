//! The server-to-gateway bridge (spec 16 §15.5). `peer.*`, `share.*`, the device registry and
//! pairing live in the host's gateway, which a TUI attached to a remote machine cannot reach. One
//! small generic bridge carries them instead of one method per operation:
//!
//! - `gateway.call {method, params?, timeout_ms? = 30000}` (full scope, never from a pane) keeps
//!   a pending request, emits `gateway.request {id, method, params, client}` to the gateway and
//!   waits for the answer. Only [`ALLOWED`] methods go through, and `share.create` only for the
//!   kinds `handoff`, `peer` and `share` (a pane or workspace share for a colleague; the TUI's
//!   People tab). The TUI's Devices view uses `devices.list` / `devices.revoke`
//!   (the app API's, as the owner) and the bridge-only `pair.create {scope?, ttl_s?}` =>
//!   `{link, pid, open_by, scope}` and `pair.status {pid}` => `{status: pending | claimed |
//!   done | rejected | gone, …}` (see `vk_gateway::bridge`); `share.revoke {id: pid}` cancels a
//!   pairing. The TUI signs the host in to an account relay with the bridge-only
//!   `account.status`, `account.login.start` / `.status` / `.cancel` and `account.logout`; a
//!   pane may never reach the login methods ([`PANE_FORBIDDEN_CALLS`]).
//! - `gateway.reply {id, result | error: {kind, message, details?}}` is the gateway's answer
//!   (gateway clients only, like `handoff.job.update`).
//! - `gateway.status {}` says whether a gateway is connected, plus the supervised gateway's
//!   status (`crate::gateway_supervisor`).
//!
//! `gateway.request` is a transient event (seq 0, never stored or replayed). It is delivered
//! only to the one gateway event subscription the request was addressed to, because its
//! params may hold an invitation link. A gateway is "connected" while it holds an
//! `events.subscribe` stream open (see [`enter`]).
//!
//! Without a connected gateway, on a timeout, or when the gateway goes away first, the call
//! fails with `remote_unavailable`: "the gateway isn't running: start it with `vibeke
//! gateway on`".

use crate::Server;
use crate::api::{Ctx, R, err, invalid, req, s};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use vk_proto::rpc::{ErrorKind, RpcError};

pub const METHODS: &[(&str, bool)] = &[
    ("gateway.call", true),
    ("gateway.reply", true),
    ("gateway.status", false),
];

/// Calling the gateway and answering for it are the user's and the gateway's, never a pane's.
pub const PANE_FORBIDDEN: &[&str] = &["gateway.call", "gateway.reply"];

/// Bridged methods refused to a pane-scoped caller even if `gateway.call` reached it: signing
/// the host in or out of its relay account and handing out access (`share.create`, any kind)
/// are the user's. (`gateway.call` itself is in
/// [`PANE_FORBIDDEN`], and `auth.approve` carries only `approve::APPROVABLE_GATEWAY`.)
pub const PANE_FORBIDDEN_CALLS: &[&str] = &[
    "share.create",
    "account.login.start",
    "account.login.status",
    "account.login.cancel",
    "account.logout",
];

/// What `gateway.call` may run. The gateway checks this list again (`vk_gateway::bridge::ALLOWED`
/// must stay identical; both crates test against the same literal).
pub const ALLOWED: &[&str] = &[
    "peer.invite",
    "peer.redeem",
    "peer.list",
    "peer.remove",
    "share.create",
    "share.list",
    "share.revoke",
    "devices.list",
    "devices.revoke",
    "pair.create",
    "pair.status",
    "account.status",
    "account.login.start",
    "account.login.status",
    "account.login.cancel",
    "account.logout",
];

/// The `share.create` kinds the bridge lets through.
pub const SHARE_KINDS: &[&str] = &["handoff", "peer", "share"];

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_PENDING: usize = 64;

pub const NOT_RUNNING: &str = "the gateway isn't running: start it with `vibeke gateway on`";

type Answer = Result<Value, RpcError>;

struct Pending {
    /// The gateway event subscription's client the request went to.
    client: String,
    tx: oneshot::Sender<Answer>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    /// Live gateway event subscriptions: (token, client id), oldest first.
    gateways: Vec<(u64, String)>,
    pending: HashMap<String, Pending>,
}

#[derive(Default)]
pub struct Bridge {
    inner: Mutex<Inner>,
}

fn bridge(server: &Server) -> &Bridge {
    &server.gateway.bridge
}

fn unavailable(msg: &str) -> RpcError {
    err(ErrorKind::RemoteUnavailable, msg)
}

/// Whether `ctx` is the host's gateway (not a pane that claims to be one).
fn is_gateway(ctx: &Ctx) -> bool {
    ctx.kind == "gateway" && ctx.pane_scope.is_none()
}

fn gateway_only(ctx: &Ctx, method: &str) -> Result<(), RpcError> {
    if is_gateway(ctx) {
        Ok(())
    } else {
        Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is sent by the host's gateway"),
        ))
    }
}

/// A live gateway event subscription. Dropping it (the stream ended, its connection closed or
/// the task was aborted) removes the gateway and fails the requests addressed to it.
pub struct Presence {
    server: Arc<Server>,
    token: u64,
    client: String,
}

/// Register `ctx`'s event subscription as a gateway, when it is one.
pub fn enter(server: &Arc<Server>, ctx: &Ctx) -> Option<Presence> {
    if !is_gateway(ctx) {
        return None;
    }
    let mut g = bridge(server).inner.lock().unwrap();
    g.next += 1;
    let token = g.next;
    g.gateways.push((token, ctx.client_id.clone()));
    Some(Presence {
        server: server.clone(),
        token,
        client: ctx.client_id.clone(),
    })
}

impl Drop for Presence {
    fn drop(&mut self) {
        let mut g = bridge(&self.server).inner.lock().unwrap();
        g.gateways.retain(|(t, _)| *t != self.token);
        if g.gateways.iter().any(|(_, c)| *c == self.client) {
            return;
        }
        let gone: Vec<String> = g
            .pending
            .iter()
            .filter(|(_, p)| p.client == self.client)
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            if let Some(p) = g.pending.remove(&id) {
                let _ = p.tx.send(Err(unavailable(NOT_RUNNING)));
            }
        }
    }
}

/// Whether a live `gateway.request` event goes to the subscription of `ctx`.
pub fn deliver_to(ev: &vk_store::Event, ctx: &Ctx) -> bool {
    is_gateway(ctx) && ev.data.get("client").and_then(Value::as_str) == Some(ctx.client_id.as_str())
}

/// Whether a gateway is connected.
pub fn connected(server: &Server) -> bool {
    !bridge(server).inner.lock().unwrap().gateways.is_empty()
}

/// Refuse what the bridge may not carry.
pub fn check_allowed(method: &str, params: &Value) -> Result<(), RpcError> {
    if !ALLOWED.contains(&method) {
        return Err(invalid(format!(
            "gateway.call does not carry {method} (allowed: {})",
            ALLOWED.join(", ")
        )));
    }
    if !params.is_object() {
        return Err(invalid("params must be an object"));
    }
    if method == "share.create" && !s(params, "kind").is_some_and(|k| SHARE_KINDS.contains(&k)) {
        return Err(invalid(format!(
            "gateway.call carries share.create for kind {} only",
            SHARE_KINDS.join(", ")
        )));
    }
    Ok(())
}

/// Removes a pending request however the call ends (answer, timeout, cancelled caller).
struct Cleanup<'a> {
    server: &'a Server,
    id: &'a str,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        bridge(self.server)
            .inner
            .lock()
            .unwrap()
            .pending
            .remove(self.id);
    }
}

/// Run `method` in the host's gateway and return its result: the allow-list check and the round
/// trip. Used by `gateway.call` and by other server code (the `auth.approve` pairing step calls
/// `peer.redeem` through it). Errors the gateway answered with keep their kind where the server
/// has one.
pub async fn call(
    server: &Arc<Server>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, RpcError> {
    check_allowed(method, &params)?;
    let timeout = timeout.clamp(
        Duration::from_millis(MIN_TIMEOUT_MS),
        Duration::from_millis(MAX_TIMEOUT_MS),
    );
    let (tx, rx) = oneshot::channel();
    let id = {
        let mut g = bridge(server).inner.lock().unwrap();
        let Some((_, client)) = g.gateways.first().cloned() else {
            return Err(unavailable(NOT_RUNNING));
        };
        if g.pending.len() >= MAX_PENDING {
            return Err(err(
                ErrorKind::RateLimited,
                "too many gateway requests are waiting",
            ));
        }
        g.next += 1;
        let id = format!("gr{}-{}", g.next, &crate::core::ulid()[16..]);
        g.pending.insert(
            id.clone(),
            Pending {
                client: client.clone(),
                tx,
            },
        );
        let ev = vk_store::Event {
            seq: 0,
            ts: vk_store::now_ms(),
            v: 1,
            tier: "transient".into(),
            kind: "gateway.request".into(),
            subject: json!({"gateway_request": id}),
            actor: json!({"kind": "system"}),
            data: json!({"id": id, "method": method, "params": params, "client": client}),
        };
        // Sent under the lock: a subscription that ends right now fails this request.
        let _ = server.events.send(Arc::new(ev));
        id
    };
    let _cleanup = Cleanup { server, id: &id };
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => Err(unavailable(NOT_RUNNING)),
        Err(_) => Err(unavailable(NOT_RUNNING)
            .details(json!({"reason": "timeout", "timeout_ms": timeout.as_millis() as u64}))),
    }
}

fn kind_of(k: &str) -> Option<ErrorKind> {
    Some(match k {
        "invalid_params" => ErrorKind::InvalidParams,
        "forbidden" | "permission_denied" => ErrorKind::PermissionDenied,
        "not_found" => ErrorKind::NotFound,
        "conflict" | "stale" => ErrorKind::Conflict,
        "unavailable" | "remote_unavailable" => ErrorKind::RemoteUnavailable,
        "method_not_found" => ErrorKind::MethodNotFound,
        "timeout" => ErrorKind::Timeout,
        "rate_limited" => ErrorKind::RateLimited,
        _ => return None,
    })
}

fn gateway_error(e: &Value) -> RpcError {
    let kind = s(e, "kind").unwrap_or("internal");
    let msg = s(e, "message").unwrap_or("the gateway refused the request");
    let details = e.get("details").cloned().unwrap_or(Value::Null);
    match kind_of(kind) {
        Some(k) => err(k, msg).details(details),
        None => {
            err(ErrorKind::Internal, msg).details(json!({"gateway_kind": kind, "details": details}))
        }
    }
}

fn reply(server: &Server, p: &Value) -> R {
    let id = req(p, "id")?;
    let answer: Answer = match (p.get("result"), p.get("error")) {
        (Some(r), None) => Ok(r.clone()),
        (None, Some(e)) if e.is_object() => Err(gateway_error(e)),
        _ => return Err(invalid("pass either `result` or `error` (an object)")),
    };
    let pending = bridge(server).inner.lock().unwrap().pending.remove(id);
    match pending {
        Some(p) => {
            let _ = p.tx.send(answer);
            Ok(json!({}))
        }
        None => Err(err(
            ErrorKind::NotFound,
            format!("no gateway request is waiting for {id} (it timed out?)"),
        )),
    }
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "gateway.call" => {
            let m = match req(p, "method") {
                Ok(m) => m,
                Err(e) => return Some(Err(e)),
            };
            if ctx.pane_scope.is_some() && PANE_FORBIDDEN_CALLS.contains(&m) {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    format!("{m} is the user's, never a pane's"),
                )));
            }
            let params = p.get("params").cloned().unwrap_or_else(|| json!({}));
            let ms = p
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS);
            call(server, m, params, Duration::from_millis(ms)).await
        }
        "gateway.reply" => gateway_only(ctx, method).and_then(|()| reply(server, p)),
        "gateway.status" => crate::gateway_supervisor::check_status_dir(server, p)
            .map(|()| crate::gateway_supervisor::status_method(server, connected(server))),
        _ => return None,
    })
}

/// Schema registry entries (`api_schema` loads them next to its own tables).
pub const SHAPES: &str = r##"
# --- the server-to-gateway bridge (spec 16 §15.5): peer.*, share.*, devices.*, pair.* and account.* run in the host's gateway ---
# full scope, never from a pane; method is one of peer.invite, peer.redeem, peer.list, peer.remove, share.create (kind handoff, peer or share), share.list, share.revoke, devices.list, devices.revoke, pair.create, pair.status, account.status, account.login.start, account.login.status, account.login.cancel, account.logout; answers with the gateway's result; remote_unavailable when no gateway is connected or it doesn't answer in time
gateway.call :: {method: string, params?: object, timeout_ms?: int = 30000} => any
# gateway clients only: the answer to a gateway.request event (result or error, not both)
gateway.reply :: {id: string, result?: any, error?: {kind: string, message: string, details?: any}} => {}
# whether a gateway holds an event stream open (connected), whether this server manages one (configured) and, when it does, the supervisor's GatewayStatus fields; dir: the caller's gateway dir, conflict when this server supervises another
gateway.status :: {dir?: string}
  => {connected: bool, configured: bool, state?: off|starting|connecting|online|offline|local_only|login_required|external|crashed, autostart?: bool, supervised?: bool, pid?: int|null, restarts?: int, relay?: string|null, devices?: int|null, since_ms?: int|null, last_error?: string|null, log?: string}
"##;

pub const EVENTS: &str = r##"
# transient (seq 0, never stored or replayed); goes only to the addressed gateway's subscription
gateway.request :: {id: string} => {id: string, method: string, params: object, client: string}
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// The gateway keeps the same list (`vk_gateway::bridge::ALLOWED`, tested against this same
    /// literal there); the crates don't depend on each other, so both pin it.
    #[test]
    fn allowed_matches_the_gateway() {
        assert_eq!(
            ALLOWED,
            [
                "peer.invite",
                "peer.redeem",
                "peer.list",
                "peer.remove",
                "share.create",
                "share.list",
                "share.revoke",
                "devices.list",
                "devices.revoke",
                "pair.create",
                "pair.status",
                "account.status",
                "account.login.start",
                "account.login.status",
                "account.login.cancel",
                "account.logout",
            ]
        );
        assert!(check_allowed("pair.create", &json!({})).is_ok());
        assert!(check_allowed("account.login.start", &json!({})).is_ok());
        assert!(check_allowed("devices.revoke", &json!({"device": "d"})).is_ok());
        assert!(check_allowed("auth.list", &json!({})).is_err());
    }

    #[test]
    fn share_create_kinds() {
        for k in ["handoff", "peer", "share"] {
            assert!(
                check_allowed("share.create", &json!({"kind": k})).is_ok(),
                "{k}"
            );
        }
        let share = json!({"kind": "share", "scope": "view", "pane": "p1", "ttl_s": 3600});
        assert!(check_allowed("share.create", &share).is_ok());
        for k in ["device", "owner", ""] {
            assert!(
                check_allowed("share.create", &json!({"kind": k})).is_err(),
                "{k}"
            );
        }
        assert!(check_allowed("share.create", &json!({})).is_err());
    }

    /// Signing in or out is never a pane's: `gateway.call` is refused to panes, the login
    /// methods are refused again by name, and `auth.approve` can't carry them.
    #[test]
    fn account_logins_are_not_for_panes() {
        use crate::api::{PaneScope, pane_scope_of};
        assert_eq!(pane_scope_of("gateway.call"), PaneScope::Forbidden);
        for m in PANE_FORBIDDEN_CALLS {
            assert!(ALLOWED.contains(m), "{m}");
            assert!(!crate::approve::APPROVABLE_GATEWAY.contains(m), "{m}");
        }
        assert!(!PANE_FORBIDDEN_CALLS.contains(&"account.status"));
        // Handing out access is the user's, whatever the kind.
        assert!(PANE_FORBIDDEN_CALLS.contains(&"share.create"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pane_callers_cannot_start_logins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = crate::paths::Paths {
            session: "t".into(),
            runtime: root.join("run"),
            state: root.join("state"),
        };
        let opts = crate::ServerOpts {
            session: "t".into(),
            machine: "m".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
            gateway: None,
        };
        let server = Server::new(paths, opts).unwrap();
        let pane = Ctx {
            client_id: "c".into(),
            kind: "tui".into(),
            pane_scope: Some("p1".into()),
            remote: false,
        };
        for m in PANE_FORBIDDEN_CALLS {
            let e = api(&server, &pane, "gateway.call", &json!({"method": m}))
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(e.data.kind, "permission_denied", "{m}");
        }
        let share = json!({"method": "share.create", "params": {"kind": "share", "pane": "p1"}});
        let e = api(&server, &pane, "gateway.call", &share)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(e.data.kind, "permission_denied");
    }
}
