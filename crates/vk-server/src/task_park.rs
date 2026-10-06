//! `task.park` / `task.resume` (07 §2.10) and the `dry_run` previews of the filesystem-touching
//! task methods (07 §5.5: `task.create {dry_run}`, `worktree.remove {dry_run}`).
//!
//! Park stops a task's agents gracefully and keeps everything else: the worktree, the
//! workspace, its shells and the port lease. Each live run is interrupted first, then its
//! harness process gets SIGTERM (an agent started from a shell leaves the shell pane open; an
//! agent that is the pane's own process closes the pane), and the run ends with reason
//! `parked`. The stopped runs are recorded so `task.resume` can restart each from its native
//! session (the `agent.resume` path: typed into its old shell pane when that is free, else a new
//! tab of the task's workspace). Runs without a resume handle are reported in `skipped`. An
//! attached task (15 §4.3) only changes its record: its processes are never stopped.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s};
use crate::core::Tx;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use vk_proto::model::*;
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[("task.park", true), ("task.resume", true)];

const KV: &str = "task_park";

fn task_of(server: &Server, p: &Value) -> Result<Task, vk_proto::rpc::RpcError> {
    let t = req(p, "task")?;
    server
        .with_core(|c| c.task(t).cloned())
        .ok_or_else(|| not_found("task", t))
}

fn set_status(server: &Server, task: &Task, status: &str, data: Value, kv: Option<String>) -> R {
    let mut t = task.clone();
    t.status = status.into();
    t.rev += 1;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "task.status_changed",
        json!({"task": t.id}),
        json!({"status": status}),
    );
    tx.event(
        if status == "parked" {
            "task.parked"
        } else {
            "task.resumed"
        },
        json!({"task": t.id}),
        data,
    );
    tx.m.kv(KV, &t.id, kv);
    tx.task(t.clone());
    let events = server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    Ok(json!({"task": t, "cursor": crate::api::cursor(server, events.last().map(|e| e.seq))}))
}

/// The task's live runs: bound to it, or running in its workspace.
fn live_runs(server: &Server, task: &Task) -> Vec<AgentRun> {
    server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none())
            .filter(|r| {
                r.task.as_deref() == Some(task.id.as_str())
                    || task
                        .workspace
                        .as_ref()
                        .is_some_and(|w| c.pane(&r.pane).is_some_and(|p| &p.workspace == w))
            })
            .cloned()
            .collect()
    })
}

async fn park(server: &Arc<Server>, p: &Value) -> R {
    let task = task_of(server, p)?;
    if task.status == "parked" {
        return Err(
            err(ErrorKind::Conflict, "already_parked: the task is parked")
                .details(json!({"reason": "already_parked"})),
        );
    }
    if !matches!(task.status.as_str(), "active") {
        return Err(err(
            ErrorKind::Conflict,
            format!("task_not_active: the task is {}", task.status),
        )
        .details(json!({"reason": "task_not_active", "status": task.status})));
    }
    if task.ownership == TaskOwnership::Attached {
        // 15 §4.3: only the record changes.
        let mut r = set_status(
            server,
            &task,
            "parked",
            json!({"attached": true, "runs": 0}),
            None,
        )?;
        r["stopped"] = json!([]);
        r["note"] = json!("attached task: processes are unchanged");
        return Ok(r);
    }
    let runs = live_runs(server, &task);
    let ctx = crate::drafts::user_ctx();
    for r in &runs {
        let _ = Box::pin(crate::api::dispatch(
            server,
            &ctx,
            "agent.interrupt",
            &json!({"target": r.id}),
        ))
        .await;
    }
    if !runs.is_empty() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let mut stopped = vec![];
    for r in &runs {
        let status = server
            .pane_rt(&r.pane)
            .and_then(|rt| rt.status.lock().unwrap().clone());
        let pane_closed = match status {
            // An agent started from the pane's shell: stop its process group, keep the shell.
            Some(st) if st.fg_pgid.is_some_and(|g| g != st.child_pid && g > 1) => {
                let g = st.fg_pgid.unwrap_or_default();
                // SAFETY: signalling a process group of a pane this server owns.
                unsafe { libc::killpg(g as i32, libc::SIGTERM) };
                false
            }
            _ => {
                server.close_pane(&r.pane);
                true
            }
        };
        server.agents.end_run(server, &r.id, "parked");
        stopped.push(json!({"run": r.id, "handle": r.handle, "pane": r.pane, "harness": r.harness, "name": r.name, "pane_closed": pane_closed, "resumable": !r.resume_argv.is_empty() || r.harness_session_id.is_some()}));
    }
    // A container task's box stops with it (13 §11: park = stop, resume restores).
    crate::sandbox::extras::on_task_parked(server, &task.id).await;
    let record = json!({"runs": runs.iter().map(|r| &r.id).collect::<Vec<_>>()}).to_string();
    let mut out = set_status(
        server,
        &task,
        "parked",
        json!({"runs": stopped.len()}),
        Some(record),
    )?;
    out["stopped"] = json!(stopped);
    Ok(out)
}

async fn resume(server: &Arc<Server>, p: &Value) -> R {
    let task = task_of(server, p)?;
    if task.status != "parked" {
        return Err(err(
            ErrorKind::Conflict,
            format!("not_parked: the task is {}", task.status),
        )
        .details(json!({"reason": "not_parked", "status": task.status})));
    }
    let ids: Vec<String> = server
        .with_core(|c| c.store.kv_get(KV, &task.id).ok().flatten())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["runs"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let mut resumed = vec![];
    let mut skipped = vec![];
    if task.ownership != TaskOwnership::Attached {
        crate::sandbox::extras::on_task_resumed(server, &task.id).await;
        for id in ids {
            let Some(run) = server.with_core(|c| {
                c.run(&id)
                    .cloned()
                    .or_else(|| c.store.find::<AgentRun>("run", &id).ok().flatten())
            }) else {
                skipped.push(json!({"run": id, "reason": "run record not found"}));
                continue;
            };
            if run.resume_argv.is_empty() && !crate::agents::headless::is_headless(&run) {
                skipped.push(json!({"run": id, "reason": "no resume handle"}));
                continue;
            }
            // Its old pane when that is a free shell, else a new tab in the task's workspace.
            let free = server.with_core(|c| {
                c.pane(&run.pane)
                    .filter(|_| c.run_for_pane(&run.pane).is_none())
                    .map(|p| p.id.clone())
            });
            let pane = match (free, &task.workspace) {
                (Some(p), _) => Some(p),
                (None, Some(ws)) => {
                    let cwd = run.cwd.clone().or_else(|| task.worktree_path.clone());
                    let (_, p) = server
                        .create_tab(ws, cwd.as_deref(), None, None, None)
                        .map_err(internal)?;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Some(p.id)
                }
                (None, None) => None,
            };
            match crate::agents::resume_from(server, run, pane).await {
                Ok(v) => resumed.push(v["run"].clone()),
                Err(e) => skipped.push(json!({"run": id, "reason": e.message})),
            }
        }
    }
    let mut out = set_status(
        server,
        &task,
        "active",
        json!({"runs": resumed.len(), "skipped": skipped.len()}),
        None,
    )?;
    out["resumed"] = json!(resumed);
    out["skipped"] = json!(skipped);
    Ok(out)
}

// ---- dry runs -------------------------------------------------------------------------------

/// `task.create {dry_run: true}`: what would be created, without touching anything.
fn task_create_plan(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let title = req(p, "title")?;
    let repo = s(p, "repo")
        .map(str::to_string)
        .or_else(|| {
            crate::api::resolve_pane(server, ctx, None)
                .ok()
                .and_then(|x| server.pane_cwd(&x.id))
        })
        .unwrap_or_else(|| ".".into());
    let info = crate::parity::resolve_checkout(&repo, p)?;
    let mut cfg = vk_tasks::WorktreeConfig::default();
    if let Ok((c, _)) = vk_config::Config::load(vk_config::config_path())
        && let Ok(r) = vk_tasks::WorktreeRoot::parse(&c.tasks.root)
    {
        cfg.root = r;
        cfg.branch_template = c.tasks.branch_template.clone();
    }
    if let Some(root) = s(p, "root")
        && let Ok(r) = vk_tasks::WorktreeRoot::parse(root)
    {
        cfg.root = r;
    }
    if let Some(t) = s(p, "branch_template") {
        cfg.branch_template = t.to_string();
    }
    let slug = s(p, "slug")
        .map(str::to_string)
        .unwrap_or_else(|| vk_tasks::slugify(title, cfg.slug_max_len));
    let repo_info = vk_tasks::repo_root(&info.root);
    let plan = if info.kind == "none" {
        json!({"checkout": "none", "path": info.root, "branch": repo_info.as_ref().and_then(|i| i.current_branch.clone())})
    } else {
        let user = cfg
            .user
            .clone()
            .unwrap_or_else(|| vk_tasks::user_handle(&info.root));
        let path = vk_tasks::worktree_path(&cfg.root, &info.root, &slug);
        let branch = s(p, "branch")
            .map(str::to_string)
            .unwrap_or_else(|| vk_tasks::render_branch(&cfg.branch_template, &user, &slug));
        let base = s(p, "base")
            .map(str::to_string)
            .or_else(|| repo_info.as_ref().map(vk_tasks::default_base));
        json!({"checkout": info.kind, "path": path, "path_exists": path.exists(), "branch": branch, "base": base, "note": "the slug gets a -N suffix when the path or branch is taken at creation"})
    };
    let agents = p.get("agents").cloned().unwrap_or(json!([]));
    Ok(json!({
        "dry_run": true,
        "title": title,
        "repo_root": info.root,
        "slug": slug,
        "plan": plan,
        "agents": agents,
        "setup": b(p, "setup").unwrap_or(true),
    }))
}

/// `worktree.remove {dry_run: true}`: whether the removal would proceed and what blocks it.
fn worktree_remove_plan(p: &Value) -> R {
    let path = req(p, "path")?;
    let force = b(p, "force").unwrap_or(false);
    let dir = Path::new(path);
    if !dir.exists() {
        return Err(not_found("worktree", path));
    }
    let blockers = vk_tasks::removal_blockers(dir).map_err(|e| invalid(e.to_string()))?;
    let status = vk_tasks::branch_status(dir, None).ok();
    Ok(json!({
        "dry_run": true,
        "path": path,
        "branch": status.as_ref().and_then(|s| s.branch.clone()),
        "dirty_files": blockers.dirty_files,
        "unpushed_commits": blockers.unpushed_commits,
        "would_remove": blockers.is_empty() || force,
        "force": force,
    }))
}

/// Dispatch hook: `task.park|resume`, and `task.create` / `worktree.remove` with `dry_run`.
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let dry = b(p, "dry_run") == Some(true);
    Some(match method {
        "task.park" => park(server, p).await,
        "task.resume" => resume(server, p).await,
        "task.create" if dry => task_create_plan(server, ctx, p),
        "worktree.remove" if dry => worktree_remove_plan(p),
        _ => return None,
    })
}
