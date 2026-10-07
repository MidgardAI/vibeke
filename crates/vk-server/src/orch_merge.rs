//! Merge orchestration (12, 05 §10): `task.claim*`, `merge.predict`, `merge.queue.*`.
//!
//! Claims are advisory globs a task declares for itself (agents may claim for their own task);
//! prediction compares live worktrees (and the claims); the queue lands task branches one at a
//! time through an integration worktree (`vk_orchestrate::merge`). The queue never touches a
//! checkout with uncommitted changes, and a failed check, conflict or dirty target leaves
//! the target branch exactly as it was.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s, u};
use crate::core::Tx;
use crate::orch::{self, from_orch, kv_get, kv_put, load_all, load_one, put, require};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use vk_orchestrate::merge::{self as mg, Claim, EntryState, MergeOutcome, MergeQueue, QueueEntry};
use vk_orchestrate::{OrchestrateConfig, gitx};
use vk_proto::model::Task;
use vk_proto::rpc::ErrorKind;

const CLAIM: &str = "orch_claim";
const QUEUE: &str = "orch_queue";

pub fn claims(server: &Server) -> Vec<Claim> {
    load_all(server, CLAIM)
}

fn queue(server: &Server) -> MergeQueue {
    load_one(server, QUEUE, "main").unwrap_or_default()
}

type QueueEvents = Vec<(&'static str, Value, Value)>;

/// Change the stored queue as one step under the core lock: read the *current* queue, apply
/// `f`, save it with `f`'s events. Nothing saved from an older snapshot can overwrite entries
/// added, cancelled or requeued in the meantime (e.g. while a merge was running).
fn update_queue<T>(
    server: &Server,
    f: impl FnOnce(&mut MergeQueue) -> Result<(T, QueueEvents), vk_proto::rpc::RpcError>,
) -> Result<T, vk_proto::rpc::RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut q: MergeQueue = c
        .store
        .find(QUEUE, "main")
        .ok()
        .flatten()
        .unwrap_or_default();
    let (out, events) = f(&mut q)?;
    let mut tx = Tx::new();
    put(&mut tx, QUEUE, "main", Some("main"), &q);
    for (k, s, d) in events {
        tx.event(k, s, d);
    }
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(out)
}

fn gate(server: &Server) -> Result<OrchestrateConfig, vk_proto::rpc::RpcError> {
    let c = orch::cfg(server);
    require(c.merge.enabled, "merge orchestration", "merge")?;
    Ok(c)
}

fn task_of(server: &Server, t: &str) -> Result<Task, vk_proto::rpc::RpcError> {
    server
        .with_core(|c| c.task(t).cloned())
        .ok_or_else(|| not_found("task", t))
}

/// The task a pane-scoped caller belongs to (its workspace's task).
fn caller_task(server: &Server, ctx: &Ctx) -> Option<String> {
    let pane = ctx.pane_scope.as_deref()?;
    server.with_core(|c| {
        let ws = c.pane(pane)?.workspace.clone();
        c.model
            .tasks
            .iter()
            .find(|t| t.workspace.as_deref() == Some(ws.as_str()))
            .map(|t| t.id.clone())
    })
}

fn owns(server: &Server, ctx: &Ctx, task: &str) -> Result<(), vk_proto::rpc::RpcError> {
    if ctx.pane_scope.is_none() || caller_task(server, ctx).as_deref() == Some(task) {
        return Ok(());
    }
    Err(err(
        ErrorKind::PermissionDenied,
        "a pane may only claim for its own task",
    )
    .details(json!({"scope": "pane"})))
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "task.claim" => claim_add(server, ctx, p).await,
        "task.claim.list" => claim_list(server, p),
        "task.claim.remove" => claim_remove(server, ctx, p),
        "merge.predict" => predict(server, p).await,
        "merge.queue.add" => queue_add(server, p).await,
        "merge.queue.list" => queue_list(server, p),
        "merge.queue.cancel" => queue_edit(server, p, true),
        "merge.queue.requeue" => queue_edit(server, p, false),
        "merge.queue.run" => queue_run(server, p).await,
        _ => return None,
    })
}

// ---- claims ---------------------------------------------------------------------------------

async fn claim_add(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    // A run that belongs to no task claims for itself (collision tracker, 05 §10, 3A).
    if let Some(r) = crate::collision::run_claim_hook(server, ctx, p) {
        return r;
    }
    let glob = s(p, "glob")
        .or_else(|| s(p, "path"))
        .ok_or_else(|| invalid("missing param `glob`"))?;
    let task_ref: String = match (s(p, "task"), s(p, "run")) {
        (Some(t), _) => t.to_string(),
        (None, Some(r)) => server
            .with_core(|c| c.run(r).and_then(|x| x.task.clone()))
            .ok_or_else(|| invalid(format!("run {r} does not belong to a task")))?,
        _ => return Err(invalid("missing param `task` (or `run`)")),
    };
    let task = task_of(server, &task_ref)?;
    mg::validate_claim_glob(glob).map_err(from_orch)?;
    owns(server, ctx, &task.id)?;
    gate(server)?;
    let claim = Claim {
        id: format!("c-{}", &crate::core::ulid().to_lowercase()[16..]),
        task: task.id.clone(),
        glob: glob.trim().to_string(),
        note: s(p, "note").map(|n| n.chars().take(300).collect()),
        created_at_ms: vk_store::now_ms(),
    };
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        put(&mut tx, CLAIM, &claim.id, Some(&claim.id), &claim);
        tx.event(
            "task.claim_added",
            json!({"task": task.id, "claim": claim.id}),
            json!({"glob": claim.glob, "note": claim.note}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    // Who already changes something under it?
    let conflicts: Vec<Value> = match predict_all(server, None).await {
        Ok(cs) => cs
            .into_iter()
            .filter(|c| matches!(c.kind, mg::ConflictKind::Claim) && c.b == task.handle)
            .map(|c| json!(c))
            .collect(),
        Err(_) => vec![],
    };
    Ok(json!({"claim": claim, "conflicts": conflicts}))
}

fn claim_list(server: &Server, p: &Value) -> R {
    let mut v = claims(server);
    if let Some(t) = s(p, "task") {
        let id = task_of(server, t)?.id;
        v.retain(|c| c.task == id);
    }
    let mut out = json!({"claims": v});
    // Run claims of the collision tracker (05 §10, 3A) are listed too.
    crate::collision::extend_claim_list(server, p, &mut out);
    Ok(out)
}

fn claim_remove(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if let Some(r) = crate::collision::claim_remove_hook(server, ctx, p) {
        return r;
    }
    let id = req(p, "claim")?;
    let Some(c0) = claims(server).into_iter().find(|c| c.id == id) else {
        return Err(not_found("claim", id));
    };
    owns(server, ctx, &c0.task)?;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.close(CLAIM, &c0.id, Some(&c0.id), &c0);
    tx.event(
        "task.claim_removed",
        json!({"task": c0.task, "claim": c0.id}),
        json!({"glob": c0.glob}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"removed": c0.id}))
}

// ---- prediction -----------------------------------------------------------------------------

/// Live tasks with a worktree, optionally limited to `handles`.
fn live_tasks(server: &Server, handles: Option<&[String]>) -> Vec<Task> {
    server.with_core(|c| {
        c.model
            .tasks
            .iter()
            .filter(|t| matches!(t.status.as_str(), "active" | "parked"))
            .filter(|t| t.worktree_path.is_some() && t.checkout.as_deref() != Some("none"))
            .filter(|t| handles.is_none_or(|h| h.iter().any(|x| x == &t.id || x == &t.handle)))
            .cloned()
            .collect()
    })
}

async fn predict_all(
    server: &Arc<Server>,
    handles: Option<&[String]>,
) -> Result<Vec<mg::Conflict>, vk_proto::rpc::RpcError> {
    let tasks = live_tasks(server, handles);
    let claims = claims(server);
    tokio::task::spawn_blocking(move || {
        let mut out = vec![];
        // Per repository: worktrees of different repositories never conflict.
        let mut repos: Vec<String> = tasks.iter().map(|t| t.repo_root.clone()).collect();
        repos.sort();
        repos.dedup();
        for repo in repos {
            let mut changes = vec![];
            for t in tasks.iter().filter(|t| t.repo_root == repo) {
                let wt = Path::new(t.worktree_path.as_deref().unwrap_or_default());
                let Ok((files, committed)) = mg::changed_files(wt, t.base_ref.as_deref()) else {
                    continue;
                };
                changes.push(mg::Changed {
                    task: t.id.clone(),
                    handle: t.handle.clone(),
                    branch: t.branch.clone(),
                    head: gitx::rev_parse(wt, "HEAD").ok(),
                    committed,
                    files,
                });
            }
            let ids: HashSet<&str> = changes.iter().map(|c| c.task.as_str()).collect();
            let repo_claims: Vec<Claim> = claims
                .iter()
                .filter(|c| {
                    ids.contains(c.task.as_str())
                        || tasks.iter().any(|t| t.id == c.task && t.repo_root == repo)
                })
                .cloned()
                .collect();
            out.extend(mg::predict(Path::new(&repo), &changes, &repo_claims));
        }
        out
    })
    .await
    .map_err(internal)
}

async fn predict(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let handles: Option<Vec<String>> = p.get("tasks").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });
    let mut conflicts = predict_all(server, handles.as_deref()).await?;
    if let Some(repo) = s(p, "repo") {
        let tasks: HashSet<String> = server.with_core(|c| {
            c.model
                .tasks
                .iter()
                .filter(|t| t.repo_root == repo)
                .map(|t| t.handle.clone())
                .collect()
        });
        conflicts.retain(|c| tasks.contains(&c.a) || tasks.contains(&c.b));
    }
    let checked: Vec<String> = live_tasks(server, handles.as_deref())
        .into_iter()
        .map(|t| t.handle)
        .collect();
    Ok(json!({"conflicts": conflicts, "tasks": checked, "at_ms": vk_store::now_ms()}))
}

/// Background: announce conflicts that appeared since the last pass (medium and up).
pub async fn tick(server: &Arc<Server>, _c: &OrchestrateConfig) {
    if live_tasks(server, None).len() < 2 && claims(server).is_empty() {
        return;
    }
    let Ok(cs) = predict_all(server, None).await else {
        return;
    };
    let key = |c: &mg::Conflict| format!("{}|{}|{:?}|{:?}", c.a, c.b, c.kind, c.severity);
    let now: Vec<(&mg::Conflict, String)> = cs
        .iter()
        .filter(|c| c.severity >= mg::Severity::Medium)
        .map(|c| (c, key(c)))
        .collect();
    let before: HashSet<String> = kv_get::<Vec<String>>(server, "orch.merge", "predicted")
        .into_iter()
        .collect();
    let mut events = vec![];
    for (c, k) in &now {
        if !before.contains(k) {
            events.push(json!({"a": c.a, "b": c.b, "kind": c.kind, "severity": c.severity, "paths": c.paths, "detail": c.detail}));
        }
    }
    let keys: Vec<String> = now.iter().map(|(_, k)| k.clone()).collect();
    if keys.iter().collect::<HashSet<_>>() == before.iter().collect::<HashSet<_>>() {
        return;
    }
    let mut core = server.core.lock().unwrap();
    let mut tx = Tx::new();
    kv_put(&mut tx, "orch.merge", "predicted", &keys);
    for d in events {
        tx.event(
            "merge.conflict_predicted",
            json!({"a": d["a"], "b": d["b"]}),
            d,
        );
    }
    let _ = server.commit(&mut core, tx);
}

// ---- queue ----------------------------------------------------------------------------------

fn short_branch(base: &str) -> String {
    base.strip_prefix("origin/")
        .or_else(|| base.strip_prefix("refs/heads/"))
        .unwrap_or(base)
        .to_string()
}

async fn queue_add(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let task = task_of(server, req(p, "task")?)?;
    let branch = task
        .branch
        .clone()
        .ok_or_else(|| err(ErrorKind::Conflict, "the task has no branch"))?;
    let repo = task.repo_root.clone();
    let target = s(p, "target")
        .map(str::to_string)
        .or_else(|| Some(c.merge.target.clone()).filter(|t| !t.is_empty()))
        .or_else(|| task.base_ref.as_deref().map(short_branch))
        .unwrap_or_else(|| "main".into());
    let wt = task.worktree_path.clone();
    let (r2, b2) = (repo.clone(), branch.clone());
    let allow_dirty = b(p, "allow_dirty").unwrap_or(false);
    let dirty = tokio::task::spawn_blocking(move || {
        if gitx::rev_parse(Path::new(&r2), &format!("refs/heads/{b2}")).is_err() {
            return Err(format!("branch {b2} does not exist in {r2}"));
        }
        Ok(wt
            .map(|w| {
                gitx::run(
                    Path::new(&w),
                    &["status", "--porcelain", "--untracked-files=no"],
                )
                .map(|o| !o.trim().is_empty())
                .unwrap_or(false)
            })
            .unwrap_or(false))
    })
    .await
    .map_err(internal)?
    .map_err(|e| err(ErrorKind::Conflict, e))?;
    if dirty && !allow_dirty {
        return Err(err(
            ErrorKind::Conflict,
            "the task's worktree has uncommitted changes that would not be merged; commit them or pass allow_dirty",
        )
        .details(json!({"reason": "dirty_worktree"})));
    }
    let entry = QueueEntry {
        id: format!("q-{}", &crate::core::ulid().to_lowercase()[16..]),
        task: task.id.clone(),
        handle: task.handle.clone(),
        repo,
        branch,
        target: target.clone(),
        priority: p.get("priority").and_then(Value::as_i64).unwrap_or(0) as i32,
        state: EntryState::Queued,
        added_at_ms: vk_store::now_ms(),
        updated_at_ms: vk_store::now_ms(),
        note: s(p, "note").map(str::to_string),
        merge_commit: None,
        error: None,
        conflict_paths: vec![],
        check: None,
    };
    let (added, position) = update_queue(server, |q| {
        let added = q.add(entry).map_err(from_orch)?.clone();
        let position = q
            .order()
            .iter()
            .position(|e| e.id == added.id)
            .map(|i| i + 1);
        let ev = vec![(
            "merge.queued",
            json!({"entry": added.id, "task": task.id}),
            json!({"handle": added.handle, "target": target, "priority": added.priority}),
        )];
        Ok(((added, position), ev))
    })?;
    let mut out = json!({"entry": added, "position": position});
    if b(p, "run").unwrap_or(false) {
        out["run"] = queue_run(server, &json!({"entry": added.id})).await?;
    }
    Ok(out)
}

fn queue_list(server: &Server, p: &Value) -> R {
    gate(server)?;
    let q = queue(server);
    let order: Vec<String> = q.order().iter().map(|e| e.id.clone()).collect();
    let entries: Vec<&QueueEntry> = if b(p, "all").unwrap_or(false) {
        q.entries.iter().collect()
    } else {
        q.entries
            .iter()
            .filter(|e| {
                e.state.is_open()
                    || matches!(
                        e.state,
                        EntryState::Conflict
                            | EntryState::CheckFailed
                            | EntryState::Blocked
                            | EntryState::Failed
                    )
            })
            .collect()
    };
    Ok(json!({"entries": entries, "order": order}))
}

fn queue_edit(server: &Arc<Server>, p: &Value, cancel: bool) -> R {
    gate(server)?;
    let which = s(p, "entry")
        .or_else(|| s(p, "task"))
        .ok_or_else(|| invalid("missing param `entry`"))?;
    let e = update_queue(server, |q| {
        if cancel {
            q.cancel(which).map_err(from_orch)?;
        } else {
            q.requeue(which).map_err(from_orch)?;
        }
        let e = q.get(which).cloned();
        let kind = if cancel {
            "merge.cancelled"
        } else {
            "merge.requeued"
        };
        let ev = vec![(
            kind,
            json!({"entry": e.as_ref().map(|e| e.id.clone())}),
            json!({"handle": e.as_ref().map(|e| e.handle.clone())}),
        )];
        Ok((e, ev))
    })?;
    Ok(json!({"entry": e}))
}

fn run_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(Default::default)
}

async fn queue_run(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let _g = run_lock().lock().await;
    let only = s(p, "entry").map(str::to_string);
    let count = if b(p, "all").unwrap_or(false) {
        usize::MAX
    } else {
        u(p, "count").unwrap_or(1) as usize
    };
    let mut results = vec![];
    for _ in 0..count {
        let mut q = queue(server);
        let check = Some(c.merge.queue_check.clone())
            .filter(|x| !x.is_empty())
            .map(|x| (x, c.merge.queue_check_timeout()));
        let (squash, only2) = (c.merge.squash(), only.clone());
        let (q2, ran) = tokio::task::spawn_blocking(move || {
            let r = mg::run_next(&mut q, only2.as_deref(), check, squash);
            (q, r)
        })
        .await
        .map_err(internal)?;
        let Some((id, outcome)) = ran else { break };
        let e = q2.entries.iter().find(|e| e.id == id).cloned().unwrap();
        let (kind, data) = match &outcome {
            Ok(MergeOutcome::Merged {
                commit,
                already,
                via,
            }) => (
                "merge.merged",
                json!({"handle": e.handle, "target": e.target, "commit": commit, "already": already, "via": via}),
            ),
            Ok(MergeOutcome::Conflict { paths }) => (
                "merge.conflict",
                json!({"handle": e.handle, "target": e.target, "paths": paths}),
            ),
            Ok(MergeOutcome::CheckFailed(ch)) => (
                "merge.check_failed",
                json!({"handle": e.handle, "target": e.target, "exit_code": ch.exit_code, "timed_out": ch.timed_out}),
            ),
            Ok(MergeOutcome::Blocked { reason }) => (
                "merge.blocked",
                json!({"handle": e.handle, "target": e.target, "reason": reason}),
            ),
            Err(er) => (
                "merge.failed",
                json!({"handle": e.handle, "target": e.target, "error": er.to_string()}),
            ),
        };
        // Record the outcome on the queue as it is *now*: entries added, cancelled or
        // requeued while the merge ran stay as they are.
        let e = update_queue(server, |cur| {
            let e = match cur.entries.iter_mut().find(|x| x.id == e.id) {
                Some(slot) => {
                    let cancelled = slot.state == EntryState::Cancelled;
                    *slot = e.clone();
                    // Cancelled mid-merge: a merge that landed is a fact; anything else stays
                    // cancelled.
                    if cancelled && e.state != EntryState::Merged {
                        slot.state = EntryState::Cancelled;
                    }
                    slot.clone()
                }
                None => {
                    cur.entries.push(e.clone());
                    e.clone()
                }
            };
            cur.trim(100);
            let ev = vec![(kind, json!({"entry": e.id, "task": e.task}), data.clone())];
            Ok((e, ev))
        })?;
        results.push(json!({"entry": e, "event": kind, "detail": data}));
        // Stop at the first entry that did not land: later ones may depend on it.
        if kind != "merge.merged" || only.is_some() {
            break;
        }
    }
    Ok(json!({"ran": results.len(), "results": results}))
}
