//! Cloud moves (spec 17 §7): an agent's work sent from this host to a cloud box, or brought back
//! from a box to this host or to a paired host. Each move is a server job.
//!
//! - `cloud.move {pane?|run?|box?, to, interrupt?, source_after?}` records a `queued` job and
//!   runs it as a tokio task in the server. `to` is `{kind: "cloud", provider?, box?}` (send),
//!   `{kind: "local"}` or `{kind: "peer", peer}` (bring back).
//! - `cloud.jobs` lists the jobs, newest first; finished ones for 7 days.
//! - `cloud.cancel {id}` marks a job `cancelled`; the runner stops before its next step.
//!
//! Every change of a job is committed to kv `cloud_job/<id>` and emitted as `cloud.job` with the
//! whole record. A failed move leaves the source as it was: the source pane closes only after the
//! agent runs at the destination.
//!
//! **Send**: wait for the turn boundary (or interrupt), make sure the pane has a task (a pane
//! without one adopts its checkout with `task.adopt`) and the task a box
//! (`sandbox::cloud::ensure_for_task`), push the task branch to the box (`task.sync`), export on
//! the host with `vk_handoff::export`, upload the bundle to `/vibeke/in/<job>.tar.zst`, import it
//! in place in the box (`vibeke sandbox import-bundle`), open a box pane in the task, resume the
//! agent there and close the source pane.
//!
//! **Bring back**: wait for the turn boundary in the box pane, export in the box
//! (`vibeke sandbox export-bundle`, the bundle on stdout), then
//! - local: `task.sync` pull, import with `handoff::import_local` into the task's host checkout
//!   (in place, or a new worktree), release the box (`source_after`, default `[cloud]
//!   after_bring_back`), switch the task to `host` and resume the agent in a host pane;
//! - peer: hand the bundle to the gateway as a `handoff.send` job with `bundle` set, wait for the
//!   delivery, then close the box pane and release the box.
//!
//! Jobs left unfinished by an earlier server process are marked `failed` the first time a
//! `cloud.*` method of this module is called.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vk_proto::layout::Direction;
use vk_proto::model::{AgentRun, Execution, Isolation, Pane};
use vk_proto::rpc::{ErrorKind, RpcError};

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s};
use crate::core::Tx;

pub const K_JOB: &str = "cloud_job";

pub const METHODS: &[(&str, bool)] = &[
    ("cloud.move", true),
    ("cloud.jobs", false),
    ("cloud.cancel", true),
];

/// Every `cloud.*` method is the user's (spec 17 §10); a pane asks for a move through
/// `auth.approve`.
pub const PANE_FORBIDDEN: &[&str] = &["cloud.move", "cloud.jobs", "cloud.cancel"];

/// In pipeline order; the last three are terminal.
pub const STATES: &[&str] = &[
    "queued",
    "waiting_turn",
    "creating",
    "bootstrapping",
    "exporting",
    "uploading",
    "importing",
    "resuming",
    "done",
    "failed",
    "cancelled",
];

pub const AFTER: &[&str] = &["keep", "suspend", "destroy"];

/// Finished jobs stay listed this long (seconds).
pub const KEEP_FINISHED_S: u64 = 7 * 86_400;
/// At most this many jobs are kept.
pub const MAX_JOBS: usize = 200;
const MAX_ERROR: usize = 500;
/// The box's import and export.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long a move waits for the agent to finish its turn.
const TURN_WAIT: Duration = Duration::from_secs(2 * 3600);
/// How long an interrupted agent gets to stop.
const INTERRUPT_WAIT: Duration = Duration::from_secs(30);
/// How long a bring-back to a peer waits for the gateway's delivery.
const PEER_WAIT: Duration = Duration::from_secs(6 * 3600);
/// Where uploads land in the box.
const BOX_IN: &str = "/vibeke/in";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    /// `send` | `bring_back`.
    pub direction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// `<provider>/<id>`: the box the work goes to or comes from.
    #[serde(rename = "box", default, skip_serializing_if = "Option::is_none")]
    pub box_id: Option<String>,
    /// `{kind: "local"}` or `{kind: "cloud", provider, box}`.
    pub from: Value,
    /// As asked: `{kind: "cloud", provider?, box?}`, `{kind: "local"}`, `{kind: "peer", peer}`.
    pub to: Value,
    /// One of [`STATES`].
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// `{kind, message, details?}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// `{pane?, task?, box?, peer_job?, ...}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default)]
    pub interrupt: bool,
    /// Bring back: what happens to the box afterwards (`keep` | `suspend` | `destroy`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_after: Option<String>,
    /// The task the move works on, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Who started it (a client kind, or `pane:<id>` for a move approved from a pane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
}

/// Where a move goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    Cloud {
        provider: Option<String>,
        box_id: Option<String>,
    },
    Local,
    Peer(String),
}

impl Dest {
    pub fn parse(v: &Value) -> Result<Dest, RpcError> {
        let kind = match v {
            Value::String(k) => k.as_str(),
            Value::Object(_) => s(v, "kind").ok_or_else(|| invalid("to.kind is required"))?,
            _ => return Err(invalid("to must be {kind: cloud|local|peer, ...}")),
        };
        let opt = |k: &str| s(v, k).filter(|x| !x.is_empty()).map(str::to_string);
        match kind {
            "cloud" => Ok(Dest::Cloud {
                provider: opt("provider"),
                box_id: opt("box"),
            }),
            "local" => Ok(Dest::Local),
            "peer" => opt("peer")
                .map(Dest::Peer)
                .ok_or_else(|| invalid("to.peer is required for kind peer")),
            other => Err(invalid(format!(
                "unknown destination kind {other}; use cloud, local or peer"
            ))),
        }
    }

    pub fn json(&self) -> Value {
        match self {
            Dest::Cloud { provider, box_id } => {
                let mut v = json!({"kind": "cloud"});
                if let Some(p) = provider {
                    v["provider"] = json!(p);
                }
                if let Some(b) = box_id {
                    v["box"] = json!(b);
                }
                v
            }
            Dest::Local => json!({"kind": "local"}),
            Dest::Peer(p) => json!({"kind": "peer", "peer": p}),
        }
    }

    pub fn direction(&self) -> &'static str {
        match self {
            Dest::Cloud { .. } => "send",
            _ => "bring_back",
        }
    }
}

pub fn terminal(state: &str) -> bool {
    matches!(state, "done" | "failed" | "cancelled")
}

fn rank(state: &str) -> Option<usize> {
    STATES.iter().position(|s| *s == state)
}

/// Whether a job in `from` may move to `to`: forward through the pipeline (steps may be
/// skipped), `done` from any started state, `failed` or `cancelled` from any unfinished one. A
/// finished job never changes.
pub fn transition_ok(from: &str, to: &str) -> bool {
    let (Some(f), Some(t)) = (rank(from), rank(to)) else {
        return false;
    };
    if terminal(from) {
        return false;
    }
    match to {
        "failed" | "cancelled" => true,
        "done" => from != "queued",
        "queued" => false,
        _ => t > f,
    }
}

fn now_s() -> u64 {
    (vk_store::now_ms() / 1000).max(0) as u64
}

fn job_json(job: &Job) -> Value {
    serde_json::to_value(job).unwrap_or(Value::Null)
}

fn put(tx: &mut Tx, job: &Job) {
    tx.m.put(K_JOB, &job.id, None, job);
    tx.event("cloud.job", json!({"job": job.id}), job_json(job));
}

fn conflict(msg: impl Into<String>, job: &Job) -> RpcError {
    err(ErrorKind::Conflict, msg).details(json!({"job": job.id, "state": job.state}))
}

fn cancelled_err(id: &str) -> RpcError {
    err(ErrorKind::Conflict, "the move was cancelled")
        .details(json!({"job": id, "state": "cancelled", "reason": "cancelled"}))
}

/// `{kind, message, details?}` of an error, for the job record.
pub fn error_json(e: &RpcError) -> Value {
    let message: String = e.message.chars().take(MAX_ERROR).collect();
    let mut v = json!({"kind": e.data.kind, "message": message});
    if !e.data.details.is_null() {
        v["details"] = e.data.details.clone();
    }
    v
}

fn herr(e: vk_handoff::Error) -> RpcError {
    let kind = match e.kind {
        "conflict" | "too_large" => ErrorKind::Conflict,
        "timeout" => ErrorKind::Timeout,
        "invalid_params" => ErrorKind::InvalidParams,
        "not_found" => ErrorKind::NotFound,
        "unsupported" => ErrorKind::Unsupported,
        _ => ErrorKind::Internal,
    };
    let reason = e.kind;
    err(kind, e.message).details(json!({"reason": reason}))
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !METHODS.iter().any(|(m, _)| *m == method) {
        return None;
    }
    recover(server);
    Some(match method {
        "cloud.move" => start_move(server, ctx, p, None),
        "cloud.jobs" => Ok(json!({"jobs": list(server).iter().map(job_json).collect::<Vec<_>>()})),
        "cloud.cancel" => cancel(server, p),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// records

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

pub(crate) fn get(server: &Server, id: &str) -> Result<Job, RpcError> {
    server
        .with_core(|c| c.store.get::<Job>(K_JOB, id))
        .map_err(internal)?
        .ok_or_else(|| not_found("cloud job", id))
}

/// Read-modify-write one job under the core lock, then emit `cloud.job`. `f` returns whether
/// anything changed.
fn modify(
    server: &Server,
    id: &str,
    f: impl FnOnce(&mut Job) -> Result<bool, RpcError>,
) -> Result<Job, RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut job = c
        .store
        .get::<Job>(K_JOB, id)
        .map_err(internal)?
        .ok_or_else(|| not_found("cloud job", id))?;
    if f(&mut job)? {
        job.updated_at = now_s();
        let mut tx = Tx::new();
        put(&mut tx, &job);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(job)
}

/// Move a running job to `state`; a cancelled job stops here.
fn step(server: &Server, id: &str, state: &str) -> Result<Job, RpcError> {
    modify(server, id, |job| {
        if job.state == "cancelled" {
            return Err(cancelled_err(id));
        }
        if job.state == state {
            return Ok(false);
        }
        if !transition_ok(&job.state, state) {
            return Err(conflict(
                format!("a {} move can't become {state}", job.state),
                job,
            ));
        }
        job.state = state.to_string();
        Ok(true)
    })
}

/// Stop here when the job was cancelled.
fn check_cancel(server: &Server, id: &str) -> Result<(), RpcError> {
    match get(server, id)?.state.as_str() {
        "cancelled" => Err(cancelled_err(id)),
        _ => Ok(()),
    }
}

fn set_progress(server: &Server, id: &str, done: u64, total: u64) {
    let _ = modify(server, id, |job| {
        let p = Some(Progress { done, total });
        if job.progress == p || terminal(&job.state) {
            return Ok(false);
        }
        job.progress = p;
        Ok(true)
    });
}

/// Merge `fields` into the job's `result` (and record the task and box once known).
fn note(server: &Server, id: &str, fields: Value) {
    let _ = modify(server, id, |job| {
        let mut r = job.result.clone().unwrap_or_else(|| json!({}));
        if let (Some(obj), Some(add)) = (r.as_object_mut(), fields.as_object()) {
            for (k, v) in add {
                obj.insert(k.clone(), v.clone());
            }
        }
        if let Some(t) = s(&fields, "task") {
            job.task = Some(t.to_string());
        }
        if let Some(b) = s(&fields, "box") {
            job.box_id = Some(b.to_string());
        }
        job.result = Some(r);
        Ok(true)
    });
}

pub(crate) fn cancel(server: &Server, p: &Value) -> R {
    let id = req(p, "id")?;
    let job = modify(server, id, |job| match job.state.as_str() {
        "cancelled" => Ok(false),
        st if terminal(st) => Err(conflict(format!("the move is already {st}"), job)),
        _ => {
            job.state = "cancelled".into();
            Ok(true)
        }
    })?;
    Ok(json!({"job": job_json(&job)}))
}

/// Jobs this process runs, by state directory and id.
static RUNNING: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// State directories whose leftover jobs were already failed.
static RECOVERED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

fn running_key(server: &Server, id: &str) -> String {
    format!("{}\0{id}", server.paths.state.display())
}

fn is_running(server: &Server, id: &str) -> bool {
    RUNNING
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|m| m.contains(&running_key(server, id)))
}

/// Unfinished jobs of an earlier server process can't continue: they fail (once per state
/// directory). The source of each was left as it was.
fn recover(server: &Server) {
    {
        let mut r = RECOVERED.lock().unwrap();
        if !r
            .get_or_insert_with(HashSet::new)
            .insert(server.paths.state.clone())
        {
            return;
        }
    }
    let jobs = server
        .with_core(|c| c.store.load::<Job>(K_JOB))
        .unwrap_or_default();
    for j in jobs
        .iter()
        .filter(|j| !terminal(&j.state) && !is_running(server, &j.id))
    {
        let _ = modify(server, &j.id, |job| {
            if terminal(&job.state) {
                return Ok(false);
            }
            job.state = "failed".into();
            job.error = Some(
                json!({"kind": "internal", "message": "the server restarted during the move; nothing was moved, try again"}),
            );
            Ok(true)
        });
    }
}

// ---------------------------------------------------------------------------------------------
// starting a move

/// What a move starts from.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub pane: Pane,
    pub run: Option<String>,
    pub task: Option<String>,
    /// `<provider>/<id>` when the pane runs in a cloud box.
    pub box_ref: Option<String>,
}

/// The workspace task of a pane.
fn pane_task(server: &Server, pane: &str) -> Option<String> {
    server.with_core(|c| {
        let p = c.pane(pane)?;
        c.ws(&p.workspace).and_then(|w| w.task.clone())
    })
}

/// `<provider>/<id>` of a task's cloud box, when it has one.
fn box_of_task(server: &Server, task: &str) -> Option<String> {
    let tb = server.sandbox.get(task)?;
    let c = crate::sandbox::cloud::ctx(&tb)?;
    // LANE-B NEEDED: `CloudCtx { provider: String, box_id: String, .. }` (spec 17 §4).
    Some(format!("{}/{}", c.provider, c.box_id))
}

/// The agent run of a pane that has not exited, if any.
fn live_run(server: &Server, pane: &str) -> Option<AgentRun> {
    server.with_core(|c| {
        c.model
            .runs
            .iter()
            .rev()
            .find(|r| r.pane == pane && r.execution.value != Execution::Exited)
            .cloned()
    })
}

/// The pane a move starts from: `box` (a pane of the box's task, the one with an agent first),
/// `run` (its pane) or `pane` (default: the caller's).
pub(crate) fn resolve_source(server: &Server, ctx: &Ctx, p: &Value) -> Result<Source, RpcError> {
    let pane = if let Some(bx) = s(p, "box") {
        let tasks: Vec<String> =
            server.with_core(|c| c.model.tasks.iter().map(|t| t.id.clone()).collect());
        let task = tasks
            .into_iter()
            .find(|t| box_of_task(server, t).as_deref() == Some(bx))
            .ok_or_else(|| {
                err(
                    ErrorKind::NotFound,
                    format!("no task of this host runs in box {bx}"),
                )
                .details(json!({"object": "box", "target": bx}))
            })?;
        let pick = server.with_core(|c| {
            let ws = c.task(&task)?.workspace.clone()?;
            let panes: Vec<&Pane> = c.model.panes.iter().filter(|x| x.workspace == ws).collect();
            panes
                .iter()
                .find(|x| {
                    c.model
                        .runs
                        .iter()
                        .any(|r| r.pane == x.id && r.execution.value != Execution::Exited)
                })
                .or(panes.first())
                .map(|x| (*x).clone())
        });
        pick.ok_or_else(|| {
            err(
                ErrorKind::NotFound,
                format!("box {bx} has no pane on this host"),
            )
        })?
    } else if let Some(r) = s(p, "run") {
        let pane_id = server
            .with_core(|c| c.run(r).map(|x| x.pane.clone()))
            .ok_or_else(|| not_found("run", r))?;
        crate::api::resolve_pane(server, ctx, Some(&pane_id))?
    } else {
        crate::api::resolve_pane(server, ctx, s(p, "pane"))?
    };
    let task = pane_task(server, &pane.id);
    let box_ref = task.as_deref().and_then(|t| box_of_task(server, t));
    let run = live_run(server, &pane.id).map(|r| r.id);
    Ok(Source {
        pane,
        run,
        task,
        box_ref,
    })
}

/// Validate a `cloud.move` request as `cloud.move` itself does (also used by `auth.approve`):
/// the source, the destination, `source_after`, and no other move of the pane under way.
pub(crate) fn check_move(
    server: &Server,
    ctx: &Ctx,
    p: &Value,
) -> Result<(Source, Dest, Option<String>), RpcError> {
    let dest = Dest::parse(p.get("to").unwrap_or(&Value::Null))?;
    let after = match s(p, "source_after") {
        None => None,
        Some(a) if AFTER.contains(&a) => Some(a.to_string()),
        Some(a) => {
            return Err(invalid(format!(
                "source_after {a}: use keep, suspend or destroy"
            )));
        }
    };
    let src = resolve_source(server, ctx, p)?;
    match (&dest, &src.box_ref) {
        (Dest::Cloud { .. }, Some(b)) => {
            return Err(err(
                ErrorKind::Conflict,
                format!("pane {} already runs in cloud box {b}", src.pane.handle),
            )
            .details(json!({"reason": "already_in_cloud", "box": b})));
        }
        (Dest::Local | Dest::Peer(_), None) => {
            return Err(invalid(format!(
                "pane {} does not run in a cloud box",
                src.pane.handle
            )));
        }
        _ => {}
    }
    let jobs = server
        .with_core(|c| c.store.load::<Job>(K_JOB))
        .map_err(internal)?;
    if let Some(busy) = jobs
        .iter()
        .find(|j| j.pane.as_deref() == Some(src.pane.id.as_str()) && !terminal(&j.state))
    {
        return Err(conflict(
            "a move of this pane is already under way; cancel it first",
            busy,
        ));
    }
    Ok((src, dest, after))
}

/// `cloud.move`: record the job and start it. `by` names a pane whose request the user
/// approved (`auth.approve`).
pub(crate) fn start_move(server: &Arc<Server>, ctx: &Ctx, p: &Value, by: Option<String>) -> R {
    let (src, dest, after) = check_move(server, ctx, p)?;
    let now = now_s();
    let from = match &src.box_ref {
        Some(b) => {
            let provider = b.split('/').next().unwrap_or_default();
            json!({"kind": "cloud", "provider": provider, "box": b})
        }
        None => json!({"kind": "local"}),
    };
    let job = Job {
        id: crate::core::ulid(),
        direction: dest.direction().into(),
        pane: Some(src.pane.id.clone()),
        run: src.run.clone(),
        box_id: src.box_ref.clone().or(match &dest {
            Dest::Cloud { box_id, .. } => box_id.clone(),
            _ => None,
        }),
        from,
        to: dest.json(),
        state: "queued".into(),
        progress: None,
        error: None,
        result: None,
        interrupt: b(p, "interrupt").unwrap_or(false),
        source_after: after,
        task: src.task.clone(),
        by: Some(by.unwrap_or_else(|| ctx.kind.clone())),
        created_at: now,
        updated_at: now,
    };
    {
        let mut c = server.core.lock().unwrap();
        let jobs = c.store.load::<Job>(K_JOB).map_err(internal)?;
        if let Some(busy) = jobs
            .iter()
            .find(|j| j.pane == job.pane && !terminal(&j.state))
        {
            return Err(conflict(
                "a move of this pane is already under way; cancel it first",
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
    }
    spawn(server, job.id.clone());
    Ok(json!({"job": job_json(&job)}))
}

fn spawn(server: &Arc<Server>, id: String) {
    let key = running_key(server, &id);
    if !RUNNING
        .lock()
        .unwrap()
        .get_or_insert_with(HashSet::new)
        .insert(key.clone())
    {
        return;
    }
    let srv = server.clone();
    tokio::spawn(async move {
        let out = run_job(&srv, &id).await;
        finish(&srv, &id, out);
        if let Some(m) = RUNNING.lock().unwrap().as_mut() {
            m.remove(&key);
        }
    });
}

/// Record the outcome: `done` with the result, or `failed` with the error (a cancelled job
/// stays cancelled).
fn finish(server: &Server, id: &str, out: R) {
    let r = modify(server, id, |job| {
        if terminal(&job.state) {
            return Ok(false);
        }
        match &out {
            Ok(result) => {
                let mut r = job.result.clone().unwrap_or_else(|| json!({}));
                if let (Some(obj), Some(add)) = (r.as_object_mut(), result.as_object()) {
                    for (k, v) in add {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                job.result = Some(r);
                job.state = "done".into();
                if let Some(p) = job.progress.as_mut() {
                    p.done = p.total;
                }
            }
            Err(e) => {
                job.state = "failed".into();
                job.error = Some(error_json(e));
            }
        }
        Ok(true)
    });
    match (&out, r) {
        (Err(e), _) => tracing::warn!(job = %id, "cloud move failed: {}", e.message),
        (Ok(_), Err(e)) => {
            tracing::warn!(job = %id, "cloud move: recording the outcome: {}", e.message)
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// running a move

async fn run_job(server: &Arc<Server>, id: &str) -> R {
    let job = get(server, id)?;
    let pane = job
        .pane
        .clone()
        .ok_or_else(|| invalid("the move names no pane"))?;
    match Dest::parse(&job.to)? {
        Dest::Cloud { provider, box_id } => {
            send(server, &job, &pane, provider.as_deref(), box_id.as_deref()).await
        }
        Dest::Local => bring_back_local(server, &job, &pane).await,
        Dest::Peer(peer) => bring_back_peer(server, &job, &pane, &peer).await,
    }
}

/// Wait until the pane's agent finished its turn (or interrupt it with `interrupt`). Returns
/// the agent's run as it is then, or `None` when the pane runs no agent.
async fn turn_boundary(
    server: &Arc<Server>,
    id: &str,
    pane: &str,
    interrupt: bool,
) -> Result<Option<AgentRun>, RpcError> {
    step(server, id, "waiting_turn")?;
    let Some(run) = live_run(server, pane) else {
        return Ok(None);
    };
    let busy = |r: &AgentRun| matches!(r.execution.value, Execution::Working | Execution::Starting);
    if !busy(&run) {
        return Ok(Some(run));
    }
    let deadline = if interrupt {
        crate::orch::call(server, "agent.interrupt", json!({"target": run.id})).await?;
        Instant::now() + INTERRUPT_WAIT
    } else {
        Instant::now() + TURN_WAIT
    };
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        check_cancel(server, id)?;
        let now = server.with_core(|c| c.run(&run.id).cloned());
        match now {
            Some(r) if busy(&r) => {}
            Some(r) => return Ok(Some(r)),
            None => return Ok(None),
        }
        if Instant::now() > deadline {
            return Err(err(
                ErrorKind::Timeout,
                if interrupt {
                    "the agent did not stop within 30 s"
                } else {
                    "the agent did not finish its turn within 2 hours"
                },
            ));
        }
    }
}

/// `<state>/cloud/moves` (private): bundles on their way.
fn moves_dir(server: &Server) -> Result<PathBuf, RpcError> {
    let base = server.paths.state.join("cloud");
    crate::paths::ensure_private_dir(&base).map_err(internal)?;
    let d = base.join("moves");
    crate::paths::ensure_private_dir(&d).map_err(internal)?;
    Ok(d)
}

/// Removes a file however the move ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Text from a process shown in an error: no control characters, bounded.
fn tail(bytes: &[u8]) -> String {
    let t = String::from_utf8_lossy(bytes);
    let t = t.trim();
    let start = t
        .char_indices()
        .rev()
        .nth(MAX_ERROR)
        .map(|(i, _)| i)
        .unwrap_or(0);
    vk_handoff::clean(&t[start..], MAX_ERROR)
}

/// POSIX shell single quotes.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The export facts of the pane's agent.
#[derive(Debug, Default)]
struct AgentFacts {
    harness: Option<String>,
    session_id: Option<String>,
    transcript: Option<PathBuf>,
    /// `resume_argv` without the program name.
    resume_args: Vec<String>,
    last_message: Option<String>,
}

fn agent_facts(run: Option<&AgentRun>) -> AgentFacts {
    match run {
        Some(r) => AgentFacts {
            harness: Some(r.harness.clone()).filter(|h| !h.is_empty()),
            session_id: r.harness_session_id.clone(),
            transcript: r.transcript_path.clone().map(PathBuf::from),
            resume_args: r.resume_argv.iter().skip(1).cloned().collect(),
            last_message: r.last_message.clone(),
        },
        None => AgentFacts::default(),
    }
}

/// Start the agent in `pane` with `resume_argv` (`[harness, args...]`). Without a transcript to
/// resume from it starts fresh with a short note.
async fn resume_in(
    server: &Arc<Server>,
    pane: &str,
    task: Option<&str>,
    resume_argv: &[String],
    resumed: bool,
) -> Result<Option<Value>, RpcError> {
    let Some(harness) = resume_argv.first() else {
        return Ok(None);
    };
    let args: Vec<String> = resume_argv[1..].to_vec();
    let prompt = (!resumed).then_some(
        "This session was moved by Vibeke. The previous transcript could not be carried over; the files are as the last agent left them.",
    );
    let opts = crate::sandbox::LaunchOpts::from_params(&json!({}))?;
    let r =
        crate::agents::start_in_pane_opts(server, pane, harness, None, prompt, &args, task, &opts)
            .await?;
    Ok(Some(r.get("run").cloned().unwrap_or(r)))
}

/// The task of a pane, adopting the pane's checkout as a task when it has none.
async fn task_for(server: &Arc<Server>, pane: &str) -> Result<String, RpcError> {
    if let Some(t) = pane_task(server, pane) {
        return Ok(t);
    }
    let r = crate::orch::call(server, "task.adopt", json!({"pane": pane})).await?;
    r.pointer("/task/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| internal("task.adopt returned no task"))
}

/// Send: host pane → cloud box.
async fn send(
    server: &Arc<Server>,
    job: &Job,
    pane: &str,
    provider: Option<&str>,
    want_box: Option<&str>,
) -> R {
    let id = job.id.as_str();
    let run = turn_boundary(server, id, pane, job.interrupt).await?;
    let cwd = server
        .pane_cwd(pane)
        .ok_or_else(|| err(ErrorKind::NotFound, "the pane has no working directory"))?;

    step(server, id, "creating")?;
    let task = task_for(server, pane).await?;
    note(server, id, json!({"task": task}));
    check_cancel(server, id)?;
    let tb = crate::sandbox::cloud::ensure_for_task(server, &task, provider).await?;
    let c = crate::sandbox::cloud::ctx(&tb)
        .ok_or_else(|| internal("the task's box is not a cloud box"))?;
    // LANE-B NEEDED: `CloudCtx { provider, box_id, workdir, bin, .. }` as plain `String`s.
    let box_ref = format!("{}/{}", c.provider, c.box_id);
    if let Some(w) = want_box
        && w != box_ref
    {
        // LANE-B NEEDED: `ensure_for_task` with a chosen existing box; until then only the
        // task's own box is accepted.
        return Err(err(
            ErrorKind::Unsupported,
            format!("the task's box is {box_ref}; moving into another box ({w}) is not supported"),
        ));
    }
    note(server, id, json!({"box": box_ref}));

    // The box has the task branch from its creation; bring it up to date with the host.
    step(server, id, "bootstrapping")?;
    if let Err(e) = crate::orch::call(
        server,
        "task.sync",
        json!({"task": task, "direction": "push"}),
    )
    .await
    {
        tracing::info!(job = %id, "cloud send: task.sync push: {}", e.message);
    }

    step(server, id, "exporting")?;
    let out = moves_dir(server)?.join(format!("{id}.out.tar.zst"));
    let _ = std::fs::remove_file(&out);
    let _cleanup = Cleanup(out.clone());
    let facts = agent_facts(run.as_ref());
    let input = vk_handoff::ExportInput {
        cwd: PathBuf::from(&cwd),
        pin: None,
        harness: facts.harness,
        session_id: facts.session_id,
        transcript: facts.transcript,
        resume_args: facts.resume_args,
        last_message: facts.last_message,
        source_host: server.opts.machine.clone(),
        source_job: Some(id.to_string()),
        full: false,
    };
    let packed = vk_handoff::export(&input, &out).await.map_err(herr)?;

    step(server, id, "uploading")?;
    set_progress(server, id, 0, packed.size);
    let data = tokio::fs::read(&out).await.map_err(internal)?;
    let remote = format!(
        "{}/{id}.tar.zst",
        crate::sandbox::cloud::in_box(&c.root, BOX_IN)
    );
    crate::sandbox::cloud::upload(server, c, &remote, data, 0o600).await?;
    set_progress(server, id, packed.size, packed.size);

    step(server, id, "importing")?;
    let argv: Vec<String> = vec![
        c.bin.clone(),
        "sandbox".into(),
        "import-bundle".into(),
        "--bundle".into(),
        remote,
        "--workspace".into(),
        c.workdir.clone(),
    ];
    let cap =
        crate::sandbox::cloud::exec_capture(server, c, &argv, Vec::new(), IMPORT_TIMEOUT).await?;
    // LANE-B NEEDED: `Captured { stdout: Vec<u8>, stderr: Vec<u8>, code: i32 }`.
    if cap.code != 0 {
        return Err(err(
            ErrorKind::Conflict,
            format!("the import in the box failed: {}", tail(&cap.stderr)),
        )
        .details(json!({"reason": "box_import_failed", "code": cap.code})));
    }
    let imported: Value = serde_json::from_slice(&cap.stdout)
        .map_err(|e| internal(format!("the box's import printed no result: {e}")))?;
    check_cancel(server, id)?;

    step(server, id, "resuming")?;
    let box_cwd = s(&imported, "cwd")
        .unwrap_or(c.workdir.as_str())
        .to_string();
    let resume_argv: Vec<String> = imported
        .get("resume_argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let resumed = b(&imported, "resumed").unwrap_or(false);
    // A new pane in the task's workspace spawns through the task's box.
    let new_pane = server
        .split_pane(
            pane,
            Direction::Right,
            0.5,
            Some(&cwd),
            None,
            Some(format!(
                "☁ {}",
                box_ref.split('/').next().unwrap_or("cloud")
            )),
            None,
            "cloud.move",
        )
        .map_err(internal)?;
    let started = async {
        let line = format!("cd {}\r", sh_quote(&box_cwd));
        crate::render::write_and_ack(
            server,
            &new_pane.id,
            server.next_internal_input_id(),
            line.into_bytes(),
        )
        .await;
        resume_in(
            server,
            &new_pane.id,
            Some(task.as_str()),
            &resume_argv,
            resumed,
        )
        .await
    }
    .await;
    let run = match started {
        Ok(r) => r,
        Err(e) => {
            server.close_pane(&new_pane.id);
            return Err(e);
        }
    };
    // The agent runs in the box: the source pane goes.
    server.close_pane(pane);
    Ok(json!({
        "pane": new_pane.id,
        "task": task,
        "box": box_ref,
        "run": run.as_ref().and_then(|r| r.get("id")).cloned(),
        "cwd": box_cwd,
        "resumed": resumed,
        "not_written": imported.get("not_written").cloned().unwrap_or(json!([])),
        "skipped": imported.get("skipped").cloned().unwrap_or(json!([])),
    }))
}

/// Bring back, first half: the box pane's work exported in the box into a local file.
async fn export_from_box(
    server: &Arc<Server>,
    job: &Job,
    pane: &str,
) -> Result<(PathBuf, String), RpcError> {
    let id = job.id.as_str();
    let run = turn_boundary(server, id, pane, job.interrupt).await?;
    let task = pane_task(server, pane)
        .ok_or_else(|| invalid("the pane belongs to no task, so it runs in no cloud box"))?;
    note(server, id, json!({"task": task}));
    let tb = server
        .sandbox
        .get(&task)
        .ok_or_else(|| err(ErrorKind::Conflict, "the task's box is not attached"))?;
    let c = crate::sandbox::cloud::ctx(&tb)
        .ok_or_else(|| invalid("the pane's task does not run in a cloud box"))?;

    step(server, id, "exporting")?;
    // The pane reports its cwd in the box (OSC 7); anything else means the box's workspace.
    let box_cwd = server
        .pane_cwd(pane)
        .filter(|p| Path::new(p).starts_with(&c.workdir))
        .unwrap_or_else(|| c.workdir.clone());
    let mut argv: Vec<String> = vec![
        c.bin.clone(),
        "sandbox".into(),
        "export-bundle".into(),
        "--cwd".into(),
        box_cwd,
        "--source-host".into(),
        // LANE-B NEEDED: `CloudCtx.name` (the box name, `vk-<host8>-<key10>`).
        c.name.clone(),
    ];
    // The box finds the transcript itself from the harness and session.
    let facts = agent_facts(run.as_ref());
    if let Some(h) = facts.harness {
        argv.extend(["--harness".into(), h]);
    }
    if let Some(sid) = facts.session_id {
        argv.extend(["--session".into(), sid]);
    }
    for a in facts.resume_args {
        argv.extend(["--resume-arg".into(), a]);
    }
    // LANE-B NEEDED: a size cap on `exec_capture`'s stdout (here MAX_BUNDLE); this checks after.
    let cap =
        crate::sandbox::cloud::exec_capture(server, c, &argv, Vec::new(), EXPORT_TIMEOUT).await?;
    if cap.code != 0 {
        return Err(err(
            ErrorKind::Conflict,
            format!("the export in the box failed: {}", tail(&cap.stderr)),
        )
        .details(json!({"reason": "box_export_failed", "code": cap.code})));
    }
    if cap.stdout.len() as u64 > vk_handoff::MAX_BUNDLE {
        return Err(err(ErrorKind::Conflict, "the box's bundle exceeds 200 MiB")
            .details(json!({"reason": "too_large"})));
    }
    check_cancel(server, id)?;
    let path = moves_dir(server)?.join(format!("{id}.in.tar.zst"));
    let data = cap.stdout;
    let p2 = path.clone();
    tokio::task::spawn_blocking(move || std::fs::write(&p2, data))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    Ok((path, task))
}

/// `source_after`, else `[cloud] after_bring_back`.
fn after_of(job: &Job) -> String {
    job.source_after
        .clone()
        .unwrap_or_else(|| vk_cloud::CloudConfig::load().after_bring_back)
}

/// The box's uncommitted work is on the other host now: stash it in the box, where it stays
/// recoverable, so the box no longer counts it as unsynced (and can be destroyed later).
async fn stash_brought_back(server: &Arc<Server>, task: &str) {
    let Some(tb) = server.sandbox.get(task) else {
        return;
    };
    let Some(c) = crate::sandbox::cloud::ctx(&tb) else {
        return;
    };
    let script = format!(
        "cd {} && git stash push -q -u -m {}",
        vk_sandbox::container::sh_quote(&c.workdir),
        vk_sandbox::container::sh_quote(crate::sandbox::cloud::BROUGHT_BACK_STASH)
    );
    let argv = vec!["/bin/sh".into(), "-c".into(), script];
    match crate::sandbox::cloud::exec_capture(server, c, &argv, vec![], Duration::from_secs(60))
        .await
    {
        Ok(o) if o.code == 0 => {}
        Ok(o) => tracing::info!(
            task,
            code = o.code,
            "cloud bring-back: stash in the box failed"
        ),
        Err(e) => tracing::info!(task, "cloud bring-back: stash in the box: {}", e.message),
    }
}

/// Release the box after a bring-back. A refused suspend or destroy (unsynced work the bundle
/// did not carry, say) keeps the box instead; the result says so.
async fn release(server: &Arc<Server>, task: &str, after: &str) -> Result<Value, RpcError> {
    // LANE-B NEEDED: `release_task` detaches the box from the task (the TaskBox leaves the
    // sandbox state, so later panes of the task spawn on the host), closes the task's box panes,
    // then keeps, suspends or destroys the remote box.
    match crate::sandbox::cloud::release_task(server, task, after, false).await {
        Ok(v) => Ok(json!({"after": after, "result": v})),
        Err(e) if after != "keep" => {
            let kept = crate::sandbox::cloud::release_task(server, task, "keep", false).await?;
            Ok(
                json!({"after": "keep", "requested": after, "error": error_json(&e), "result": kept}),
            )
        }
        Err(e) => Err(e),
    }
}

/// The task runs on the host again.
fn set_task_host(server: &Server, task: &str) -> Result<(), RpcError> {
    let mut c = server.core.lock().unwrap();
    let Some(mut t) = c.task(task).cloned() else {
        return Ok(());
    };
    if !t.isolation.is_contained() {
        return Ok(());
    }
    t.isolation = Isolation::default();
    let mut tx = Tx::new();
    tx.task(t);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

/// A host pane at `cwd` in the task's workspace (a new tab), or a new workspace for the task
/// when its workspace closed with the box panes.
fn host_pane_for_task(server: &Arc<Server>, task: &str, cwd: &str) -> Result<Pane, RpcError> {
    let (ws, slug) = server
        .with_core(|c| {
            c.task(task).map(|t| {
                (
                    t.workspace.clone().filter(|w| c.ws(w).is_some()),
                    t.slug.clone(),
                )
            })
        })
        .ok_or_else(|| not_found("task", task))?;
    if let Some(ws) = ws {
        let (_tab, pane) = server
            .create_tab(&ws, Some(cwd), None, None, None)
            .map_err(internal)?;
        return Ok(pane);
    }
    let (ws, _tab, pane) = server
        .create_workspace_for(cwd, Some(slug), None, None, Some(task))
        .map_err(internal)?;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    if let Some(mut t) = c.task(task).cloned() {
        t.workspace = Some(ws.id.clone());
        tx.task(t);
    }
    let mut w = ws.clone();
    w.task = Some(task.to_string());
    tx.ws(w);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(pane)
}

/// Bring back to this host.
async fn bring_back_local(server: &Arc<Server>, job: &Job, pane: &str) -> R {
    let id = job.id.as_str();
    let (bundle, task) = export_from_box(server, job, pane).await?;
    let _cleanup = Cleanup(bundle.clone());

    step(server, id, "importing")?;
    if let Err(e) = crate::orch::call(
        server,
        "task.sync",
        json!({"task": task, "direction": "pull"}),
    )
    .await
    {
        tracing::info!(job = %id, "cloud bring-back: task.sync pull: {}", e.message);
    }
    let wt = server
        .with_core(|c| c.task(&task).and_then(|t| t.worktree_path.clone()))
        .ok_or_else(|| err(ErrorKind::Conflict, "the task has no checkout on this host"))?;
    check_cancel(server, id)?;
    let imp = crate::handoff::import_local(server, &bundle, Path::new(&wt)).await?;
    note(
        server,
        id,
        json!({"worktree": imp.worktree, "branch": imp.branch, "in_place": imp.in_place}),
    );

    // From here on the work is on this host: no more cancelling.
    step(server, id, "resuming")?;
    stash_brought_back(server, &task).await;
    let released = release(server, &task, &after_of(job))
        .await
        .map_err(|e| e.details(json!({"reason": "release_failed", "imported": imp.worktree})))?;
    set_task_host(server, &task)?;
    let cwd = imp.cwd.display().to_string();
    let host_pane = if imp.in_place {
        host_pane_for_task(server, &task, &cwd)?
    } else {
        server
            .create_workspace(&cwd, None, None, None)
            .map_err(internal)?
            .2
    };
    let run = match &imp.resume_argv {
        Some(argv) => {
            match resume_in(
                server,
                &host_pane.id,
                Some(task.as_str()),
                argv,
                imp.resumed,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => Some(json!({"agent_error": error_json(&e)})),
            }
        }
        None => None,
    };
    if server.with_core(|c| c.pane(pane).is_some()) {
        server.close_pane(pane);
    }
    Ok(json!({
        "pane": host_pane.id,
        "task": task,
        "worktree": imp.worktree,
        "branch": imp.branch,
        "cwd": cwd,
        "in_place": imp.in_place,
        "resumed": imp.resumed,
        "run": run.as_ref().and_then(|r| r.get("id")).cloned(),
        "not_written": imp.not_written,
        "skipped": imp.manifest.skipped,
        "release": released,
    }))
}

/// Bring back to a paired host: the gateway delivers the bundle as a handoff.
async fn bring_back_peer(server: &Arc<Server>, job: &Job, pane: &str, peer: &str) -> R {
    let id = job.id.as_str();
    let (bundle, task) = export_from_box(server, job, pane).await?;
    step(server, id, "uploading")?;
    // The gateway removes the file when its job ends.
    let sent = crate::handoff_out::send_bundle(
        server,
        &crate::drafts::user_ctx(),
        &json!({"pane": pane, "peer": peer, "interrupt": false, "actor": format!("cloud.move {id}")}),
        bundle.display().to_string(),
    );
    let hjob = match sent {
        Ok(v) => v
            .pointer("/job/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| internal("handoff.send returned no job"))?,
        Err(e) => {
            let _ = std::fs::remove_file(&bundle);
            return Err(e);
        }
    };
    note(server, id, json!({"peer_job": hjob}));
    let deadline = Instant::now() + PEER_WAIT;
    let delivered = loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if check_cancel(server, id).is_err() {
            let _ = crate::handoff_out::cancel(server, &json!({"id": hjob}));
            return Err(cancelled_err(id));
        }
        let h = crate::handoff_out::get(server, &hjob)?;
        if h.total > 0 {
            set_progress(server, id, h.sent, h.total);
        }
        match h.state.as_str() {
            "delivered" => break h,
            "failed" => {
                return Err(err(
                    ErrorKind::Conflict,
                    format!(
                        "the handoff to {} failed: {}",
                        h.peer_name,
                        h.error.as_deref().unwrap_or("no reason given")
                    ),
                )
                .details(json!({"reason": "handoff_failed", "peer_job": hjob})));
            }
            "cancelled" => {
                return Err(err(ErrorKind::Conflict, "the handoff was cancelled")
                    .details(json!({"reason": "handoff_cancelled", "peer_job": hjob})));
            }
            _ => {}
        }
        if Instant::now() > deadline {
            let _ = crate::handoff_out::cancel(server, &json!({"id": hjob}));
            return Err(err(
                ErrorKind::Timeout,
                "the handoff was not delivered within 6 hours",
            ));
        }
    };
    step(server, id, "resuming")?;
    // The agent continues on the peer: the box pane goes, then the box is released.
    server.close_pane(pane);
    stash_brought_back(server, &task).await;
    let released = release(server, &task, &after_of(job)).await?;
    Ok(json!({
        "task": task,
        "peer_job": hjob,
        "incoming": delivered.incoming,
        "incoming_state": delivered.incoming_state,
        "release": released,
    }))
}

// ---------------------------------------------------------------------------------------------
// schema

/// Schema registry entries (`api_schema` loads them next to its own tables).
pub const SHAPES: &str = r##"
# --- cloud moves (spec 17 §7): server jobs; full scope, never from a pane (a pane asks with auth.approve) ---
cloud.move :: {pane?: Target, run?: string, box?: string, to: {kind: cloud|local|peer, provider?: string, box?: string, peer?: string}, interrupt?: bool = false, source_after?: keep|suspend|destroy}
  => {job: {id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, created_at: int, updated_at: int}}
# newest first; finished jobs for 7 days
cloud.jobs :: {}
  => {jobs: [{id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, created_at: int, updated_at: int}]}
cloud.cancel :: {id: string}
  => {job: {id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, created_at: int, updated_at: int}}
"##;

pub const EVENTS: &str = r##"
cloud.job :: {job: string} => {id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, created_at: int, updated_at: int}
"##;

#[cfg(test)]
#[path = "cloud_move_tests.rs"]
mod tests;
