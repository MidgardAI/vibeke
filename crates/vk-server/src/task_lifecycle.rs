//! Task lifecycle methods beyond create/park/finish (05 §1, §4, §6, §7, §8; 07 §2.10):
//!
//! - `task.setup_log {task, max_bytes?}`: the tail of the task's setup log
//!   (`<worktree>/.vibeke/setup.log`).
//! - `task.archive {task, force?}`: stop the agents (recording resume handles, as `task.park`),
//!   delete the worktree directory, keep the branch, release the ports; the task record, its
//!   events and transcripts stay. A dirty checkout or unpushed commits refuse without `force`.
//!   An attached task (15 §4.3) only changes its record.
//! - `task.adopt {path | pane, title?, focus?}`: record an existing git worktree as an owned
//!   task. Nothing moves: no checkout, branch or setup is created; a port block is leased and
//!   the workspace rooted at the worktree (the pane's workspace with `pane`) gets the task.
//! - `task.recreate {task}` / `task.forget {task, force?}`: the two answers to a task the
//!   reconcile loop marked `missing`. Recreate adds the worktree back at its recorded path from
//!   the kept branch; forget closes the record (ports released, previews retired) and never
//!   touches the disk.
//! - `task.ports {task}` / `task.ports.re_lease {task}`: the leased block and the env it maps
//!   to; re-lease moves the task to another block of the same size (after `EADDRINUSE`).
//!   Running panes keep the old env; new panes get the new one.
//! - The "PR merged" cleanup hint: when a task's cached pull request (`task.pr`) is first seen
//!   merged, `task.cleanup_suggested {reason: pr_merged}` and a notification suggest finishing
//!   and removing it (`tasks.cleanup.merged_branches = suggest`; never automatic).
//!
//! `task.archive|adopt|recreate|forget|ports.re_lease` are full scope only; `task.setup_log`
//! and `task.ports` are readable from a pane for tasks of its own workspace.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s, u};
use crate::core::{Tx, ulid};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};

pub const METHODS: &[(&str, bool)] = &[
    ("task.setup_log", false),
    ("task.archive", true),
    ("task.adopt", true),
    ("task.recreate", true),
    ("task.forget", true),
    ("task.ports", false),
    ("task.ports.re_lease", true),
];

/// Default and largest `max_bytes` of `task.setup_log`.
const LOG_DEFAULT: u64 = 256 * 1024;
const LOG_MAX: u64 = 4 * 1024 * 1024;

fn conflict(reason: &str, msg: impl Into<String>) -> RpcError {
    err(ErrorKind::Conflict, format!("{reason}: {}", msg.into())).details(json!({"reason": reason}))
}

fn task_of(server: &Server, p: &Value) -> Result<Task, RpcError> {
    let t = req(p, "task")?;
    server
        .with_core(|c| c.task(t).cloned())
        .ok_or_else(|| not_found("task", t))
}

/// Pane-scoped callers may read only tasks of their own workspace.
fn visible_to(server: &Server, ctx: &Ctx, task: &Task) -> Result<(), RpcError> {
    let Some(scope) = &ctx.pane_scope else {
        return Ok(());
    };
    let ws = server.with_core(|c| c.pane(scope).map(|p| p.workspace.clone()));
    if ws.is_some() && ws == task.workspace {
        Ok(())
    } else {
        Err(not_found("task", &task.handle))
    }
}

fn commit(server: &Server, tx: Tx) -> Result<Option<i64>, RpcError> {
    let mut c = server.core.lock().unwrap();
    let events = server.commit(&mut c, tx).map_err(internal)?;
    Ok(events.last().map(|e| e.seq))
}

fn pool() -> vk_tasks::PortPool {
    let cfg = crate::task_workspace::load_tasks_cfg();
    vk_tasks::PortPool {
        start: cfg.port_pool.start,
        end: cfg.port_pool.end,
        block: cfg.port_block,
    }
}

fn leases() -> vk_tasks::PortLeases {
    vk_tasks::PortLeases::new(crate::paths::state_root(), pool())
}

// ---- task.setup_log -------------------------------------------------------------------------

fn setup_log(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task = task_of(server, p)?;
    visible_to(server, ctx, &task)?;
    let wt = task
        .worktree_path
        .clone()
        .ok_or_else(|| conflict("no_checkout", "the task has no checkout"))?;
    let path = Path::new(&wt).join(".vibeke/setup.log");
    let max = u(p, "max_bytes").unwrap_or(LOG_DEFAULT).clamp(1, LOG_MAX);
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({
                "task": task.id, "path": path, "exists": false, "text": "", "size": 0,
                "truncated": false, "setup_status": task.setup_status,
            }));
        }
        Err(e) => return Err(internal(e)),
    };
    let size = f.metadata().map_err(internal)?.len();
    let start = size.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).map_err(internal)?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).map_err(internal)?;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    // Start at a line boundary when the head was cut.
    if start > 0
        && let Some(i) = text.find('\n')
    {
        text.drain(..=i);
    }
    Ok(json!({
        "task": task.id,
        "path": path,
        "exists": true,
        "text": text,
        "size": size,
        "truncated": start > 0,
        "setup_status": task.setup_status,
    }))
}

// ---- task.archive ---------------------------------------------------------------------------

async fn archive(server: &Arc<Server>, p: &Value) -> R {
    let task = task_of(server, p)?;
    let force = b(p, "force").unwrap_or(false);
    let user = crate::drafts::user_ctx();
    if task.ownership == TaskOwnership::Attached {
        let mut r = Box::pin(crate::api::dispatch(
            server,
            &user,
            "task.finish",
            &json!({"task": task.id, "status": "archived"}),
        ))
        .await?;
        r["archived"] = json!(true);
        return Ok(r);
    }
    let kind = task.checkout.clone().unwrap_or_else(|| "worktree".into());
    let wt = task.worktree_path.clone().filter(|_| kind != "none");
    // protect_dirty (05 §8): checked before anything stops.
    if let Some(w) = &wt
        && Path::new(w).exists()
        && !force
    {
        let blockers =
            vk_tasks::removal_blockers(Path::new(w)).map_err(|e| invalid(e.to_string()))?;
        if !blockers.is_empty() {
            return Err(err(
                ErrorKind::Conflict,
                "dirty_checkout: the worktree has uncommitted changes or unpushed commits (archive with force to delete it anyway)",
            )
            .details(json!({"reason": "dirty_checkout", "dirty_files": blockers.dirty_files, "unpushed_commits": blockers.unpushed_commits})));
        }
    }
    // Stop the agents and record their resume handles (the park record).
    let mut stopped = json!([]);
    if task.status == "active" {
        let r = Box::pin(crate::api::dispatch(
            server,
            &user,
            "task.park",
            &json!({"task": task.id}),
        ))
        .await?;
        stopped = r["stopped"].clone();
    }
    let path_exists = wt.as_deref().is_some_and(|w| Path::new(w).exists());
    let r = Box::pin(crate::api::dispatch(
        server,
        &user,
        "task.finish",
        &json!({"task": task.id, "remove_worktree": path_exists, "archive": true, "force": force}),
    ))
    .await?;
    let seq = commit(server, {
        let mut tx = Tx::new();
        tx.event(
            "task.archived",
            json!({"task": task.id}),
            json!({"path": wt, "branch": task.branch, "job": r["job"], "runs": stopped.as_array().map_or(0, Vec::len)}),
        );
        tx
    })?;
    Ok(json!({
        "task": r["task"],
        "job": r["job"],
        "stopped": stopped,
        "branch_kept": task.branch,
        "worktree_removed": path_exists,
        "cursor": crate::api::cursor(server, seq),
    }))
}

// ---- task.adopt -----------------------------------------------------------------------------

fn adopt(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let (path, pane) = match (s(p, "path"), s(p, "pane")) {
        (Some(path), _) => (PathBuf::from(path), None),
        (None, Some(t)) => {
            let pane = crate::api::resolve_pane(server, ctx, Some(t))?;
            let cwd = server
                .pane_cwd(&pane.id)
                .ok_or_else(|| conflict("no_cwd", "the pane's working directory is unknown"))?;
            (PathBuf::from(cwd), Some(pane))
        }
        (None, None) => return Err(invalid("path or pane is required")),
    };
    let info = vk_tasks::repo_root(&path).ok_or_else(|| {
        conflict(
            "not_a_repo",
            format!("{} is not inside a git repository", path.display()),
        )
    })?;
    let entry = vk_tasks::find_worktree(&info.root, &info.worktree_root)
        .map_err(|e| conflict("not_a_worktree", e.to_string()))?;
    let root = entry
        .path
        .canonicalize()
        .unwrap_or_else(|_| entry.path.clone());
    let root_s = root.to_string_lossy().into_owned();
    // One owned task per checkout.
    if let Some(t) = server.with_core(|c| {
        c.model
            .tasks
            .iter()
            .find(|t| {
                t.worktree_path.as_deref().is_some_and(|w| {
                    Path::new(w)
                        .canonicalize()
                        .unwrap_or_else(|_| PathBuf::from(w))
                        == root
                })
            })
            .cloned()
    }) {
        return Err(conflict(
            "already_tracked",
            format!("task {} already owns {}", t.handle, root.display()),
        )
        .details(json!({"reason": "already_tracked", "task": t.id, "handle": t.handle})));
    }
    let id = ulid();
    let slug = vk_tasks::slugify(
        s(p, "slug").unwrap_or_else(|| {
            root.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("adopted")
        }),
        vk_tasks::DEFAULT_SLUG_MAX,
    );
    let title = s(p, "title")
        .map(str::to_string)
        .or_else(|| entry.branch.clone())
        .unwrap_or_else(|| slug.clone());
    let mut warnings: Vec<String> = vec![];
    let lease = match leases().lease(&vk_tasks::LeaseRequest {
        task_id: id.clone(),
        session: server.opts.session.clone(),
        owner_pid: None,
    }) {
        Ok(l) => Some(l),
        Err(e) => {
            warnings.push(format!("no ports leased: {e}"));
            None
        }
    };
    // The workspace: the pane's, else one already rooted at the checkout, else a new one.
    let existing = match &pane {
        Some(pn) => server.with_core(|c| c.ws(&pn.workspace).cloned()),
        None => server.with_core(|c| {
            c.model
                .workspaces
                .iter()
                .find(|w| {
                    let r = Path::new(&w.root_path);
                    r.canonicalize().unwrap_or_else(|_| r.to_path_buf()) == root
                })
                .cloned()
        }),
    };
    if let Some(w) = &existing
        && let Some(other) = &w.task
    {
        let _ = lease.as_ref().map(|_| leases().release(&id));
        return Err(conflict(
            "workspace_has_task",
            format!("workspace {} already belongs to a task", w.handle),
        )
        .details(json!({"reason": "workspace_has_task", "task": other})));
    }
    let created = existing.is_none();
    let ws = match existing {
        Some(w) => w,
        None => {
            let focus = (b(p, "focus") == Some(true) && ctx.pane_scope.is_none())
                .then_some(ctx.client_id.as_str());
            server
                .create_workspace_for(&root_s, Some(slug.clone()), None, focus, Some(&id))
                .map_err(internal)?
                .0
        }
    };
    let main = entry.is_main;
    let handle = server.with_core(|c| c.next_task_handle());
    let task = Task {
        id: id.clone(),
        handle,
        title: title.clone(),
        slug,
        workspace: Some(ws.id.clone()),
        repo_root: info.root.to_string_lossy().into_owned(),
        worktree_path: Some(root_s.clone()),
        branch: entry.branch.clone(),
        base_ref: None,
        port_range: lease.as_ref().map(|l| (l.start, l.end)),
        status: "active".into(),
        created_at_ms: vk_store::now_ms(),
        owner_machine: server.opts.machine.clone(),
        // The main working tree is shared with the user's own checkout: never removed.
        checkout: Some(if main { "none" } else { "worktree" }.into()),
        ..Default::default()
    };
    let seq = {
        let mut c = server.core.lock().unwrap();
        let mut w = ws.clone();
        w.task = Some(id.clone());
        w.branch = entry.branch.clone();
        let mut tx = Tx::new();
        tx.ws(w);
        tx.task(task.clone());
        tx.counters = true;
        tx.event(
            "task.created",
            json!({"task": id, "workspace": ws.id}),
            json!({"title": title, "branch": entry.branch, "path": root_s}),
        );
        tx.event(
            "task.adopted",
            json!({"task": id, "workspace": ws.id}),
            json!({"path": root_s, "branch": entry.branch, "via": if pane.is_some() { "pane" } else { "path" }, "main_worktree": main, "created_workspace": created}),
        );
        let events = server.commit(&mut c, tx).map_err(internal)?;
        events.last().map(|e| e.seq)
    };
    let ws = server.with_core(|c| c.ws(&ws.id).cloned()).unwrap_or(ws);
    Ok(json!({
        "task": task,
        "workspace": ws,
        "created_workspace": created,
        "warnings": warnings,
        "cursor": crate::api::cursor(server, seq),
    }))
}

// ---- task.recreate / task.forget ------------------------------------------------------------

async fn recreate(server: &Arc<Server>, p: &Value) -> R {
    let task = task_of(server, p)?;
    if task.ownership == TaskOwnership::Attached {
        return Err(conflict(
            "attached",
            "an attached task has no checkout of its own",
        ));
    }
    if task.status != "missing" {
        return Err(conflict(
            "not_missing",
            format!(
                "the task is {}; only a missing task is recreated",
                task.status
            ),
        ));
    }
    let path = PathBuf::from(
        task.worktree_path
            .clone()
            .ok_or_else(|| conflict("no_checkout", "the task has no recorded checkout"))?,
    );
    let branch = task
        .branch
        .clone()
        .ok_or_else(|| conflict("no_branch", "the task has no branch to recreate from"))?;
    let repo = PathBuf::from(&task.repo_root);
    let (p2, b2) = (path.clone(), branch.clone());
    let co = tokio::task::spawn_blocking(move || vk_tasks::recreate_worktree(&repo, &p2, &b2))
        .await
        .map_err(internal)?
        .map_err(|e| conflict("recreate_failed", e.to_string()))?;
    let path_s = co.path.to_string_lossy().into_owned();
    // The workspace: kept when it still exists, else a new one at the checkout.
    let ws_alive = task
        .workspace
        .as_deref()
        .is_some_and(|w| server.with_core(|c| c.ws(w).is_some()));
    let mut t2 = task.clone();
    t2.status = "active".into();
    t2.rev += 1;
    if !ws_alive {
        let (ws, _, _) = server
            .create_workspace_for(&path_s, Some(task.slug.clone()), None, None, Some(&task.id))
            .map_err(internal)?;
        let mut w = ws.clone();
        w.task = Some(task.id.clone());
        w.branch = Some(branch.clone());
        t2.workspace = Some(ws.id.clone());
        commit(server, {
            let mut tx = Tx::new();
            tx.ws(w);
            tx
        })?;
    }
    let seq = commit(server, {
        let mut tx = Tx::new();
        tx.event(
            "task.status_changed",
            json!({"task": t2.id}),
            json!({"status": "active"}),
        );
        tx.event(
            "task.recreated",
            json!({"task": t2.id}),
            json!({"path": path_s, "branch": branch, "new_workspace": !ws_alive}),
        );
        tx.task(t2.clone());
        tx
    })?;
    Ok(
        json!({"task": t2, "path": path_s, "branch": branch, "new_workspace": !ws_alive, "cursor": crate::api::cursor(server, seq)}),
    )
}

fn forget(server: &Arc<Server>, p: &Value) -> R {
    let task = task_of(server, p)?;
    let force = b(p, "force").unwrap_or(false);
    if !force && !matches!(task.status.as_str(), "missing" | "finished") {
        return Err(conflict(
            "task_live",
            format!(
                "the task is {}; forget is for missing or finished tasks (force to forget anyway)",
                task.status
            ),
        ));
    }
    if task.ownership == TaskOwnership::Owned {
        let _ = leases().release(&task.id);
        crate::preview_fabric::retire_task_previews(server, &task.id);
    }
    let mut t2 = task.clone();
    t2.status = "forgotten".into();
    t2.rev += 1;
    let seq = commit(server, {
        let mut tx = Tx::new();
        if let Some(ws) = &task.workspace
            && let Some(mut w) = server.with_core(|c| c.ws(ws).cloned())
            && w.task.as_deref() == Some(task.id.as_str())
        {
            w.task = None;
            tx.ws(w);
        }
        tx.event(
            "task.status_changed",
            json!({"task": t2.id}),
            json!({"status": "forgotten"}),
        );
        tx.event(
            "task.forgotten",
            json!({"task": t2.id}),
            json!({"path": task.worktree_path, "branch": task.branch, "previous_status": task.status}),
        );
        tx.task(t2.clone());
        tx
    })?;
    Ok(json!({"task": t2, "files_touched": false, "cursor": crate::api::cursor(server, seq)}))
}

// ---- task.ports -----------------------------------------------------------------------------

fn ports_json(task: &Task) -> Value {
    let lease = task
        .port_range
        .map(|(a, z)| json!({"start": a, "end": z, "count": z - a + 1}));
    let mut env = serde_json::Map::new();
    if let Some((a, z)) = task.port_range {
        env.insert("VIBEKE_PORT_BASE".into(), json!(a.to_string()));
        env.insert("VIBEKE_PORT_END".into(), json!(z.to_string()));
    }
    env.insert("VIBEKE_TASK".into(), json!(task.handle));
    env.insert("VIBEKE_TASK_SLUG".into(), json!(task.slug));
    if let Some(w) = &task.worktree_path {
        env.insert("VIBEKE_WORKTREE".into(), json!(w));
    }
    for (k, v) in crate::preview_fabric::task_port_env(task) {
        env.insert(k, json!(v));
    }
    json!({"task": task.id, "handle": task.handle, "lease": lease, "env": env})
}

fn ports(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task = task_of(server, p)?;
    visible_to(server, ctx, &task)?;
    Ok(ports_json(&task))
}

fn re_lease(server: &Arc<Server>, p: &Value) -> R {
    let task = task_of(server, p)?;
    if task.ownership == TaskOwnership::Attached {
        return Err(conflict("attached", "an attached task has no port lease"));
    }
    let req = vk_tasks::LeaseRequest {
        task_id: task.id.clone(),
        session: server.opts.session.clone(),
        owner_pid: None,
    };
    let (old, new) = leases()
        .re_lease(&req, pool().block)
        .map_err(|e| conflict("ports_exhausted", e.to_string()))?;
    let mut t2 = task.clone();
    t2.port_range = Some((new.start, new.end));
    t2.rev += 1;
    let seq = commit(server, {
        let mut tx = Tx::new();
        tx.event(
            "task.ports_changed",
            json!({"task": t2.id}),
            json!({"old": old.as_ref().map(|l| json!([l.start, l.end])), "new": [new.start, new.end]}),
        );
        tx.task(t2.clone());
        tx
    })?;
    let mut out = ports_json(&t2);
    out["old_lease"] = old.map_or(Value::Null, |l| json!({"start": l.start, "end": l.end}));
    out["note"] = json!(
        "running panes keep the old ports in their environment; restart dev servers in a new pane of the task"
    );
    out["cursor"] = crate::api::cursor(server, seq);
    Ok(out)
}

// ---- PR merged cleanup hint -----------------------------------------------------------------

const PR_KV: &str = "task_pr_merged";

/// A task's PR lookup came in: the first time it says merged, suggest cleaning up.
pub fn note_pr(server: &Server, task: &Task, lookup: &vk_tasks::PrLookup) {
    let vk_tasks::PrLookup::Pr { pr } = lookup else {
        return;
    };
    if pr.state != "MERGED"
        || task.ownership != TaskOwnership::Owned
        || matches!(task.status.as_str(), "finished" | "archived" | "forgotten")
    {
        return;
    }
    let mut c = server.core.lock().unwrap();
    if c.store.kv_get(PR_KV, &task.id).ok().flatten().as_deref()
        == Some(pr.number.to_string().as_str())
    {
        return;
    }
    let hint = format!("vibeke task finish {} --remove-worktree", task.handle);
    let mut tx = Tx::new();
    tx.event(
        "task.cleanup_suggested",
        json!({"task": task.id}),
        json!({"reason": "pr_merged", "pr": pr.number, "url": pr.url, "hint": hint}),
    );
    tx.m.kv(PR_KV, &task.id, Some(pr.number.to_string()));
    if server.commit(&mut c, tx).is_err() {
        return;
    }
    drop(c);
    server.notify(
        "task",
        None,
        &format!("PR #{} merged", pr.number),
        &format!("Task {} can be finished and removed: {hint}", task.handle),
        "low",
    );
}

/// Check every owned task's cached PR (never runs `gh`; the cache is filled by `task.pr`).
pub fn pr_sweep(server: &Server) {
    let tasks: Vec<Task> = server.with_core(|c| {
        c.model
            .tasks
            .iter()
            .filter(|t| t.ownership == TaskOwnership::Owned && t.status == "active")
            .cloned()
            .collect()
    });
    for t in tasks {
        if let Some(v) = crate::task_workspace::pr_peek(&t)
            && let Ok(lookup) = serde_json::from_value::<PrPeek>(v)
            && let Some(l) = lookup.into_lookup()
        {
            note_pr(server, &t, &l);
        }
    }
}

/// The serialized `PrLookup` read back (it only derives `Serialize`).
#[derive(serde::Deserialize)]
struct PrPeek {
    kind: String,
    pr: Option<PrPeekStatus>,
}

#[derive(serde::Deserialize)]
struct PrPeekStatus {
    number: u64,
    state: String,
    url: String,
}

impl PrPeek {
    fn into_lookup(self) -> Option<vk_tasks::PrLookup> {
        let p = self.pr.filter(|_| self.kind == "pr")?;
        Some(vk_tasks::PrLookup::Pr {
            pr: vk_tasks::PrStatus {
                number: p.number,
                state: p.state,
                is_draft: false,
                review_decision: None,
                checks: vk_tasks::ChecksState::None,
                url: p.url,
                label: String::new(),
            },
        })
    }
}

/// Dispatch hook for the methods above.
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "task.setup_log" => setup_log(server, ctx, p),
        "task.archive" => archive(server, p).await,
        "task.adopt" => adopt(server, ctx, p),
        "task.recreate" => recreate(server, p).await,
        "task.forget" => forget(server, p),
        "task.ports" => ports(server, ctx, p),
        "task.ports.re_lease" => re_lease(server, p),
        _ => return None,
    })
}

#[cfg(test)]
#[path = "task_lifecycle_tests.rs"]
mod tests;
