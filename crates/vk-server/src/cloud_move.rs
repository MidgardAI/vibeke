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
//!
//! Safety rules (review findings D1.8 to D1.15):
//! - From the turn boundary until the move commits or rolls back, the moving pane takes no
//!   input and its agent no follow-ups ([`input_paused`]).
//! - A send never takes uncommitted work from a shared checkout (the repository's main
//!   checkout, or one other panes use): it is refused with `shared_checkout`, and `new_task`
//!   copies the work into a new task worktree instead. A send from the task's own checkout
//!   stashes only the paths it exported.
//! - A failed send also forgets the task it adopted (releasing its ports) or removes the task
//!   it created.
//! - A local bring-back stops in `awaiting_confirmation` with `result.review` (the changes and
//!   a warning: the box ran with full permissions) until `cloud.confirm`; `code_only` brings
//!   the files back without resuming the agent from the box's transcript. A resumed agent's
//!   first message says that its earlier turns came from the box.

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
    ("cloud.confirm", true),
];

/// Every `cloud.*` method is the user's (spec 17 §10); a pane asks for a move through
/// `auth.approve`.
pub const PANE_FORBIDDEN: &[&str] = &["cloud.move", "cloud.jobs", "cloud.cancel", "cloud.confirm"];

/// In pipeline order; the last three are terminal.
pub const STATES: &[&str] = &[
    "queued",
    "waiting_turn",
    "creating",
    "bootstrapping",
    "exporting",
    "awaiting_confirmation",
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
/// How long a bring-back waits in `awaiting_confirmation` for the user.
const CONFIRM_WAIT: Duration = Duration::from_secs(24 * 3600);
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
    /// Send: the pane works in a shared checkout (the repository's main checkout, or one other
    /// panes use). The send creates a task worktree for the work first and leaves the shared
    /// checkout as it was.
    #[serde(default)]
    pub new_task: bool,
    /// Bring back to this host: stop after the export in `awaiting_confirmation` with a summary
    /// of the changes (`result.review`) until `cloud.confirm`.
    #[serde(default)]
    pub review: bool,
    /// Bring back the code only: the agent is not resumed from the box's transcript.
    #[serde(default)]
    pub code_only: bool,
    /// Set by `cloud.confirm` (a bring-back waiting in `awaiting_confirmation` goes on).
    #[serde(default)]
    pub confirmed: bool,
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
        "cloud.confirm" => confirm(server, p),
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
        // The commit point: the work is at the destination and the agent is starting there.
        // Checked under the same lock as the runner's step to `resuming`.
        "resuming" => Err(conflict(
            "too late to cancel: the work already moved and the agent is resuming",
            job,
        )
        .details(json!({"job": job.id, "state": job.state, "reason": "too_late"}))),
        _ => {
            job.state = "cancelled".into();
            Ok(true)
        }
    })?;
    Ok(json!({"job": job_json(&job)}))
}

/// `cloud.confirm {id, code_only?}`: the user saw the bring-back's summary and warning
/// (`result.review`) and lets it go on; `code_only` brings back the files without resuming the
/// agent from the box's transcript.
pub(crate) fn confirm(server: &Server, p: &Value) -> R {
    let id = req(p, "id")?;
    let code_only = b(p, "code_only");
    let job = modify(server, id, |job| {
        if job.state != "awaiting_confirmation" {
            return Err(conflict(
                format!("the move is {}, not waiting for confirmation", job.state),
                job,
            ));
        }
        job.confirmed = true;
        if let Some(c) = code_only {
            job.code_only = c;
        }
        Ok(true)
    })?;
    Ok(json!({"job": job_json(&job)}))
}

// ---------------------------------------------------------------------------------------------
// input freeze

fn frozen() -> &'static Mutex<HashSet<String>> {
    static F: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    F.get_or_init(Mutex::default)
}

/// Why input to `pane` is refused right now: its agent is being moved between this host and a
/// cloud box. From the turn boundary until the move commits or rolls back, the agent must not
/// change anything (the export would miss it).
pub(crate) fn input_paused(pane: &str) -> Option<&'static str> {
    frozen().lock().unwrap().contains(pane).then_some(
        "pane_moving: this agent is moving to or from a cloud box; input is paused until the move finishes",
    )
}

/// Pauses input to a pane (and follow-ups to its agent) while it lives.
struct Freeze(String);

impl Freeze {
    fn new(pane: &str) -> Freeze {
        frozen().lock().unwrap().insert(pane.to_string());
        Freeze(pane.to_string())
    }
}

impl Drop for Freeze {
    fn drop(&mut self) {
        frozen().lock().unwrap().remove(&self.0);
    }
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
    crate::sandbox::cloud::ctx(&tb).map(|c| c.box_ref())
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
    // Sign-in is checked now, so clients get `needs_auth` and can prompt before a job exists.
    let provider = match (&dest, &src.box_ref) {
        (Dest::Cloud { provider, .. }, _) => provider
            .clone()
            .unwrap_or_else(|| vk_cloud::CloudConfig::load().default_provider),
        (_, Some(b)) => b.split('/').next().unwrap_or_default().to_string(),
        _ => String::new(),
    };
    if !provider.is_empty() {
        crate::sandbox::cloud::credential(server, &provider)?;
    }
    if matches!(dest, Dest::Cloud { .. })
        && !b(p, "new_task").unwrap_or(false)
        && let Some(why) = shared_checkout(server, &src)
    {
        return Err(err(
            ErrorKind::Conflict,
            format!(
                "Pane {} works in {why}. Sending it would move uncommitted work that is not only this agent's. Send it with a new task instead: Vibeke copies the work into a new task worktree, sends that, and leaves this checkout as it is.",
                src.pane.handle
            ),
        )
        .details(json!({
            "reason": "shared_checkout",
            "why": why,
            "retry": {"new_task": true},
            "hint": "retry with new_task: true (`vibeke cloud move --new-task`), or create a task first with `vibeke task create`",
        })));
    }
    let jobs = server
        .with_core(|c| c.store.load::<Job>(K_JOB))
        .map_err(internal)?;
    let busy = server.with_core(|c| {
        busy_move(
            &jobs,
            &src.pane.id,
            &src.pane.workspace,
            src.task.as_deref(),
            |p| c.pane(p).map(|x| x.workspace.clone()),
        )
        .map(|(j, same)| (j.clone(), same))
    });
    if let Some((j, same)) = busy {
        return Err(busy_err(&j, same));
    }
    Ok((src, dest, after))
}

/// Why the checkout of `src.pane` is shared, so a send must not take its uncommitted work:
/// it is the repository's main checkout, or a pane outside the source's workspace (or another
/// agent in it) works there. `None` when it is the source's own (a task worktree) or unknown.
fn shared_checkout(server: &Server, src: &Source) -> Option<String> {
    let cwd = server.pane_cwd(&src.pane.id)?;
    let info = vk_tasks::repo_root(Path::new(&cwd))?;
    if !info.is_linked_worktree {
        return Some("the repository's main checkout".into());
    }
    let root = info
        .worktree_root
        .canonicalize()
        .unwrap_or(info.worktree_root.clone());
    let others: Vec<(String, String)> = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .filter(|x| x.id != src.pane.id)
            .map(|x| (x.id.clone(), x.workspace.clone()))
            .collect()
    });
    let mut n = 0;
    for (id, ws) in others {
        let Some(pc) = server.pane_cwd(&id) else {
            continue;
        };
        let pc = Path::new(&pc);
        let inside = pc.starts_with(&root) || pc.canonicalize().is_ok_and(|c| c.starts_with(&root));
        if !inside {
            continue;
        }
        // Panes of the source's own workspace (its task) share its work; another agent there
        // does not.
        let agent = live_run(server, &id).is_some();
        if ws != src.pane.workspace || agent {
            n += 1;
        }
    }
    (n > 0).then(|| format!("a checkout that {n} other pane(s) use"))
}

/// The unfinished move that keeps a new move of `pane` (in workspace `ws`, of `task`) from
/// starting, and whether it moves the same pane. Moves are one at a time per task: the panes
/// of a task share its box, and a failed send undoes what it did to the task. A job's task is
/// its recorded `task`, or (not known yet) its pane's workspace (`ws_of`), which is the task's.
pub(crate) fn busy_move<'a>(
    jobs: &'a [Job],
    pane: &str,
    ws: &str,
    task: Option<&str>,
    ws_of: impl Fn(&str) -> Option<String>,
) -> Option<(&'a Job, bool)> {
    let live = || jobs.iter().filter(|j| !terminal(&j.state));
    if let Some(j) = live().find(|j| j.pane.as_deref() == Some(pane)) {
        return Some((j, true));
    }
    live()
        .find(|j| {
            (task.is_some() && j.task.as_deref() == task)
                || j.pane.as_deref().and_then(&ws_of).as_deref() == Some(ws)
        })
        .map(|j| (j, false))
}

fn busy_err(j: &Job, same_pane: bool) -> RpcError {
    let msg = if same_pane {
        "a move of this pane is already under way; cancel it first"
    } else {
        "a move of this task is already under way; wait for it or cancel it first"
    };
    conflict(msg, j).details(json!({"job": j.id, "state": j.state, "reason": "move_under_way"}))
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
        new_task: matches!(dest, Dest::Cloud { .. }) && b(p, "new_task").unwrap_or(false),
        // A bring-back to this host applies files and a transcript written in the box: the
        // user sees what comes back before it is applied, unless they asked not to.
        review: matches!(dest, Dest::Local) && b(p, "review").unwrap_or(true),
        code_only: b(p, "code_only").unwrap_or(false),
        confirmed: false,
        created_at: now,
        updated_at: now,
    };
    {
        let mut c = server.core.lock().unwrap();
        let jobs = c.store.load::<Job>(K_JOB).map_err(internal)?;
        // Checked again under the lock that records the job: one move per task at a time.
        if let Some((busy, same)) = busy_move(
            &jobs,
            &src.pane.id,
            &src.pane.workspace,
            src.task.as_deref(),
            |p| c.pane(p).map(|x| x.workspace.clone()),
        ) {
            return Err(busy_err(busy, same));
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
/// `resumed_note`: the first message of a resumed session (a transcript from a cloud box gets
/// [`vk_handoff::RESUMED_FROM_BOX_NOTE`]).
async fn resume_in(
    server: &Arc<Server>,
    pane: &str,
    task: Option<&str>,
    resume_argv: &[String],
    resumed: bool,
    resumed_note: Option<&str>,
) -> Result<Option<Value>, RpcError> {
    let Some(harness) = resume_argv.first() else {
        return Ok(None);
    };
    let args: Vec<String> = resume_argv[1..].to_vec();
    let prompt = if resumed {
        resumed_note
    } else {
        Some(
            "This session was moved by Vibeke. The previous transcript could not be carried over; the files are as the last agent left them.",
        )
    };
    let opts = crate::sandbox::LaunchOpts::from_params(&json!({}))?;
    let r =
        crate::agents::start_in_pane_opts(server, pane, harness, None, prompt, &args, task, &opts)
            .await?;
    Ok(Some(r.get("run").cloned().unwrap_or(r)))
}

/// The task of a pane, adopting the pane's checkout as a task when it has none. Returns the
/// task and whether this call adopted it (a failed send forgets it again).
async fn task_for(server: &Arc<Server>, pane: &str) -> Result<(String, bool), RpcError> {
    if let Some(t) = pane_task(server, pane) {
        return Ok((t, false));
    }
    let r = crate::orch::call(server, "task.adopt", json!({"pane": pane})).await?;
    r.pointer("/task/id")
        .and_then(Value::as_str)
        .map(|t| (t.to_string(), true))
        .ok_or_else(|| internal("task.adopt returned no task"))
}

/// A send with `new_task`: a new task worktree at the source checkout's `HEAD` (no setup, no
/// agents) takes the work; the shared source checkout stays as it is. Returns the task, its
/// first pane and its worktree.
async fn new_task_for(
    server: &Arc<Server>,
    pane: &str,
    cwd: &str,
) -> Result<(String, String, String), RpcError> {
    let head = vk_tasks::branch_status(Path::new(cwd), None)
        .ok()
        .and_then(|s| s.oid)
        .ok_or_else(|| {
            err(
                ErrorKind::Conflict,
                "the checkout has no commit to start a task from",
            )
        })?;
    let title = server
        .with_core(|c| c.pane(pane).map(|p| p.display_title().to_string()))
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "Sent to the cloud".into());
    let r = crate::orch::call(
        server,
        "task.create",
        json!({"title": title, "repo": cwd, "base": head, "setup": false}),
    )
    .await?;
    let task = r
        .pointer("/task/id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("task.create returned no task"))?
        .to_string();
    let first = r
        .pointer("/panes/0/id")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("task.create returned no pane"))?
        .to_string();
    let wt = r
        .pointer("/task/worktree_path")
        .and_then(Value::as_str)
        .ok_or_else(|| internal("task.create returned no worktree"))?
        .to_string();
    Ok((task, first, wt))
}

/// What a send changed before its commit point, so a failure can undo it ([`roll_back_send`]).
/// Only what this send made is undone: the context it installed (by its box), the panes it
/// opened and the box it created. The job records them as they happen (`result.created`).
#[derive(Debug, Default)]
struct SendUndo {
    /// The task whose context this send created.
    task: Option<String>,
    /// The task's isolation before the send.
    prior: Option<Isolation>,
    /// The context this send installed (its box).
    ctx: Option<crate::sandbox::cloud::CloudCtx>,
    /// Panes this send opened that are still open.
    panes: Vec<String>,
    /// The task had no box on record before: the send created it.
    new_box: bool,
    /// The box's import ran (it may hold the work now): the box is kept.
    imported: bool,
    /// The source pane closed: the move is done, nothing is undone any more.
    committed: bool,
    /// The task this send adopted from the pane's checkout (forgotten again on failure, which
    /// also releases its port lease).
    adopted: Option<String>,
    /// The task this send created for `new_task` (finished, with its worktree removed, on
    /// failure).
    created_task: Option<String>,
}

/// Send: host pane → cloud box. A failure or a cancel before the source pane closes puts the
/// task back on the host ([`roll_back_send`]), so the move can be tried again.
async fn send(
    server: &Arc<Server>,
    job: &Job,
    pane: &str,
    provider: Option<&str>,
    want_box: Option<&str>,
) -> R {
    let mut undo = SendUndo::default();
    let out = send_steps(server, job, pane, provider, want_box, &mut undo).await;
    if out.is_err() {
        note_created(server, &job.id, &undo);
    }
    if out.is_err()
        && !undo.committed
        && (undo.task.is_some() || undo.adopted.is_some() || undo.created_task.is_some())
    {
        let rb = roll_back_send(server, &undo).await;
        tracing::info!(job = %job.id, rollback = %rb, "cloud send rolled back");
        note(server, &job.id, json!({"rollback": rb}));
    }
    out
}

/// Undo what a failed send did to its task: stop serving the task from the box, restore the
/// task's isolation, and destroy the box when this send created it and nothing was imported
/// into it (a box that existed before, or holds an import, is kept). Returns
/// `{box?, box_action}` for the job result.
async fn roll_back_send(server: &Arc<Server>, undo: &SendUndo) -> Value {
    let mut v = roll_back_box(server, undo).await;
    // A task this send made for the move goes again: an adopted one is forgotten (its port
    // lease is released, the pane's workspace is left as it was), a created one is finished
    // and its new worktree removed.
    if let Some(t) = &undo.adopted {
        v["adopted_task"] =
            match crate::orch::call(server, "task.forget", json!({"task": t, "force": true})).await
            {
                Ok(_) => json!({"task": t, "action": "forgotten"}),
                Err(e) => json!({"task": t, "action": "kept", "error": error_json(&e)}),
            };
    }
    if let Some(t) = &undo.created_task {
        v["created_task"] = match crate::orch::call(
            server,
            "task.finish",
            json!({"task": t, "remove_worktree": true, "force": true, "archive": true}),
        )
        .await
        {
            Ok(_) => json!({"task": t, "action": "removed"}),
            Err(e) => json!({"task": t, "action": "kept", "error": error_json(&e)}),
        };
    }
    v
}

/// The box part of [`roll_back_send`].
async fn roll_back_box(server: &Arc<Server>, undo: &SendUndo) -> Value {
    let Some(task) = undo.task.as_deref() else {
        return json!({});
    };
    // Only the panes this send opened.
    for p in &undo.panes {
        server.close_pane(p);
    }
    let ctx = undo.ctx.clone();
    // The task's context goes only while it is still the one this send installed.
    let current = server
        .sandbox
        .get(task)
        .as_deref()
        .and_then(crate::sandbox::cloud::ctx)
        .map(|c| c.box_ref());
    let ours = match (&current, &ctx) {
        (Some(cur), Some(c)) => *cur == c.box_ref(),
        _ => false,
    };
    if !ours {
        let b = ctx.as_ref().map(|c| c.box_ref());
        return json!({"box": b, "box_action": "kept", "context": "not_ours"});
    }
    crate::sandbox::cloud::detach(server, task);
    if let Some(prior) = undo.prior.clone() {
        let mut c = server.core.lock().unwrap();
        if let Some(mut t) = c.task(task).cloned()
            && t.isolation != prior
        {
            t.isolation = prior;
            let mut tx = Tx::new();
            tx.event(
                "task.updated",
                json!({"task": t.id}),
                json!({"isolation": t.isolation.level.as_str()}),
            );
            tx.task(t);
            let _ = server.commit(&mut c, tx);
        }
    }
    let Some(c) = ctx else {
        return json!({"box_action": "none"});
    };
    let box_ref = c.box_ref();
    // Destroy only the box this send created (its context names it).
    if undo.new_box && !undo.imported {
        match crate::sandbox::cloud::destroy_box(server, &c).await {
            Ok(()) => json!({"box": box_ref, "box_action": "destroyed"}),
            Err(e) => {
                json!({"box": box_ref, "box_action": "kept", "error": error_json(&e)})
            }
        }
    } else {
        json!({"box": box_ref, "box_action": "kept"})
    }
}

/// Record in the job what this send made so far (`result.created`): the box of the context it
/// installed, whether it created that box, and the panes it opened.
fn note_created(server: &Server, id: &str, undo: &SendUndo) {
    if undo.task.is_none() && undo.panes.is_empty() {
        return;
    }
    note(server, id, json!({"created": created_json(undo)}));
}

fn created_json(undo: &SendUndo) -> Value {
    json!({
        "box": undo.ctx.as_ref().map(|c| c.box_ref()),
        "new_box": undo.new_box && undo.ctx.is_some(),
        "panes": undo.panes,
    })
}

/// How long a resumed agent in the box gets to come up after `start_in_pane_opts` returned.
const AGENT_UP_WAIT: Duration = Duration::from_secs(60);

/// The destination agent runs: its run exists, has not exited and left `starting`.
async fn confirm_agent(server: &Arc<Server>, run: Option<&Value>) -> Result<(), RpcError> {
    let failed = |why: &str| {
        err(
            ErrorKind::Conflict,
            format!("the agent did not start in the box: {why}"),
        )
        .details(json!({"reason": "agent_not_started"}))
    };
    let id = run
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| failed("no run was recorded"))?
        .to_string();
    let deadline = Instant::now() + AGENT_UP_WAIT;
    loop {
        match server.with_core(|c| c.run(&id).map(|r| r.execution.value.clone())) {
            None | Some(Execution::Exited) => return Err(failed("it exited")),
            Some(
                Execution::Idle | Execution::Working | Execution::Error | Execution::RateLimited,
            ) => return Ok(()),
            Some(_) => {}
        }
        if Instant::now() > deadline {
            return Err(failed("it did not report itself in time"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn send_steps(
    server: &Arc<Server>,
    job: &Job,
    pane: &str,
    provider: Option<&str>,
    want_box: Option<&str>,
    undo: &mut SendUndo,
) -> R {
    let id = job.id.as_str();
    let run = turn_boundary(server, id, pane, job.interrupt).await?;
    // From the turn boundary until the move commits or rolls back, the agent gets no input and
    // no follow-ups: anything it did now would miss the export.
    let _freeze = Freeze::new(pane);
    let cwd = server
        .pane_cwd(pane)
        .ok_or_else(|| err(ErrorKind::NotFound, "the pane has no working directory"))?;

    step(server, id, "creating")?;
    // `split_from`/`dest_cwd`: where the box pane opens (a pane of the task's workspace, so it
    // spawns through the task's box).
    let (task, split_from, dest_cwd) = if job.new_task {
        let (task, first, wt) = new_task_for(server, pane, &cwd).await?;
        undo.created_task = Some(task.clone());
        note(
            server,
            id,
            json!({"task": task, "new_task": true, "worktree": wt}),
        );
        (task, first, wt)
    } else {
        let (task, adopted) = task_for(server, pane).await?;
        if adopted {
            undo.adopted = Some(task.clone());
        }
        note(server, id, json!({"task": task, "adopted": adopted}));
        (task, pane.to_string(), cwd.clone())
    };
    check_cancel(server, id)?;
    // Only a context this send creates is rolled back.
    if server.sandbox.get(&task).is_none() {
        undo.prior = server.with_core(|c| c.task(&task).map(|t| t.isolation.clone()));
        undo.new_box = crate::sandbox::cloud::record_for_key(server, &task).is_none();
    }
    let tb = crate::sandbox::cloud::ensure_for_task(server, &task, provider).await?;
    let c = crate::sandbox::cloud::ctx(&tb)
        .ok_or_else(|| internal("the task's box is not a cloud box"))?;
    if undo.prior.is_some() {
        undo.task = Some(task.clone());
        undo.ctx = Some(c.clone());
        note_created(server, id, undo);
    }
    let box_ref = c.box_ref();
    if let Some(w) = want_box
        && w != box_ref
    {
        // A send goes into the task's own box; moving into another existing box is not
        // supported (review finding D1.16).
        return Err(err(
            ErrorKind::Unsupported,
            format!(
                "Vibeke sends work into the task's own box ({box_ref}). Moving into another existing box ({w}) is not supported: send without choosing a box."
            ),
        )
        .details(json!({"reason": "other_box", "box": box_ref})));
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
    // Changes an earlier bring-back already carried to the host would block the import: they
    // are dropped, but only while the tree is exactly the one that was carried.
    let clear = crate::sandbox::cloud::clear_synced_script(&c.workdir);
    let argv = vec!["/bin/sh".into(), "-c".into(), clear];
    if let Err(e) =
        crate::sandbox::cloud::exec_capture(server, c, &argv, vec![], Duration::from_secs(60)).await
    {
        tracing::info!(job = %id, "cloud send: clearing brought-back changes: {}", e.message);
    }
    let argv: Vec<String> = vec![
        c.bin.clone(),
        "sandbox".into(),
        "import-bundle".into(),
        "--bundle".into(),
        remote,
        "--workspace".into(),
        c.workdir.clone(),
    ];
    // The panes' env: the transcript goes where the box's agent looks for it
    // (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, ...).
    undo.imported = true;
    let cap = crate::sandbox::cloud::exec_capture_env(
        server,
        c,
        &argv,
        &c.runner.exec_env,
        Vec::new(),
        IMPORT_TIMEOUT,
    )
    .await?;
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
            &split_from,
            Direction::Right,
            0.5,
            Some(&dest_cwd),
            None,
            Some(format!(
                "☁ {}",
                box_ref.split('/').next().unwrap_or("cloud")
            )),
            None,
            "cloud.move",
        )
        .map_err(internal)?;
    undo.panes.push(new_pane.id.clone());
    note_created(server, id, undo);
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
            None,
        )
        .await
    }
    .await;
    // An agent must run at the destination before its source closes; a plain shell pane
    // only needs its new pane.
    let started = match started {
        Ok(r) if !resume_argv.is_empty() => match confirm_agent(server, r.as_ref()).await {
            Ok(()) => Ok(r),
            Err(e) => Err(e),
        },
        other => other,
    };
    let run = match started {
        Ok(r) => r,
        Err(e) => {
            server.close_pane(&new_pane.id);
            undo.panes.retain(|p| *p != new_pane.id);
            return Err(e);
        }
    };
    // The agent runs in the box: the source pane goes. In the task's own checkout the
    // uncommitted changes that went to the box are stashed (only those paths), so a later
    // bring-back lands in a clean checkout. A shared checkout (`new_task`) is left as it was.
    undo.committed = true;
    server.close_pane(pane);
    let host_stash = if job.new_task {
        // The new task's first pane is a host shell; the task now runs in the box.
        if split_from != pane {
            server.close_pane(&split_from);
        }
        json!({"stashed": false, "reason": "the work was copied from a shared checkout, which is left as it was"})
    } else {
        stash_sent(&cwd, id, &box_ref, &packed.manifest).await
    };
    Ok(json!({
        "pane": new_pane.id,
        "task": task,
        "box": box_ref,
        "host_stash": host_stash,
        "run": run.as_ref().and_then(|r| r.get("id")).cloned(),
        "cwd": box_cwd,
        "resumed": resumed,
        "not_written": imported.get("not_written").cloned().unwrap_or(json!([])),
        "skipped": imported.get("skipped").cloned().unwrap_or(json!([])),
    }))
}

/// Stash what a send exported from the host checkout at `cwd`: the tracked files with changes
/// against `HEAD` and the untracked files the bundle carried (`manifest.untracked`), nothing
/// else. Files the export skipped stay in the checkout. The stash keeps them recoverable on
/// this host (`git stash list`). Returns `{stashed, paths?, reason?}` for the job result.
async fn stash_sent(cwd: &str, job: &str, box_ref: &str, m: &vk_handoff::Manifest) -> Value {
    let git = |args: Vec<String>| {
        let cwd = cwd.to_string();
        async move {
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(&args)
                .stdin(std::process::Stdio::null())
                .output()
                .await
        }
    };
    // Paths are relative to the repository root: run the stash there.
    let tracked = match git(vec![
        "diff".into(),
        "--name-only".into(),
        "-z".into(),
        "--no-renames".into(),
        "HEAD".into(),
    ])
    .await
    {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => return json!({"stashed": false, "reason": tail(&o.stderr)}),
        Err(e) => return json!({"stashed": false, "reason": e.to_string()}),
    };
    let skipped: HashSet<&str> = m.skipped.iter().map(|x| x.path.as_str()).collect();
    let mut paths: Vec<String> = tracked
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .chain(m.untracked.iter().cloned())
        .filter(|p| !skipped.contains(p.as_str()))
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return json!({"stashed": false, "reason": "nothing was exported from the checkout"});
    }
    let msg = format!("vibeke: sent to {box_ref} (job {job})");
    let mut args: Vec<String> = vec![
        "stash".into(),
        "push".into(),
        "-q".into(),
        "-u".into(),
        "-m".into(),
        msg.clone(),
        "--".into(),
    ];
    // `top`: relative to the repository root, whatever `cwd` is; `literal`: no glob.
    args.extend(paths.iter().map(|p| format!(":(top,literal){p}")));
    match git(args).await {
        Ok(o) if o.status.success() => {
            json!({"stashed": true, "message": msg, "paths": paths.len()})
        }
        Ok(o) => json!({"stashed": false, "reason": tail(&o.stderr)}),
        Err(e) => json!({"stashed": false, "reason": e.to_string()}),
    }
}

/// Bring back, first half: the box pane's work exported in the box into a local file.
/// Also returns the [`Freeze`] of the box pane: it holds until the caller commits or fails.
async fn export_from_box(
    server: &Arc<Server>,
    job: &Job,
    pane: &str,
) -> Result<(PathBuf, String, Option<String>, Freeze), RpcError> {
    let id = job.id.as_str();
    let run = turn_boundary(server, id, pane, job.interrupt).await?;
    let freeze = Freeze::new(pane);
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
    // The working tree as it is before the export: a successful bring-back marks exactly this
    // tree as synced, so a change made during or after the export still counts as unsynced.
    let fp = tree_fingerprint(server, c).await;
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
        // The box name (`vk-<host8>-<key10>`).
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
    // Open (sandbox_cloud): `exec_capture` reads the whole stdout before this size check; a
    // cap while reading (MAX_BUNDLE) would bound the memory a box can make the host use.
    // The panes' env, so the export finds the transcript where the box's agent wrote it.
    let cap = crate::sandbox::cloud::exec_capture_env(
        server,
        c,
        &argv,
        &c.runner.exec_env,
        Vec::new(),
        EXPORT_TIMEOUT,
    )
    .await?;
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
    Ok((path, task, fp, freeze))
}

/// Most entries of each list in a bring-back review.
const REVIEW_MAX: usize = 200;

/// Files a unified diff touches, with added and removed line counts.
fn patch_files(patch: &str) -> Vec<(String, u64, u64)> {
    let mut out: Vec<(String, u64, u64)> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `a/<path> b/<path>`: the `b/` side names the file after the change.
            let path = rest
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            out.push((path, 0, 0));
        } else if let Some(cur) = out.last_mut() {
            if line.starts_with('+') && !line.starts_with("+++") {
                cur.1 += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                cur.2 += 1;
            }
        }
    }
    out
}

/// What a bring-back would apply (`result.review`): the changed and new files, what the export
/// left out, whether a transcript comes along, and the warning to show (review finding D1.15).
async fn review_summary(server: &Server, id: &str, bundle: &Path) -> Result<Value, RpcError> {
    let d2 = moves_dir(server)?.join(format!("{id}.review"));
    let b = bundle.to_path_buf();
    let (m, patch) = tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_dir_all(&d2);
        std::fs::create_dir(&d2)?;
        let m = vk_handoff::unpack(&b, &d2);
        let patch = std::fs::read(d2.join("changes.patch")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&d2);
        m.map(|m| (m, patch))
    })
    .await
    .map_err(internal)?
    .map_err(|e| invalid(format!("bad bundle: {e}")))?;
    let files = patch_files(&String::from_utf8_lossy(&patch));
    let (added, removed) = files.iter().fold((0, 0), |(a, r), f| (a + f.1, r + f.2));
    Ok(json!({
        "warning": vk_handoff::BOX_BRING_BACK_WARNING,
        "head": m.head,
        "branch": m.branch,
        "files": files.iter().take(REVIEW_MAX).map(|(p, a, r)| json!({"path": p, "added": a, "removed": r})).collect::<Vec<_>>(),
        "files_total": files.len(),
        "added": added,
        "removed": removed,
        "untracked": m.untracked.iter().take(REVIEW_MAX).collect::<Vec<_>>(),
        "untracked_total": m.untracked.len(),
        "skipped": m.skipped.iter().take(REVIEW_MAX).map(|x| json!({"path": x.path, "reason": x.reason})).collect::<Vec<_>>(),
        "harness": m.harness,
        "transcript": m.transcript_rel.is_some(),
        "confirm": "cloud.confirm {id} to apply (code_only: true to bring back the files without resuming the agent), cloud.cancel {id} to stop",
    }))
}

/// Wait in `awaiting_confirmation` until `cloud.confirm` (returns `code_only`), a cancel, or
/// [`CONFIRM_WAIT`].
async fn wait_confirmed(server: &Server, id: &str) -> Result<bool, RpcError> {
    let deadline = Instant::now() + CONFIRM_WAIT;
    loop {
        let j = get(server, id)?;
        match j.state.as_str() {
            "cancelled" => return Err(cancelled_err(id)),
            _ if j.confirmed => return Ok(j.code_only),
            _ => {}
        }
        if Instant::now() > deadline {
            return Err(err(
                ErrorKind::Timeout,
                "the bring-back was not confirmed within 24 hours; nothing was applied",
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Fingerprint of the box repo's working tree ([`crate::sandbox::cloud::TREE_FINGERPRINT`]).
async fn tree_fingerprint(
    server: &Arc<Server>,
    c: &crate::sandbox::cloud::CloudCtx,
) -> Option<String> {
    let script = crate::sandbox::cloud::fingerprint_script(&c.workdir);
    let argv = vec!["/bin/sh".into(), "-c".into(), script];
    let o = crate::sandbox::cloud::exec_capture(server, c, &argv, vec![], Duration::from_secs(60))
        .await
        .ok()
        .filter(|o| o.code == 0)?;
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("fp="))
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
}

/// `source_after`, else `[cloud] after_bring_back`.
fn after_of(job: &Job) -> String {
    job.source_after
        .clone()
        .unwrap_or_else(|| vk_cloud::CloudConfig::load().after_bring_back)
}

/// Why the box's leftovers must stay unsynced after a bring-back, if they must: the export
/// left files out (`skipped`: too large, secret, not a regular file) or the import did not write
/// some (`not_written`). Stashing then would hide work the other host never got from the destroy
/// guard.
pub(crate) fn keep_leftovers_reason(skipped: usize, not_written: usize) -> Option<String> {
    match (skipped, not_written) {
        (0, 0) => None,
        (s, 0) => Some(format!("{s} file(s) were not exported")),
        (0, n) => Some(format!("{n} file(s) were not written on the host")),
        (s, n) => Some(format!(
            "{s} file(s) were not exported and {n} were not written on the host"
        )),
    }
}

/// The box's uncommitted work is on this host now: record the working tree the export saw
/// (`fp`) as synced in the box ([`crate::sandbox::cloud::SYNCED_MARK`]), so the box no longer
/// counts it as unsynced while the tree stays exactly that. Nothing is recorded when the move
/// did not carry everything ([`keep_leftovers_reason`]) or the tree was not fingerprinted: the
/// destroy guard then keeps the box. Returns `{marked, reason?}` for the job result.
async fn mark_brought_back(
    server: &Arc<Server>,
    task: &str,
    fp: Option<&str>,
    skipped: usize,
    not_written: usize,
) -> Value {
    if let Some(reason) = keep_leftovers_reason(skipped, not_written) {
        return json!({"marked": false, "reason": format!("{reason}; the box keeps them as unsynced work")});
    }
    let Some(fp) = fp else {
        return json!({"marked": false, "reason": "the box's working tree could not be fingerprinted; its changes stay unsynced"});
    };
    let Some(tb) = server.sandbox.get(task) else {
        return json!({"marked": false, "reason": "the box is not attached"});
    };
    let Some(c) = crate::sandbox::cloud::ctx(&tb) else {
        return json!({"marked": false, "reason": "the task does not run in a cloud box"});
    };
    let script = crate::sandbox::cloud::mark_synced_script(&c.workdir, fp);
    let argv = vec!["/bin/sh".into(), "-c".into(), script];
    match crate::sandbox::cloud::exec_capture(server, c, &argv, vec![], Duration::from_secs(60))
        .await
    {
        Ok(o) if o.code == 0 => json!({"marked": true}),
        // A bundle never carries submodule work: the box keeps it as unsynced.
        Ok(o) if o.code == crate::sandbox::cloud::MARK_SUBMODULES_EXIT => {
            json!({"marked": false, "reason": "submodule changes were not carried; the box keeps them as unsynced work"})
        }
        Ok(o) => {
            json!({"marked": false, "reason": format!("recording the synced tree failed: {}", tail(&o.stderr))})
        }
        Err(e) => json!({"marked": false, "reason": e.message}),
    }
}

/// Release the box after a bring-back. A refused suspend or destroy (unsynced work the bundle
/// did not carry, say) keeps the box instead; the result says so.
async fn release(server: &Arc<Server>, task: &str, after: &str) -> Result<Value, RpcError> {
    // `release_task` detaches the box from the task (later panes of the task spawn on the
    // host), closes the task's box panes, then keeps, suspends or destroys the remote box.
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
    let (bundle, task, fp, _freeze) = export_from_box(server, job, pane).await?;
    let _cleanup = Cleanup(bundle.clone());

    // The user sees what comes back, and the warning, before anything is applied.
    let mut code_only = job.code_only;
    if job.review {
        let summary = review_summary(server, id, &bundle).await?;
        note(server, id, json!({"review": summary}));
        step(server, id, "awaiting_confirmation")?;
        code_only = wait_confirmed(server, id).await?;
    }

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
    let synced = mark_brought_back(
        server,
        &task,
        fp.as_deref(),
        imp.manifest.skipped.len(),
        imp.not_written.len(),
    )
    .await;
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
    // Code only: the files are here, the box's transcript is not resumed (the pane is a shell).
    let run = match &imp.resume_argv {
        Some(argv) if !code_only => {
            match resume_in(
                server,
                &host_pane.id,
                Some(task.as_str()),
                argv,
                imp.resumed,
                Some(vk_handoff::RESUMED_FROM_BOX_NOTE),
            )
            .await
            {
                Ok(r) => r,
                Err(e) => Some(json!({"agent_error": error_json(&e)})),
            }
        }
        _ => None,
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
        "resumed": imp.resumed && !code_only,
        "code_only": code_only,
        "run": run.as_ref().and_then(|r| r.get("id")).cloned(),
        "not_written": imp.not_written,
        "skipped": imp.manifest.skipped,
        "box_synced": synced,
        "release": released,
    }))
}

/// Bring back to a paired host: the gateway delivers the bundle as a handoff.
async fn bring_back_peer(server: &Arc<Server>, job: &Job, pane: &str, peer: &str) -> R {
    let id = job.id.as_str();
    let (bundle, task, _fp, _freeze) = export_from_box(server, job, pane).await?;
    // What the export left out; the gateway takes the file, so read it now.
    let b2 = bundle.clone();
    let skipped = match tokio::task::spawn_blocking(move || vk_handoff::read_manifest(&b2)).await {
        Ok(Ok(m)) => m.skipped.len(),
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&bundle);
            return Err(internal(format!("the box's bundle has no manifest: {e}")));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&bundle);
            return Err(internal(e));
        }
    };
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
    // Whether the peer's import wrote everything is not known here, so the box keeps its
    // uncommitted work as unsynced: destroying it needs `force` (or a later bring-back here).
    let synced = json!({"marked": false, "reason": format!(
        "the paired host's import is not confirmed here ({skipped} file(s) were not exported); the box keeps its uncommitted work as unsynced"
    )});
    let released = release(server, &task, &after_of(job)).await?;
    Ok(json!({
        "task": task,
        "peer_job": hjob,
        "incoming": delivered.incoming,
        "incoming_state": delivered.incoming_state,
        "box_synced": synced,
        "release": released,
    }))
}

// ---------------------------------------------------------------------------------------------
// schema

/// Schema registry entries (`api_schema` loads them next to its own tables).
pub const SHAPES: &str = r##"
# --- cloud moves (spec 17 §7): server jobs; full scope, never from a pane (a pane asks with auth.approve) ---
cloud.move :: {pane?: Target, run?: string, box?: string, to: {kind: cloud|local|peer, provider?: string, box?: string, peer?: string}, interrupt?: bool = false, source_after?: keep|suspend|destroy, new_task?: bool = false, review?: bool = true, code_only?: bool = false}
  => {job: {id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|awaiting_confirmation|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, new_task: bool, review: bool, code_only: bool, confirmed: bool, created_at: int, updated_at: int}}
# newest first; finished jobs for 7 days
cloud.jobs :: {}
  => {jobs: [{id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|awaiting_confirmation|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, new_task: bool, review: bool, code_only: bool, confirmed: bool, created_at: int, updated_at: int}]}
# a local bring-back waiting in awaiting_confirmation goes on (code_only: no agent resume)
cloud.confirm :: {id: string, code_only?: bool}
  => {job: {id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|awaiting_confirmation|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, new_task: bool, review: bool, code_only: bool, confirmed: bool, created_at: int, updated_at: int}}
cloud.cancel :: {id: string}
  => {job: {id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|awaiting_confirmation|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, new_task: bool, review: bool, code_only: bool, confirmed: bool, created_at: int, updated_at: int}}
"##;

pub const EVENTS: &str = r##"
cloud.job :: {job: string} => {id: string, direction: send|bring_back, pane?: string, run?: string, box?: string, from: object, to: object, state: queued|waiting_turn|creating|bootstrapping|exporting|awaiting_confirmation|uploading|importing|resuming|done|failed|cancelled, progress?: {done: int, total: int}, error?: {kind: string, message: string, details?: any}, result?: object, interrupt: bool, source_after?: keep|suspend|destroy, task?: string, by?: string, new_task: bool, review: bool, code_only: bool, confirmed: bool, created_at: int, updated_at: int}
"##;

#[cfg(test)]
#[path = "cloud_move_tests.rs"]
mod tests;
