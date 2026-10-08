//! Approved calls (09 §3.2 "Approved calls"): a pane may *ask* the user to run one specific call
//! it is not allowed to make itself, and the user decides outside the pane.
//!
//! - `auth.approve {method, params, reason?, wait?, timeout_ms?, request?}` (pane scope only).
//!   [`APPROVABLE`] lists what can be asked for: `handoff.send`, `handoff.cancel` of the pane's
//!   own jobs, and `gateway.call {method: "peer.redeem"}`. The params are validated as the
//!   target method would validate them and frozen; the summary the user reads is computed here
//!   from server facts (pane, repository, branch, changed files, agent, peer and its owner),
//!   never from the caller's text. The caller's `reason` is carried separately and marked
//!   unverified. At most [`crate::auth::MAX_OPEN_PER_PANE`] requests are open per pane. The
//!   request emits `auth.approval_requested`, sends a high-urgency notification and waits;
//!   `wait: false` returns the request at once and `request` resumes waiting on it.
//! - `auth.approve.decide {request, decision: approve|always|deny}` (full scope, never from a
//!   pane or an elevated connection, exactly like `auth.elevate.decide`). Approving runs the
//!   frozen call exactly once, as the user ([`run_approved`]); the waiting call returns the
//!   target's result. `always` also records a standing grant for (pane, method, peer, target
//!   pane) that lasts until the pane's process restarts: in memory only, cleared with the pane
//!   token (revocation). A later identical ask is then run at once, announced and audited.
//! - `auth.approve.withdraw {request}` (the asking pane). A waiter whose connection closes
//!   withdraws its request too ([`client_gone`]).
//!
//! An approved `handoff.send` records the repository root, branch and HEAD seen when the pane
//! asked (`expect` on the job). Approving re-checks them first, and the gateway re-checks its
//! export against them, so a pane that moved to another repository, branch or commit in between
//! fails with `repo_moved` instead of sending something the user didn't see.
//!
//! Events: `auth.approval_requested`, `auth.approval_granted`, `auth.approval_denied`,
//! `auth.approval_withdrawn`; every one is also an audit entry. `auth.list` shows the open
//! requests (`approvals`) and the standing grants (`grants`).

use crate::Server;
use crate::api::{Ctx, R, err, invalid, not_found, req, s, u};
use crate::core::{Tx, ulid};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_store::now_ms;

/// Methods a pane can ask the user to run.
pub const APPROVABLE: &[&str] = &["handoff.send", "handoff.cancel", "gateway.call"];
/// `gateway.call` methods a pane can ask for.
pub const APPROVABLE_GATEWAY: &[&str] = &["peer.redeem"];
/// How long `auth.approve` waits for a decision by default.
const DEFAULT_WAIT_MS: u64 = 120_000;
/// Requests nobody decided are withdrawn after this long.
const REQUEST_TTL_MS: i64 = 30 * 60 * 1000;
/// How long an approved `gateway.call` may take.
const GATEWAY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REASON: usize = 500;
const MAX_LINK: usize = 4096;

#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    requests: HashMap<String, Request>,
    grants: Vec<Grant>,
}

/// How a request ended.
#[derive(Clone)]
enum Outcome {
    /// Approved and run: the target method's result, or its error.
    Ran(Result<Value, RpcError>),
    Denied,
    /// Withdrawn by the pane, its connection closing, revocation, restart or age.
    Withdrawn(&'static str),
}

#[derive(Clone)]
struct Peer {
    id: String,
    name: String,
    owner: String,
}

/// A frozen call: what runs when the user approves.
#[derive(Clone)]
struct Frozen {
    method: String,
    params: Value,
    /// The pane the call acts on (the sent pane, the cancelled job's pane), if any.
    target: Option<String>,
    peer: Option<Peer>,
    /// `handoff.send`: repository root, branch and HEAD when the pane asked.
    repo: Option<(String, Option<String>, String)>,
    summary: String,
    facts: Value,
    always_allowed: bool,
}

#[derive(Clone)]
struct Request {
    id: String,
    /// The asking pane.
    pane: String,
    pane_handle: String,
    workspace: String,
    /// The asking pane's child process when it asked (a restarted pane can't collect).
    child_pid: Option<u32>,
    frozen: Frozen,
    reason: String,
    created_at_ms: i64,
    /// Set while an approved call runs (so it runs once).
    running: bool,
    /// Client ids of the connections waiting on it.
    waiters: Vec<String>,
    decision: watch::Sender<Option<Outcome>>,
}

#[derive(Clone)]
struct Grant {
    pane: String,
    child_pid: Option<u32>,
    method: String,
    target: Option<String>,
    peer: String,
    peer_name: String,
    request: String,
    created_at_ms: i64,
}

fn state(server: &Server) -> &State {
    &server.security.approve
}

fn denied(msg: impl Into<String>) -> RpcError {
    err(ErrorKind::PermissionDenied, msg).details(json!({"scope": "pane"}))
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "auth.approve" => approve(server, ctx, p).await,
        "auth.approve.decide" => decide(server, ctx, p).await,
        "auth.approve.withdraw" => withdraw(server, ctx, p),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Validation and the server's summary.

fn owner_text(owner: &str) -> &'static str {
    if owner == "self" {
        "your host"
    } else {
        "a teammate's host"
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// The published peer by id, for its owner (cancel requests).
fn peer_by_id(server: &Server, ctx: &Ctx, id: &str) -> Option<Peer> {
    let r = crate::handoff_out::api(server, ctx, "handoff.peers", &json!({}))?.ok()?;
    let x = r["peers"]
        .as_array()?
        .iter()
        .find(|x| s(x, "id") == Some(id))?
        .clone();
    Some(Peer {
        id: id.to_string(),
        name: s(&x, "name").unwrap_or(id).to_string(),
        owner: s(&x, "owner").unwrap_or("teammate").to_string(),
    })
}

/// The agent in `pane`, as the summary names it.
fn agent_of(server: &Server, pane: &str) -> Option<String> {
    server.with_core(|c| {
        c.run_for_pane(pane).map(|r| {
            if r.ended_at_ms.is_some() {
                format!("{} (exited)", r.harness)
            } else {
                r.harness.clone()
            }
        })
    })
}

fn cwd_of(server: &Server, pane: &vk_proto::model::Pane) -> Option<String> {
    server.pane_cwd(&pane.id).or_else(|| pane.cwd.clone())
}

/// Validate a pane's ask as the target method would, and freeze it with the summary.
async fn prepare(
    server: &Server,
    ctx: &Ctx,
    me: &str,
    method: &str,
    p: &Value,
) -> Result<Frozen, RpcError> {
    let params = p.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return Err(invalid("params must be an object"));
    }
    match method {
        "handoff.send" => {
            let (pane, peer) = crate::handoff_out::check_send(server, ctx, &params)?;
            let my_ws = server.with_core(|c| c.pane(me).map(|x| x.workspace.clone()));
            if pane.id != me && my_ws.as_deref() != Some(pane.workspace.as_str()) {
                return Err(denied(
                    "a pane can only ask to send itself or a pane of its own workspace",
                ));
            }
            let cwd = cwd_of(server, &pane)
                .ok_or_else(|| err(ErrorKind::NotFound, "pane has no working directory"))?;
            let (root, branch, head, changed) =
                crate::git_api::repo_facts(std::path::Path::new(&cwd)).await?;
            let root_s = root.display().to_string();
            let repo_name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root_s.clone());
            let agent = agent_of(server, &pane.id);
            let interrupt = params.get("interrupt").and_then(Value::as_bool) == Some(true);
            let peer = Peer {
                id: s(&peer, "id").unwrap_or_default().to_string(),
                name: s(&peer, "name").unwrap_or_default().to_string(),
                owner: s(&peer, "owner").unwrap_or("teammate").to_string(),
            };
            let summary = format!(
                "Send pane {} (repo {repo_name}, branch {}, {}, agent: {}) to {} ({}){}",
                pane.handle,
                branch.as_deref().unwrap_or("(detached)"),
                plural(changed, "changed file"),
                agent.as_deref().unwrap_or("none"),
                peer.name,
                owner_text(&peer.owner),
                if interrupt && agent.is_some() {
                    ", interrupting the agent if it is working"
                } else {
                    ""
                }
            );
            let facts = json!({
                "pane": pane.id, "pane_handle": pane.handle, "cwd": cwd,
                "repo_root": root_s, "repo_name": repo_name, "branch": branch, "head": head,
                "changed_files": changed, "agent": agent, "interrupt": interrupt,
            });
            Ok(Frozen {
                method: method.into(),
                params: json!({"pane": pane.id, "peer": peer.id, "interrupt": interrupt}),
                target: Some(pane.id.clone()),
                repo: Some((root_s, branch, head)),
                peer: Some(peer),
                summary,
                facts,
                always_allowed: true,
            })
        }
        "handoff.cancel" => {
            let id = req(&params, "id")?;
            let job = crate::handoff_out::get(server, id)?;
            let mine = job.pane == me || job.expect.as_ref().is_some_and(|e| e.requested_by == me);
            if !mine {
                return Err(denied(
                    "a pane can only ask to cancel its own pane's handoffs",
                ));
            }
            if crate::handoff_out::terminal(&job.state) {
                return Err(err(
                    ErrorKind::Conflict,
                    format!("the handoff is already {}", job.state),
                )
                .details(json!({"job": job.id, "state": job.state})));
            }
            let handle = server
                .with_core(|c| c.pane(&job.pane).map(|x| x.handle.clone()))
                .unwrap_or_else(|| job.pane.clone());
            let peer = peer_by_id(server, ctx, &job.peer).unwrap_or(Peer {
                id: job.peer.clone(),
                name: job.peer_name.clone(),
                owner: "teammate".into(),
            });
            let progress = (job.sent * 100)
                .checked_div(job.total)
                .map(|pct| format!(", {pct}% sent"))
                .unwrap_or_default();
            let summary = format!(
                "Cancel the handoff of pane {handle} to {} ({}; {}{progress})",
                peer.name,
                owner_text(&peer.owner),
                job.state
            );
            let facts = json!({"job": job.id, "job_state": job.state, "pane": job.pane, "pane_handle": handle});
            Ok(Frozen {
                method: method.into(),
                params: json!({"id": job.id}),
                target: Some(job.pane.clone()),
                repo: None,
                peer: Some(peer),
                summary,
                facts,
                always_allowed: true,
            })
        }
        "gateway.call" => {
            let inner = s(&params, "method").unwrap_or("");
            if !APPROVABLE_GATEWAY.contains(&inner) {
                return Err(invalid(format!(
                    "only gateway.call {{method: {}}} can be asked for from a pane",
                    APPROVABLE_GATEWAY.join("|")
                )));
            }
            let args = params.get("params").cloned().unwrap_or_else(|| json!({}));
            let link = s(&args, "link").map(str::trim).unwrap_or("");
            if link.is_empty() {
                return Err(invalid("link is required"));
            }
            if link.len() > MAX_LINK {
                return Err(invalid("link is too long"));
            }
            let share_user = args.get("share_user").and_then(Value::as_bool) == Some(true);
            let summary = format!(
                "Redeem a peer invitation: pair this host with the host that made the link, so work can be handed between them{}",
                if share_user {
                    "; your git user name and email are shown to that host"
                } else {
                    ""
                }
            );
            Ok(Frozen {
                method: method.into(),
                params: json!({"method": inner, "params": {"link": link, "share_user": share_user}}),
                target: None,
                repo: None,
                peer: None,
                summary,
                facts: json!({"gateway_method": inner, "share_user": share_user}),
                always_allowed: false,
            })
        }
        other => Err(invalid(format!(
            "{other} can't be asked for; auth.approve covers {}",
            APPROVABLE.join(", ")
        ))),
    }
}

/// The frozen params as listed: an invitation link is a secret and is never shown.
fn shown_params(f: &Frozen) -> Value {
    let mut p = f.params.clone();
    if let Some(l) = p.pointer_mut("/params/link") {
        *l = json!("(hidden)");
    }
    p
}

fn peer_json(p: Option<&Peer>) -> Value {
    match p {
        Some(p) => json!({"id": p.id, "name": p.name, "owner": p.owner}),
        None => Value::Null,
    }
}

fn status_of(r: &Request) -> &'static str {
    match &*r.decision.borrow() {
        None if r.running => "running",
        None => "pending",
        Some(Outcome::Ran(Ok(_))) => "approved",
        Some(Outcome::Ran(Err(_))) => "failed",
        Some(Outcome::Denied) => "denied",
        Some(Outcome::Withdrawn(_)) => "withdrawn",
    }
}

fn request_json(r: &Request) -> Value {
    json!({
        "request": r.id, "kind": "approval",
        "pane": r.pane, "pane_handle": r.pane_handle, "workspace": r.workspace,
        "method": r.frozen.method, "params": shown_params(&r.frozen),
        "summary": r.frozen.summary, "facts": r.frozen.facts,
        "reason": r.reason, "reason_verified": false,
        "peer": peer_json(r.frozen.peer.as_ref()), "always_allowed": r.frozen.always_allowed,
        "created_at_ms": r.created_at_ms, "status": status_of(r),
    })
}

fn subject(pane: &str, request: &str) -> Value {
    json!({"pane": pane, "request": request})
}

/// Emit `kind` and record it in the audit log.
fn announce(server: &Server, kind: &str, actor: Value, subj: Value, data: Value) {
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event_by(kind, subj.clone(), actor.clone(), data.clone());
        let _ = server.commit(&mut c, tx);
    }
    crate::audit::record(server, kind, actor, subj, data);
}

fn server_actor() -> Value {
    json!({"kind": "server"})
}

// ---------------------------------------------------------------------------------------------
// Pruning, revocation, disconnects.

/// The current child process of each pane (`None` for a pane that is gone).
fn pids(server: &Server, panes: Vec<String>) -> HashMap<String, Option<Option<u32>>> {
    server.with_core(|c| {
        panes
            .into_iter()
            .map(|p| {
                let pid = c.pane(&p).map(|x| x.child_pid);
                (p, pid)
            })
            .collect()
    })
}

/// End requests (and grants) whose pane restarted or is gone, and requests left undecided too
/// long.
fn prune(server: &Server) {
    let panes: Vec<String> = {
        let g = state(server).inner.lock().unwrap();
        g.requests
            .values()
            .map(|r| r.pane.clone())
            .chain(g.grants.iter().map(|gr| gr.pane.clone()))
            .collect()
    };
    let now_pids = pids(server, panes);
    let alive = |pane: &str, pid: Option<u32>| now_pids.get(pane) == Some(&Some(pid));
    let now = now_ms();
    let mut ended = Vec::new();
    {
        let mut g = state(server).inner.lock().unwrap();
        g.grants.retain(|gr| alive(&gr.pane, gr.child_pid));
        let mut drop_ids = Vec::new();
        for r in g.requests.values() {
            let decided = r.decision.borrow().is_some();
            if decided {
                // Outcomes nobody collected.
                if now - r.created_at_ms >= REQUEST_TTL_MS {
                    drop_ids.push((r.id.clone(), None));
                }
                continue;
            }
            if r.running {
                continue;
            }
            if !alive(&r.pane, r.child_pid) {
                drop_ids.push((r.id.clone(), Some("pane_restarted")));
            } else if now - r.created_at_ms >= REQUEST_TTL_MS {
                drop_ids.push((r.id.clone(), Some("expired")));
            }
        }
        for (id, why) in drop_ids {
            if let Some(r) = g.requests.remove(&id)
                && let Some(why) = why
            {
                r.decision.send_replace(Some(Outcome::Withdrawn(why)));
                ended.push((r, why));
            }
        }
    }
    for (r, why) in ended {
        announce(
            server,
            "auth.approval_withdrawn",
            server_actor(),
            subject(&r.pane, &r.id),
            json!({"method": r.frozen.method, "reason": why}),
        );
    }
}

/// Revocation of `pane`'s token (`auth.revoke_token`): its open requests are withdrawn and its
/// standing grants end. Returns (requests withdrawn, grants removed).
pub fn clear_pane(server: &Server, pane: &str, actor: Value) -> (usize, usize) {
    let (ended, grants) = {
        let mut g = state(server).inner.lock().unwrap();
        let before = g.grants.len();
        g.grants.retain(|gr| gr.pane != pane);
        let grants = before - g.grants.len();
        let ids: Vec<String> = g
            .requests
            .values()
            .filter(|r| r.pane == pane && r.decision.borrow().is_none() && !r.running)
            .map(|r| r.id.clone())
            .collect();
        let mut ended = Vec::new();
        for id in ids {
            if let Some(r) = g.requests.remove(&id) {
                r.decision.send_replace(Some(Outcome::Withdrawn("revoked")));
                ended.push(r);
            }
        }
        (ended, grants)
    };
    let n = ended.len();
    for r in ended {
        announce(
            server,
            "auth.approval_withdrawn",
            actor.clone(),
            subject(&r.pane, &r.id),
            json!({"method": r.frozen.method, "reason": "revoked"}),
        );
    }
    (n, grants)
}

/// A connection closed (`run::ConnGuard`): requests only it was waiting on are withdrawn (the
/// CLI's Ctrl-C).
pub fn client_gone(server: &Server, client_id: &str) {
    let ended = {
        let mut g = state(server).inner.lock().unwrap();
        let ids: Vec<String> = g
            .requests
            .values_mut()
            .filter_map(|r| {
                let before = r.waiters.len();
                r.waiters.retain(|w| w != client_id);
                let was_waiting = r.waiters.len() < before;
                (was_waiting && r.waiters.is_empty() && !r.running && r.decision.borrow().is_none())
                    .then(|| r.id.clone())
            })
            .collect();
        let mut ended = Vec::new();
        for id in ids {
            if let Some(r) = g.requests.remove(&id) {
                r.decision
                    .send_replace(Some(Outcome::Withdrawn("disconnected")));
                ended.push(r);
            }
        }
        ended
    };
    for r in ended {
        announce(
            server,
            "auth.approval_withdrawn",
            server_actor(),
            subject(&r.pane, &r.id),
            json!({"method": r.frozen.method, "reason": "disconnected"}),
        );
    }
}

/// `auth.list`: open requests (`approvals`) and standing grants (`grants`).
pub fn list_json(server: &Server) -> (Vec<Value>, Vec<Value>) {
    prune(server);
    let g = state(server).inner.lock().unwrap();
    let mut open: Vec<Value> = g
        .requests
        .values()
        .filter(|r| r.decision.borrow().is_none())
        .map(request_json)
        .collect();
    open.sort_by_key(|v| v["created_at_ms"].as_i64());
    let grants = g
        .grants
        .iter()
        .map(|gr| {
            json!({"pane": gr.pane, "method": gr.method, "target": gr.target, "peer": gr.peer,
                   "peer_name": gr.peer_name, "request": gr.request, "created_at_ms": gr.created_at_ms})
        })
        .collect();
    (open, grants)
}

// ---------------------------------------------------------------------------------------------
// Running an approved call.

/// Run an approved call as the user. Every approved call goes through here, once.
async fn run_approved(server: &Arc<Server>, approver: &Ctx, r: &Request) -> R {
    let ctx = Ctx {
        client_id: approver.client_id.clone(),
        kind: approver.kind.clone(),
        pane_scope: None,
        remote: approver.remote,
    };
    let f = &r.frozen;
    match f.method.as_str() {
        "handoff.send" => {
            let Some((root, branch, head)) = f.repo.clone() else {
                return Err(err(
                    ErrorKind::Internal,
                    "approved send without repository facts",
                ));
            };
            // The pane's repository must still be the one the user approved.
            let pane = crate::api::resolve_pane(server, &ctx, s(&f.params, "pane"))?;
            let now = match cwd_of(server, &pane) {
                Some(cwd) => crate::git_api::repo_facts(std::path::Path::new(&cwd))
                    .await
                    .ok()
                    .map(|(root, branch, head, _)| (root.display().to_string(), branch, head)),
                None => None,
            };
            if now.as_ref() != Some(&(root.clone(), branch.clone(), head.clone())) {
                let short = |h: &str| h.chars().take(12).collect::<String>();
                let at = |r: &str, b: &Option<String>, h: &str| {
                    format!(
                        "{r} on {} at {}",
                        b.as_deref().unwrap_or("a detached HEAD"),
                        short(h)
                    )
                };
                let found = now
                    .as_ref()
                    .map(|(r, b, h)| at(r, b, h))
                    .unwrap_or_else(|| "no repository".into());
                return Err(err(
                    ErrorKind::Conflict,
                    format!(
                        "repo_moved: the pane's repository changed since the handoff was requested (requested: {}; now: {found}); nothing was sent, ask again",
                        at(&root, &branch, &head)
                    ),
                )
                .details(json!({"reason": "repo_moved", "request": r.id})));
            }
            let expect = crate::handoff_out::Expect {
                repo_root: root,
                branch,
                head,
                request: r.id.clone(),
                requested_by: r.pane.clone(),
                approved_by: approver.kind.clone(),
            };
            crate::handoff_out::send_job(server, &ctx, &f.params, Some(expect))
        }
        "handoff.cancel" => crate::handoff_out::cancel(server, &f.params),
        "gateway.call" => {
            let method = s(&f.params, "method").unwrap_or("");
            let params = f.params.get("params").cloned().unwrap_or(Value::Null);
            crate::gateway_bridge::call(server, method, params, GATEWAY_TIMEOUT).await
        }
        other => Err(invalid(format!("{other} can't be approved"))),
    }
}

/// A short form of an approved call's result for events and the audit log.
fn result_brief(method: &str, v: &Value) -> Value {
    match method {
        "handoff.send" | "handoff.cancel" => {
            json!({"job": v.pointer("/job/id"), "state": v.pointer("/job/state")})
        }
        "gateway.call" => json!({"peer": v.pointer("/peer/id"), "name": v.pointer("/peer/name")}),
        _ => Value::Null,
    }
}

/// Run the call of a standing grant at once (no request to decide).
async fn run_standing(server: &Arc<Server>, ctx: &Ctx, r: Request, grant: &Grant) -> R {
    let pane = r.pane.clone();
    let id = r.id.clone();
    let actor = json!({"kind": "user", "grant": grant.request, "client_kind": "standing_grant"});
    let user = Ctx {
        client_id: ctx.client_id.clone(),
        kind: format!("standing_grant:{}", grant.request),
        pane_scope: None,
        remote: false,
    };
    let out = run_approved(server, &user, &r).await;
    let (ok, error) = match &out {
        Ok(_) => (true, Value::Null),
        Err(e) => (false, json!(e.message)),
    };
    announce(
        server,
        "auth.approval_granted",
        actor,
        subject(&pane, &id),
        json!({"method": r.frozen.method, "grant": "standing", "ok": ok, "error": error,
               "summary": r.frozen.summary, "standing_grant": grant.request,
               "result": out.as_ref().map(|v| result_brief(&r.frozen.method, v)).unwrap_or(Value::Null)}),
    );
    if ok {
        server.notify(
            "auth.approve",
            Some(&pane),
            &format!("Pane {} used its standing approval", r.pane_handle),
            &format!(
                "{}\nAllowed until the pane restarts (`vibeke pane revoke-token {}` ends it now).",
                r.frozen.summary, r.pane_handle
            ),
            "normal",
        );
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The methods.

async fn approve(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let Some(me) = ctx.pane_scope.clone() else {
        return Err(invalid(
            "auth.approve is for callers inside a pane; this connection can call the method itself",
        ));
    };
    prune(server);
    let wait_ms = u(p, "timeout_ms").unwrap_or(DEFAULT_WAIT_MS).min(600_000);
    let wait = p.get("wait").and_then(Value::as_bool) != Some(false);
    if let Some(id) = s(p, "request") {
        // Resume waiting on an earlier request of this pane.
        let rx = {
            let g = state(server).inner.lock().unwrap();
            let r = g
                .requests
                .get(id)
                .filter(|r| r.pane == me)
                .ok_or_else(|| not_found("approval request", id))?;
            if !wait {
                return Ok(request_json(r));
            }
            r.decision.subscribe()
        };
        return wait_for(server, ctx, id, rx, wait_ms).await;
    }
    let method = req(p, "method")?;
    let frozen = prepare(server, ctx, &me, method, p).await?;
    let (handle, workspace, child_pid) = server
        .with_core(|c| {
            c.pane(&me)
                .map(|x| (x.handle.clone(), x.workspace.clone(), x.child_pid))
        })
        .ok_or_else(|| not_found("pane", &me))?;
    let reason: String = s(p, "reason")
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(MAX_REASON)
        .collect();
    let (tx, rx) = watch::channel(None);
    let r = Request {
        id: format!("ap-{}", &ulid()[16..]),
        pane: me.clone(),
        pane_handle: handle.clone(),
        workspace,
        child_pid,
        frozen,
        reason,
        created_at_ms: now_ms(),
        running: false,
        waiters: Vec::new(),
        decision: tx,
    };
    // A standing grant for exactly this (pane, method, target, peer) runs it now.
    let standing = {
        let g = state(server).inner.lock().unwrap();
        g.grants
            .iter()
            .find(|gr| {
                gr.pane == me
                    && gr.child_pid == child_pid
                    && gr.method == r.frozen.method
                    && gr.target == r.frozen.target
                    && r.frozen.peer.as_ref().is_some_and(|p| p.id == gr.peer)
            })
            .cloned()
    };
    if let Some(grant) = standing {
        return run_standing(server, ctx, r, &grant).await;
    }
    let id = r.id.clone();
    {
        let mut g = state(server).inner.lock().unwrap();
        let open = g
            .requests
            .values()
            .filter(|x| x.pane == me && x.decision.borrow().is_none())
            .count();
        if open >= crate::auth::MAX_OPEN_PER_PANE {
            return Err(err(
                ErrorKind::RateLimited,
                "too many open approval requests from this pane",
            ));
        }
        g.requests.insert(id.clone(), r.clone());
    }
    announce(
        server,
        "auth.approval_requested",
        crate::audit::actor_of(ctx),
        subject(&me, &id),
        json!({"method": r.frozen.method, "summary": r.frozen.summary,
               "reason": vk_redact::redact(&r.reason), "peer": r.frozen.peer.as_ref().map(|p| p.id.clone()),
               "always_allowed": r.frozen.always_allowed}),
    );
    let body = format!(
        "{}{}\nReview it in Vibeke (prefix+shift+e) or outside the pane: vibeke auth approval {id} approve|deny{}",
        r.frozen.summary,
        if r.reason.is_empty() {
            String::new()
        } else {
            format!("\nThe pane says (unverified): {}", r.reason)
        },
        if r.frozen.always_allowed {
            "|always"
        } else {
            ""
        }
    );
    server.notify(
        "auth.approve",
        Some(&me),
        &format!("Pane {handle} asks for approval"),
        &body,
        "high",
    );
    if !wait {
        let g = state(server).inner.lock().unwrap();
        return Ok(g.requests.get(&id).map(request_json).unwrap_or(Value::Null));
    }
    wait_for(server, ctx, &id, rx, wait_ms).await
}

/// Removes this waiter from the request however the wait ends.
struct Waiting<'a> {
    server: &'a Server,
    id: String,
    client: String,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut g = state(self.server).inner.lock().unwrap();
        if let Some(r) = g.requests.get_mut(&self.id)
            && let Some(i) = r.waiters.iter().position(|w| *w == self.client)
        {
            r.waiters.remove(i);
        }
    }
}

async fn wait_for(
    server: &Arc<Server>,
    ctx: &Ctx,
    id: &str,
    mut rx: watch::Receiver<Option<Outcome>>,
    wait_ms: u64,
) -> R {
    {
        let mut g = state(server).inner.lock().unwrap();
        if let Some(r) = g.requests.get_mut(id) {
            r.waiters.push(ctx.client_id.clone());
        }
    }
    let guard = Waiting {
        server: server.as_ref(),
        id: id.to_string(),
        client: ctx.client_id.clone(),
    };
    let decided = tokio::time::timeout(Duration::from_millis(wait_ms), async {
        loop {
            if let Some(d) = rx.borrow_and_update().clone() {
                return d;
            }
            if rx.changed().await.is_err() {
                return Outcome::Withdrawn("withdrawn");
            }
        }
    })
    .await;
    drop(guard);
    let outcome = match decided {
        Err(_) => {
            return Err(err(
                ErrorKind::Timeout,
                format!("no decision on approval request {id} yet; call auth.approve {{request: \"{id}\"}} to keep waiting"),
            )
            .details(json!({"request": id})));
        }
        Ok(o) => o,
    };
    // Collected: the request is done.
    state(server).inner.lock().unwrap().requests.remove(id);
    match outcome {
        Outcome::Ran(r) => r,
        Outcome::Denied => Err(denied(format!(
            "approval_denied: the user denied request {id}"
        ))),
        Outcome::Withdrawn(why) => Err(err(
            if why == "revoked" {
                ErrorKind::PermissionDenied
            } else {
                ErrorKind::Conflict
            },
            format!("approval_withdrawn: request {id} ended ({why})"),
        )
        .details(json!({"request": id, "reason": why}))),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Decision {
    Once,
    Always,
    Deny,
}

async fn decide(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "request")?;
    let decision = match s(p, "decision").unwrap_or("") {
        "approve" | "approved" | "allow" | "once" => Decision::Once,
        "always" => Decision::Always,
        "deny" | "denied" | "reject" => Decision::Deny,
        _ => return Err(invalid("decision must be approve, always or deny")),
    };
    let pane = {
        let g = state(server).inner.lock().unwrap();
        g.requests
            .get(id)
            .map(|r| r.pane.clone())
            .ok_or_else(|| not_found("approval request", id))?
    };
    let pid_now = server.with_core(|c| c.pane(&pane).map(|x| x.child_pid));
    // Claim it: exactly one decision, and an approved call runs once.
    let r = {
        let mut g = state(server).inner.lock().unwrap();
        let (busy, always_ok, pid, method) = {
            let r = g
                .requests
                .get(id)
                .ok_or_else(|| not_found("approval request", id))?;
            (
                r.running || r.decision.borrow().is_some(),
                r.frozen.always_allowed,
                r.child_pid,
                r.frozen.method.clone(),
            )
        };
        if busy {
            return Err(err(ErrorKind::Conflict, "approval request already decided"));
        }
        if decision == Decision::Always && !always_ok {
            return Err(invalid(format!(
                "{method} can only be approved once, not always"
            )));
        }
        if pid_now != Some(pid) {
            let r = g.requests.remove(id).expect("present");
            drop(g);
            r.decision
                .send_replace(Some(Outcome::Withdrawn("pane_restarted")));
            announce(
                server,
                "auth.approval_withdrawn",
                crate::audit::actor_of(ctx),
                subject(&r.pane, &r.id),
                json!({"method": r.frozen.method, "reason": "pane_restarted"}),
            );
            return Err(err(
                ErrorKind::Conflict,
                "the pane restarted since it asked; the request is withdrawn",
            ));
        }
        let r = g.requests.get_mut(id).expect("present");
        if decision == Decision::Deny {
            r.decision.send_replace(Some(Outcome::Denied));
        } else {
            r.running = true;
        }
        r.clone()
    };
    if decision == Decision::Deny {
        announce(
            server,
            "auth.approval_denied",
            crate::audit::actor_of(ctx),
            subject(&r.pane, &r.id),
            json!({"method": r.frozen.method, "summary": r.frozen.summary}),
        );
        return Ok(
            json!({"request": id, "pane": r.pane, "decision": "denied", "grant": null,
                          "ok": false, "result": null, "error": null}),
        );
    }
    let grant = if decision == Decision::Always {
        let peer = r.frozen.peer.clone().unwrap_or(Peer {
            id: String::new(),
            name: String::new(),
            owner: String::new(),
        });
        let gr = Grant {
            pane: r.pane.clone(),
            child_pid: r.child_pid,
            method: r.frozen.method.clone(),
            target: r.frozen.target.clone(),
            peer: peer.id,
            peer_name: peer.name,
            request: r.id.clone(),
            created_at_ms: now_ms(),
        };
        state(server).inner.lock().unwrap().grants.push(gr);
        "always"
    } else {
        "once"
    };
    let out = run_approved(server, ctx, &r).await;
    {
        let mut g = state(server).inner.lock().unwrap();
        match g.requests.get_mut(id) {
            Some(x) => {
                x.running = false;
                x.decision.send_replace(Some(Outcome::Ran(out.clone())));
            }
            // Revoked or withdrawn while running: still tell a waiter what happened.
            None => {
                r.decision.send_replace(Some(Outcome::Ran(out.clone())));
            }
        }
    }
    let mut actor = crate::audit::actor_of(ctx);
    actor["for_pane"] = json!(r.pane);
    let (ok, error) = match &out {
        Ok(_) => (true, Value::Null),
        Err(e) => (false, json!(e.message)),
    };
    announce(
        server,
        "auth.approval_granted",
        actor,
        subject(&r.pane, &r.id),
        json!({"method": r.frozen.method, "grant": grant, "ok": ok, "error": error,
               "summary": r.frozen.summary,
               "result": out.as_ref().map(|v| result_brief(&r.frozen.method, v)).unwrap_or(Value::Null)}),
    );
    let (result, error) = match out {
        Ok(v) => (v, Value::Null),
        Err(e) => (Value::Null, serde_json::to_value(&e).unwrap_or(Value::Null)),
    };
    Ok(
        json!({"request": id, "pane": r.pane, "decision": "approved", "grant": grant,
              "ok": ok, "result": result, "error": error}),
    )
}

fn withdraw(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let Some(me) = ctx.pane_scope.clone() else {
        return Err(invalid(
            "auth.approve.withdraw is for the asking pane; deny the request with auth.approve.decide",
        ));
    };
    let id = req(p, "request")?;
    let r = {
        let mut g = state(server).inner.lock().unwrap();
        let r = g
            .requests
            .get(id)
            .filter(|r| r.pane == me)
            .ok_or_else(|| not_found("approval request", id))?;
        if r.running || r.decision.borrow().is_some() {
            return Err(err(ErrorKind::Conflict, "approval request already decided"));
        }
        let r = g.requests.remove(id).expect("present");
        r.decision
            .send_replace(Some(Outcome::Withdrawn("withdrawn")));
        r
    };
    announce(
        server,
        "auth.approval_withdrawn",
        crate::audit::actor_of(ctx),
        subject(&r.pane, &r.id),
        json!({"method": r.frozen.method, "reason": "withdrawn"}),
    );
    Ok(json!({"request": id, "withdrawn": true}))
}

/// Tests: whether `pane` holds a standing grant.
#[cfg(test)]
pub fn grants_of(server: &Server, pane: &str) -> usize {
    state(server)
        .inner
        .lock()
        .unwrap()
        .grants
        .iter()
        .filter(|g| g.pane == pane)
        .count()
}

/// Definitions for the schema registry (`api_schema` loads them with its own).
pub const DEFS: &str = r##"
ApprovalRequest = {request: string, kind: approval, pane: string, pane_handle: string, workspace: string, method: "handoff.send"|"handoff.cancel"|"gateway.call", params: object, summary: string, facts: object, reason: string, reason_verified: bool, peer: {id: string, name: string, owner: string}|null, always_allowed: bool, created_at_ms: int, status: pending|running|approved|failed|denied|withdrawn}
ApprovalGrant = {pane: string, method: string, target: string|null, peer: string, peer_name: string, request: string, created_at_ms: int}
"##;

/// Method shapes (`api_schema` loads them next to its own tables).
pub const SHAPES: &str = r##"
# --- approved calls (09 §3.2): a pane asks, the user decides outside it ---
# pane scope only: handoff.send, handoff.cancel (the pane's own jobs) or gateway.call {method: peer.redeem}; waits for the decision and returns the target method's result; `wait: false` returns the request; `request` resumes waiting; a standing grant runs it at once
auth.approve :: {method?: "handoff.send"|"handoff.cancel"|"gateway.call", params?: object, reason?: string, wait?: bool = true, timeout_ms?: int = 120000, request?: string}
  => ApprovalRequest | object
# full scope only, never from a pane or an elevated connection; approve runs the frozen call once as the caller; always also grants (pane, method, target pane, peer) until the pane restarts
auth.approve.decide :: {request: string, decision: approve|always|deny}
  => {request: string, pane: string, decision: approved|denied, grant: once|always|null, ok: bool, result: any, error: RpcError|null}
# the asking pane only
auth.approve.withdraw :: {request: string} => {request: string, withdrawn: bool}
"##;

pub const EVENTS: &str = r##"
auth.approval_requested :: {pane: string, request: string} => {method: string, summary: string, reason: string, peer: string|null, always_allowed: bool}
auth.approval_granted :: {pane: string, request: string} => {method: string, grant: once|always|standing, ok: bool, error: string|null, summary: string, result: any, standing_grant?: string}
auth.approval_denied :: {pane: string, request: string} => {method: string, summary: string}
auth.approval_withdrawn :: {pane: string, request: string} => {method: string, reason: withdrawn|disconnected|revoked|pane_restarted|expired}
"##;
