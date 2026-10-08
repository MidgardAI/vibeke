//! Outgoing handoffs (spec 16 §15.2): the server keeps the job records so the TUI, the CLI and
//! the apps see the same state; the host's gateway runs them (vk-gateway `handoff_send.rs`).
//!
//! - `handoff.send {pane, peer, interrupt?}` records a `queued` job and emits `handoff.job`; the
//!   gateway picks it up, exports the pane's work, delivers it to the peer's gateway and reports
//!   back with `handoff.job.update` (gateway clients only): `exporting` → `sending` (with
//!   `sent`/`total`) → `delivered` (with the destination's `incoming_state`) or `failed`.
//! - `handoff.cancel {id}` stops a job that hasn't finished; the gateway's next update is refused,
//!   so it stops and discards the upload on the peer.
//! - `handoff.peers` lists the hosts a job may go to. The server can't read the gateway's
//!   `peers.json` (it holds private keys), so the gateway publishes the list (`handoff.peers.set`,
//!   without keys or addresses) at start and whenever it changes.
//!
//! Every change emits `handoff.job` with the whole record. Finished jobs are listed for 7 days.
//! Starting, cancelling and publishing are never available to panes. A pane may only *ask* for a
//! send or a cancel of its own jobs through `auth.approve` (`crate::approve`, 09 §3.2 "Approved
//! calls"); an approved send carries `expect`, the repository facts the user approved, and the
//! gateway refuses to deliver an export that doesn't match them.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s, u};
use crate::core::Tx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vk_proto::model::Pane;
use vk_proto::rpc::{ErrorKind, RpcError};

pub const K_JOB: &str = "handoff_job";
pub const K_PEERS: &str = "handoff_peers";

pub const METHODS: &[(&str, bool)] = &[
    ("handoff.send", true),
    ("handoff.jobs", false),
    ("handoff.cancel", true),
    ("handoff.job.update", true),
    ("handoff.peers", false),
    ("handoff.peers.set", true),
];

/// Sending work to another host, cancelling it and reporting for the gateway are the user's (an
/// agent could otherwise ship its repository elsewhere).
pub const PANE_FORBIDDEN: &[&str] = &[
    "handoff.send",
    "handoff.cancel",
    "handoff.job.update",
    "handoff.peers.set",
];

pub const STATES: &[&str] = &[
    "queued",
    "exporting",
    "sending",
    "delivered",
    "failed",
    "cancelled",
];

/// Finished jobs stay listed this long (seconds).
pub const KEEP_FINISHED_S: u64 = 7 * 86_400;
/// At most this many jobs are kept.
pub const MAX_JOBS: usize = 200;
const MAX_PEERS: usize = 100;
const MAX_ERROR: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    /// Pane id whose work is sent.
    pub pane: String,
    /// Peer id (from `handoff.peers`) and its name when the job was created.
    pub peer: String,
    pub peer_name: String,
    #[serde(default)]
    pub interrupt: bool,
    /// One of [`STATES`].
    pub state: String,
    /// Bytes the destination has, of `total`.
    #[serde(default)]
    pub sent: u64,
    #[serde(default)]
    pub total: u64,
    /// The destination's incoming handoff id and its state there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
    /// Who started it (`gateway:<device>` for apps, `pane:<id>` for a send approved from a
    /// pane, else the client kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// For a send approved from a pane (`auth.approve`): what the user approved, re-checked by
    /// the gateway against the export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
}

/// The repository facts an approved `handoff.send` was approved for, recorded when the pane
/// asked. The gateway compares them with the export's manifest (`source_root`, `branch`, `head`)
/// and fails the job when the pane's repository moved in between.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Expect {
    pub repo_root: String,
    pub branch: Option<String>,
    pub head: String,
    /// The approval request, the pane that asked and who approved (a client kind).
    pub request: String,
    pub requested_by: String,
    pub approved_by: String,
}

pub fn terminal(state: &str) -> bool {
    matches!(state, "delivered" | "failed" | "cancelled")
}

/// Whether a job in `from` may move to `to`. Unfinished jobs may restart their export (a gateway
/// restart resumes them) and fail or be cancelled at any point; only a job that was sending can
/// be delivered; a finished job never changes.
pub fn transition_ok(from: &str, to: &str) -> bool {
    if terminal(from) || !STATES.contains(&from) {
        return false;
    }
    match to {
        "exporting" | "failed" | "cancelled" => true,
        "sending" => from != "queued",
        "delivered" => from == "sending",
        _ => false,
    }
}

fn now_s() -> u64 {
    (vk_store::now_ms() / 1000).max(0) as u64
}

fn conflict(msg: impl Into<String>, job: &Job) -> RpcError {
    err(ErrorKind::Conflict, msg).details(json!({"job": job.id, "state": job.state}))
}

fn gateway_only(ctx: &Ctx, method: &str) -> Result<(), RpcError> {
    if ctx.kind == "gateway" && ctx.pane_scope.is_none() {
        Ok(())
    } else {
        Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is reported by the host's gateway"),
        ))
    }
}

fn job_json(job: &Job) -> Value {
    serde_json::to_value(job).unwrap_or(Value::Null)
}

fn put(tx: &mut Tx, job: &Job) {
    tx.m.put(K_JOB, &job.id, None, job);
    tx.event("handoff.job", json!({"job": job.id}), job_json(job));
}

pub fn api(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "handoff.send" => send(server, ctx, p),
        "handoff.jobs" => {
            Ok(json!({"jobs": list(server).iter().map(job_json).collect::<Vec<_>>()}))
        }
        "handoff.cancel" => cancel(server, p),
        "handoff.job.update" => gateway_only(ctx, method).and_then(|()| update(server, p)),
        "handoff.peers" => Ok(peers(server)),
        "handoff.peers.set" => gateway_only(ctx, method).and_then(|()| set_peers(server, p)),
        _ => return None,
    })
}

/// Jobs to show: newest first, finished ones only while they are recent.
pub fn list(server: &Server) -> Vec<Job> {
    let now = now_s();
    let mut jobs: Vec<Job> = server
        .with_core(|c| c.store.load::<Job>(K_JOB))
        .unwrap_or_default()
        .into_iter()
        .filter(|j| !terminal(&j.state) || j.updated_at + KEEP_FINISHED_S > now)
        .collect();
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    jobs.truncate(MAX_JOBS);
    jobs
}

/// The published peer list, with `expired` brought up to date.
fn peer_list(server: &Server) -> (Vec<Value>, Option<u64>) {
    let stored = server
        .with_core(|c| c.store.get::<Value>(K_PEERS, "list"))
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    let now = now_s();
    let peers = stored
        .get("peers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut p| {
            let expired = p
                .get("expires_at")
                .and_then(Value::as_u64)
                .is_some_and(|t| t <= now);
            p["expired"] = expired.into();
            p
        })
        .collect();
    (peers, u(&stored, "updated_at"))
}

fn peers(server: &Server) -> Value {
    let (peers, at) = peer_list(server);
    json!({"peers": peers, "updated_at": at})
}

fn set_peers(server: &Server, p: &Value) -> R {
    let given = p
        .get("peers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("peers must be an array"))?;
    let mut peers = Vec::new();
    for x in given.iter().take(MAX_PEERS) {
        let (Some(id), Some(name)) = (s(x, "id"), s(x, "name")) else {
            return Err(invalid("every peer needs an id and a name"));
        };
        let owner = match s(x, "owner") {
            Some("self") => "self",
            _ => "teammate",
        };
        peers.push(json!({
            "id": id, "name": name, "owner": owner,
            "added_at": u(x, "added_at"),
            "expires_at": u(x, "expires_at"),
            "expired": b(x, "expired").unwrap_or(false),
        }));
    }
    let (before, _) = peer_list(server);
    let ids = |v: &[Value]| -> Vec<Value> {
        v.iter()
            .map(|p| json!([p["id"], p["name"], p["owner"], p["expires_at"]]))
            .collect()
    };
    let changed = ids(&before) != ids(&peers);
    let n = peers.len();
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put(
        K_PEERS,
        "list",
        None,
        &json!({"peers": peers, "updated_at": now_s()}),
    );
    if changed {
        tx.event("handoff.peers_changed", json!({}), json!({"peers": n}));
    }
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"peers": n, "changed": changed}))
}

/// The pane and peer `handoff.send {pane, peer}` would use, refused as `handoff.send` refuses
/// them: no such peer, expired access, a handoff of that pane already under way. `auth.approve`
/// validates a pane's request with it before asking the user.
pub(crate) fn check_send(server: &Server, ctx: &Ctx, p: &Value) -> Result<(Pane, Value), RpcError> {
    let pane = crate::api::resolve_pane(server, ctx, s(p, "pane"))?;
    let want = req(p, "peer")?;
    let (peers, _) = peer_list(server);
    let by_id: Vec<&Value> = peers.iter().filter(|x| s(x, "id") == Some(want)).collect();
    let by_name: Vec<&Value> = peers
        .iter()
        .filter(|x| s(x, "name") == Some(want))
        .collect();
    let peer = match (by_id.first(), by_name.as_slice()) {
        (Some(x), _) => *x,
        (None, [x]) => *x,
        (None, []) => {
            return Err(err(
                ErrorKind::NotFound,
                format!(
                    "no peer {want}; `vibeke handoff peers` lists them, `vibeke gateway peer add <link>` adds one"
                ),
            )
            .details(json!({"object": "peer", "target": want})));
        }
        (None, _) => {
            return Err(err(
                ErrorKind::AmbiguousTarget,
                format!("several peers are called {want}; use the id"),
            ));
        }
    };
    if b(peer, "expired") == Some(true) {
        return Err(err(
            ErrorKind::PermissionDenied,
            format!("access to {want} has expired"),
        ));
    }
    let jobs = server
        .with_core(|c| c.store.load::<Job>(K_JOB))
        .map_err(internal)?;
    if let Some(busy) = jobs
        .iter()
        .find(|j| j.pane == pane.id && !terminal(&j.state))
    {
        return Err(conflict(
            "a handoff of this pane is already under way; cancel it first",
            busy,
        ));
    }
    Ok((pane, peer.clone()))
}

fn send(server: &Server, ctx: &Ctx, p: &Value) -> R {
    send_job(server, ctx, p, None)
}

/// `handoff.send`; `expect` is set for a send a pane asked for and the user approved
/// (`auth.approve`).
pub(crate) fn send_job(server: &Server, ctx: &Ctx, p: &Value, expect: Option<Expect>) -> R {
    let (pane, peer) = check_send(server, ctx, p)?;
    let want = req(p, "peer")?;
    // The gateway runs the job: start it if it is set up but not running.
    crate::gateway_supervisor::ensure_running(server);
    let now = now_s();
    let by = match &expect {
        Some(e) => format!("pane:{}", e.requested_by),
        None => s(p, "actor")
            .map(str::to_string)
            .unwrap_or_else(|| ctx.kind.clone()),
    };
    let job = Job {
        id: crate::core::ulid(),
        pane: pane.id.clone(),
        peer: s(&peer, "id").unwrap_or(want).to_string(),
        peer_name: s(&peer, "name").unwrap_or(want).to_string(),
        interrupt: b(p, "interrupt").unwrap_or(false),
        state: "queued".into(),
        sent: 0,
        total: 0,
        incoming: None,
        incoming_state: None,
        error: None,
        created_at: now,
        updated_at: now,
        by: Some(by),
        expect,
    };
    let mut c = server.core.lock().unwrap();
    let jobs = c.store.load::<Job>(K_JOB).map_err(internal)?;
    if let Some(busy) = jobs
        .iter()
        .find(|j| j.pane == job.pane && !terminal(&j.state))
    {
        return Err(conflict(
            "a handoff of this pane is already under way; cancel it first",
            busy,
        ));
    }
    let mut tx = Tx::new();
    // Forget old finished jobs (and the oldest beyond the cap) while we are here.
    let mut finished: Vec<&Job> = jobs.iter().filter(|j| terminal(&j.state)).collect();
    finished.sort_by_key(|j| std::cmp::Reverse(j.updated_at));
    for (i, j) in finished.iter().enumerate() {
        if j.updated_at + KEEP_FINISHED_S <= now || i + 1 >= MAX_JOBS {
            tx.m.delete(K_JOB, &j.id);
        }
    }
    put(&mut tx, &job);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"job": job_json(&job)}))
}

/// Read-modify-write one job under the core lock, then emit `handoff.job`.
fn modify(server: &Server, id: &str, f: impl FnOnce(&mut Job) -> Result<bool, RpcError>) -> R {
    let mut c = server.core.lock().unwrap();
    let mut job = c
        .store
        .get::<Job>(K_JOB, id)
        .map_err(internal)?
        .ok_or_else(|| not_found("handoff job", id))?;
    if f(&mut job)? {
        job.updated_at = now_s();
        let mut tx = Tx::new();
        put(&mut tx, &job);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(json!({"job": job_json(&job)}))
}

/// One job record (`auth.approve` checks a pane's cancel against it).
pub(crate) fn get(server: &Server, id: &str) -> Result<Job, RpcError> {
    server
        .with_core(|c| c.store.get::<Job>(K_JOB, id))
        .map_err(internal)?
        .ok_or_else(|| not_found("handoff job", id))
}

pub(crate) fn cancel(server: &Server, p: &Value) -> R {
    let id = req(p, "id")?;
    modify(server, id, |job| match job.state.as_str() {
        "cancelled" => Ok(false),
        st if terminal(st) => Err(conflict(format!("the handoff is already {st}"), job)),
        _ => {
            job.state = "cancelled".into();
            Ok(true)
        }
    })
}

fn update(server: &Server, p: &Value) -> R {
    let id = req(p, "id")?;
    let state = s(p, "state");
    if let Some(st) = state
        && !STATES.contains(&st)
    {
        return Err(invalid(format!("unknown state {st}")));
    }
    modify(server, id, |job| {
        if terminal(&job.state) {
            return Err(conflict(format!("the handoff is {}", job.state), job));
        }
        if let Some(st) = state {
            if st != job.state && !transition_ok(&job.state, st) {
                return Err(conflict(
                    format!("a {} handoff can't become {st}", job.state),
                    job,
                ));
            }
            job.state = st.to_string();
        }
        if let Some(n) = u(p, "total") {
            job.total = n;
        }
        if let Some(n) = u(p, "sent") {
            job.sent = if job.total > 0 { n.min(job.total) } else { n };
        }
        if let Some(x) = s(p, "incoming") {
            job.incoming = Some(x.chars().take(100).collect());
        }
        if let Some(x) = s(p, "incoming_state") {
            job.incoming_state = Some(x.chars().take(40).collect());
        }
        if let Some(x) = s(p, "error") {
            job.error = Some(x.chars().take(MAX_ERROR).collect());
        }
        if job.state == "delivered" {
            job.sent = job.total;
        }
        Ok(true)
    })
}

/// Schema registry entries (`api_schema` loads them next to its own tables).
pub const SHAPES: &str = r##"
# --- outgoing handoffs (spec 16 §15.2): jobs the host's gateway runs; full scope, never from a pane (a pane asks with auth.approve) ---
handoff.send :: {pane?: Target, peer: string, interrupt?: bool = false}
  => {job: {id: string, pane: string, peer: string, peer_name: string, interrupt: bool, state: queued|exporting|sending|delivered|failed|cancelled, sent: int, total: int, incoming?: string, incoming_state?: string, error?: string, created_at: int, updated_at: int, by?: string, expect?: {repo_root: string, branch: string|null, head: string, request: string, requested_by: string, approved_by: string}}}
# newest first; finished jobs for 7 days
handoff.jobs :: {}
  => {jobs: [{id: string, pane: string, peer: string, peer_name: string, interrupt: bool, state: queued|exporting|sending|delivered|failed|cancelled, sent: int, total: int, incoming?: string, incoming_state?: string, error?: string, created_at: int, updated_at: int, by?: string, expect?: {repo_root: string, branch: string|null, head: string, request: string, requested_by: string, approved_by: string}}]}
handoff.cancel :: {id: string}
  => {job: {id: string, pane: string, peer: string, peer_name: string, interrupt: bool, state: queued|exporting|sending|delivered|failed|cancelled, sent: int, total: int, incoming?: string, incoming_state?: string, error?: string, created_at: int, updated_at: int, by?: string, expect?: {repo_root: string, branch: string|null, head: string, request: string, requested_by: string, approved_by: string}}}
# gateway clients only: progress and outcome; a finished or cancelled job refuses updates (conflict)
handoff.job.update :: {id: string, state?: queued|exporting|sending|delivered|failed|cancelled, sent?: int, total?: int, incoming?: string, incoming_state?: string, error?: string}
  => {job: {id: string, pane: string, peer: string, peer_name: string, interrupt: bool, state: queued|exporting|sending|delivered|failed|cancelled, sent: int, total: int, incoming?: string, incoming_state?: string, error?: string, created_at: int, updated_at: int, by?: string, expect?: {repo_root: string, branch: string|null, head: string, request: string, requested_by: string, approved_by: string}}}
# the hosts handoff.send can deliver to, as the gateway last published them
handoff.peers :: {} => {peers: [{id: string, name: string, owner: self|teammate, added_at: int|null, expires_at: int|null, expired: bool}], updated_at: int|null}
# gateway clients only
handoff.peers.set :: {peers: [{id: string, name: string, owner?: string, added_at?: int, expires_at?: int|null, expired?: bool}]} => {peers: int, changed: bool}
"##;

pub const EVENTS: &str = r##"
handoff.job :: {job: string} => {id: string, pane: string, peer: string, peer_name: string, interrupt: bool, state: queued|exporting|sending|delivered|failed|cancelled, sent: int, total: int, incoming?: string, incoming_state?: string, error?: string, created_at: int, updated_at: int, by?: string, expect?: {repo_root: string, branch: string|null, head: string, request: string, requested_by: string, approved_by: string}}
handoff.peers_changed :: {} => {peers: int}
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::dispatch;
    use crate::hardening::testkit::{pane_ctx, sample_pane, server, user};
    use std::sync::Arc;

    fn gateway() -> Ctx {
        Ctx {
            client_id: "c-gw".into(),
            kind: "gateway".into(),
            pane_scope: None,
            remote: false,
        }
    }

    fn setup(session: &str) -> (tempfile::TempDir, Arc<Server>) {
        let dir = tempfile::tempdir().unwrap();
        let s = server(dir.path(), session);
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(sample_pane("p1", "w1"));
        tx.pane(sample_pane("p2", "w1"));
        s.commit(&mut c, tx).unwrap();
        drop(c);
        (dir, s)
    }

    async fn call(s: &Arc<Server>, ctx: &Ctx, m: &str, p: Value) -> R {
        dispatch(s, ctx, m, &p).await
    }

    fn kind(e: &RpcError) -> String {
        e.data.kind.clone()
    }

    async fn publish(s: &Arc<Server>) {
        let r = call(
            s,
            &gateway(),
            "handoff.peers.set",
            json!({"peers": [
                {"id": "pr1", "name": "laptop", "owner": "self", "added_at": 1, "expires_at": null, "expired": false},
                {"id": "pr2", "name": "kari", "owner": "teammate", "added_at": 1, "expires_at": 1, "expired": false},
            ]}),
        )
        .await
        .unwrap();
        assert_eq!(r["peers"], 2);
    }

    fn job_events(s: &Server) -> Vec<Value> {
        s.with_core(|c| {
            c.store
                .events_after(0, 10_000, &["handoff.job".to_string()])
                .unwrap()
        })
        .into_iter()
        .map(|e| e.data)
        .collect()
    }

    #[test]
    fn transitions() {
        for (from, to, ok) in [
            ("queued", "exporting", true),
            ("queued", "sending", false),
            ("queued", "delivered", false),
            ("queued", "cancelled", true),
            ("queued", "failed", true),
            ("exporting", "sending", true),
            ("exporting", "delivered", false),
            ("sending", "delivered", true),
            ("sending", "exporting", true),
            ("sending", "queued", false),
            ("delivered", "failed", false),
            ("failed", "exporting", false),
            ("cancelled", "sending", false),
            ("bogus", "failed", false),
        ] {
            assert_eq!(transition_ok(from, to), ok, "{from} -> {to}");
        }
        assert_eq!(STATES.iter().filter(|s| terminal(s)).count(), 3);
    }

    #[tokio::test]
    async fn peers_are_published_by_the_gateway_only() {
        let (_d, s) = setup("hpeers");
        let empty = call(&s, &user(), "handoff.peers", json!({})).await.unwrap();
        assert_eq!(empty["peers"], json!([]));
        assert!(empty["updated_at"].is_null());
        for ctx in [user(), pane_ctx("p1")] {
            let e = call(&s, &ctx, "handoff.peers.set", json!({"peers": []}))
                .await
                .unwrap_err();
            assert_eq!(kind(&e), "permission_denied");
        }
        publish(&s).await;
        let r = call(&s, &user(), "handoff.peers", json!({})).await.unwrap();
        let peers = r["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0]["name"], "laptop");
        assert_eq!(peers[0]["expired"], false);
        // Expiry is judged when listing.
        assert_eq!(peers[1]["expired"], true);
        assert!(r["updated_at"].as_u64().is_some());
        // Publishing the same list again changes nothing.
        let again = call(
            &s,
            &gateway(),
            "handoff.peers.set",
            json!({"peers": [
                {"id": "pr1", "name": "laptop", "owner": "self", "expires_at": null},
                {"id": "pr2", "name": "kari", "owner": "teammate", "expires_at": 1},
            ]}),
        )
        .await
        .unwrap();
        assert_eq!(again["changed"], false);
        // Panes may read the list.
        call(&s, &pane_ctx("p1"), "handoff.peers", json!({}))
            .await
            .unwrap();
        let e = call(
            &s,
            &gateway(),
            "handoff.peers.set",
            json!({"peers": [{"name": "no id"}]}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&e), "invalid_params");
    }

    #[tokio::test]
    async fn send_records_a_queued_job() {
        let (_d, s) = setup("hsend");
        // No peers published yet.
        let e = call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "laptop"}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&e), "not_found");
        publish(&s).await;

        // Panes can't send work away.
        let e = call(
            &s,
            &pane_ctx("p1"),
            "handoff.send",
            json!({"pane": "p1", "peer": "laptop"}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&e), "permission_denied");

        // By name or id; an expired peer is refused.
        let e = call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "kari"}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&e), "permission_denied");
        let r = call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "laptop", "interrupt": true}),
        )
        .await
        .unwrap();
        let job = &r["job"];
        assert_eq!(job["state"], "queued");
        assert_eq!(job["pane"], "p1");
        assert_eq!(job["peer"], "pr1");
        assert_eq!(job["peer_name"], "laptop");
        assert_eq!(job["interrupt"], true);
        assert_eq!(job["sent"], 0);
        assert!(job.get("error").is_none());
        let id = job["id"].as_str().unwrap().to_string();

        // One running handoff per pane; another pane is fine.
        let e = call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "pr1"}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&e), "conflict");
        assert_eq!(e.data.details["job"], id.as_str());
        call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p2", "peer": "pr1"}),
        )
        .await
        .unwrap();

        let list = call(&s, &pane_ctx("p1"), "handoff.jobs", json!({}))
            .await
            .unwrap();
        let jobs = list["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|j| j["id"] == id.as_str()));
        let ev = job_events(&s);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0]["id"], id.as_str());
        assert_eq!(ev[0]["state"], "queued");
    }

    #[tokio::test]
    async fn the_gateway_reports_progress_and_outcome() {
        let (_d, s) = setup("hupdate");
        publish(&s).await;
        let r = call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "laptop"}),
        )
        .await
        .unwrap();
        let id = r["job"]["id"].as_str().unwrap().to_string();
        let upd = |p: Value| {
            let s = s.clone();
            let mut p = p;
            p["id"] = id.clone().into();
            async move { call(&s, &gateway(), "handoff.job.update", p).await }
        };

        // Only the gateway reports.
        let e = call(
            &s,
            &user(),
            "handoff.job.update",
            json!({"id": id, "state": "exporting"}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&e), "permission_denied");
        // Sending can't start before the export.
        let e = upd(json!({"state": "sending"})).await.unwrap_err();
        assert_eq!(kind(&e), "conflict");
        let e = upd(json!({"state": "lost"})).await.unwrap_err();
        assert_eq!(kind(&e), "invalid_params");

        upd(json!({"state": "exporting"})).await.unwrap();
        let r = upd(json!({"state": "sending", "sent": 0, "total": 3000}))
            .await
            .unwrap();
        assert_eq!(r["job"]["state"], "sending");
        assert_eq!(r["job"]["total"], 3000);
        let r = upd(json!({"sent": 1000, "total": 3000})).await.unwrap();
        assert_eq!(r["job"]["sent"], 1000);
        // A gateway restart exports again.
        upd(json!({"state": "exporting"})).await.unwrap();
        upd(json!({"state": "sending", "sent": 2000, "total": 3000}))
            .await
            .unwrap();
        let r = upd(json!({"state": "delivered", "incoming": "in1", "incoming_state": "pending"}))
            .await
            .unwrap();
        let job = &r["job"];
        assert_eq!(job["state"], "delivered");
        assert_eq!(job["sent"], 3000);
        assert_eq!(job["incoming"], "in1");
        assert_eq!(job["incoming_state"], "pending");

        // Finished: no more updates, no cancel; the pane is free again.
        let e = upd(json!({"sent": 5})).await.unwrap_err();
        assert_eq!(kind(&e), "conflict");
        let e = call(&s, &user(), "handoff.cancel", json!({"id": id}))
            .await
            .unwrap_err();
        assert_eq!(kind(&e), "conflict");
        call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "laptop"}),
        )
        .await
        .unwrap();

        // Every change was announced with the whole record.
        let states: Vec<String> = job_events(&s)
            .iter()
            .filter(|e| e["id"] == id.as_str())
            .map(|e| e["state"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            states,
            [
                "queued",
                "exporting",
                "sending",
                "sending",
                "exporting",
                "sending",
                "delivered"
            ]
        );
    }

    #[tokio::test]
    async fn cancelling_stops_the_gateway() {
        let (_d, s) = setup("hcancel");
        publish(&s).await;
        let r = call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "laptop"}),
        )
        .await
        .unwrap();
        let id = r["job"]["id"].as_str().unwrap().to_string();
        call(
            &s,
            &gateway(),
            "handoff.job.update",
            json!({"id": id, "state": "exporting"}),
        )
        .await
        .unwrap();
        let e = call(&s, &pane_ctx("p1"), "handoff.cancel", json!({"id": id}))
            .await
            .unwrap_err();
        assert_eq!(kind(&e), "permission_denied");
        let r = call(&s, &user(), "handoff.cancel", json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(r["job"]["state"], "cancelled");
        // Cancelling again is a no-op; the gateway's next update is refused.
        let r = call(&s, &user(), "handoff.cancel", json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(r["job"]["state"], "cancelled");
        let e = call(
            &s,
            &gateway(),
            "handoff.job.update",
            json!({"id": id, "state": "sending", "sent": 1, "total": 2}),
        )
        .await
        .unwrap_err();
        assert_eq!(kind(&e), "conflict");
        assert_eq!(e.data.details["state"], "cancelled");
        let e = call(&s, &user(), "handoff.cancel", json!({"id": "nope"}))
            .await
            .unwrap_err();
        assert_eq!(kind(&e), "not_found");
        // A failure keeps its reason.
        let r = call(
            &s,
            &user(),
            "handoff.send",
            json!({"pane": "p1", "peer": "laptop"}),
        )
        .await
        .unwrap();
        let id2 = r["job"]["id"].as_str().unwrap().to_string();
        let r = call(
            &s,
            &gateway(),
            "handoff.job.update",
            json!({"id": id2, "state": "failed", "error": "busy: the agent is working"}),
        )
        .await
        .unwrap();
        assert_eq!(r["job"]["state"], "failed");
        assert_eq!(r["job"]["error"], "busy: the agent is working");
    }
}
