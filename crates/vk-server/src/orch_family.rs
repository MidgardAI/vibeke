//! Best-of-N families (05 §12): `task.best_of_n`, `task.compare`, `task.pick`, `family.*`.
//!
//! A family record (`orch_family` entity, handle `k7`) lists its children, each a normal task
//! whose handle is renamed to `k7.<n>`. There is no parent task: the family handle only exists
//! in the record (a phantom task would confuse every task listing).

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s};
use crate::core::Tx;
use crate::orch::{self, call, from_orch, put, require};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use vk_orchestrate::OrchestrateConfig;
use vk_orchestrate::family::{self as fam, Family, FamilyState};
use vk_proto::rpc::ErrorKind;

const KIND: &str = "orch_family";

pub fn families(server: &Server) -> Vec<Family> {
    orch::load_all(server, KIND)
}

pub fn find(server: &Server, id: &str) -> Result<Family, vk_proto::rpc::RpcError> {
    orch::load_one(server, KIND, id).ok_or_else(|| not_found("family", id))
}

/// The family a child task belongs to.
pub fn family_of_task(server: &Server, task: &str) -> Option<Family> {
    families(server)
        .into_iter()
        .find(|f| f.child(task).is_some())
}

fn save(
    server: &Server,
    f: &Family,
    events: Vec<(&str, Value, Value)>,
) -> Result<(), vk_proto::rpc::RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    put(&mut tx, KIND, &f.id, Some(&f.id), f);
    for (k, s, d) in events {
        tx.event(k, s, d);
    }
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "family.list" => Ok(json!({"families": families(server)})),
        "family.get" => family_get(server, p),
        "family.check" => family_check(server, p).await,
        "task.best_of_n" => best_of_n(server, ctx, p).await,
        "task.compare" => compare(server, p).await,
        "task.pick" => pick(server, p).await,
        _ => return None,
    })
}

fn family_get(server: &Server, p: &Value) -> R {
    let f = find(server, req(p, "family")?)?;
    let children: Vec<Value> = f
        .children
        .iter()
        .map(|c| {
            let (task, run) = server.with_core(|core| {
                (
                    core.task(&c.task).cloned(),
                    core.model
                        .runs
                        .iter()
                        .find(|r| r.task.as_deref() == Some(c.task.as_str()) && r.ended_at_ms.is_none())
                        .map(|r| json!({"run": r.id, "execution": r.execution.value.as_str()})),
                )
            });
            json!({"handle": c.handle, "harness": c.harness, "task": task, "run": run, "discarded": c.discarded})
        })
        .collect();
    Ok(json!({"family": f, "children": children}))
}

fn resolve_agents(p: &Value, max: u32) -> Result<Vec<fam::AgentSpec>, vk_proto::rpc::RpcError> {
    match p.get("agents") {
        Some(Value::String(a)) => fam::parse_agents(a, max).map_err(from_orch),
        Some(Value::Array(a)) => {
            let text: Vec<String> = a
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(_) => Some(format!(
                        "{}:{}",
                        v.get("harness").and_then(Value::as_str).unwrap_or(""),
                        v.get("count").and_then(Value::as_u64).unwrap_or(1)
                    )),
                    _ => None,
                })
                .collect();
            fam::parse_agents(&text.join(","), max).map_err(from_orch)
        }
        _ => Err(invalid(
            "missing param `agents` (for example \"claude:2,codex:1\")",
        )),
    }
}

fn read_prompt(p: &Value) -> Result<Option<String>, vk_proto::rpc::RpcError> {
    if let Some(f) = s(p, "prompt_file") {
        let md = std::fs::metadata(f).map_err(|e| invalid(format!("prompt_file {f}: {e}")))?;
        if md.len() > 1 << 20 {
            return Err(invalid("prompt_file is larger than 1 MiB"));
        }
        return Ok(Some(
            std::fs::read_to_string(f).map_err(|e| invalid(format!("prompt_file {f}: {e}")))?,
        ));
    }
    Ok(s(p, "prompt").map(str::to_string))
}

/// The check command: the request, then the config fallback, then a trusted repo file's
/// `[check] command` in `.vibeke/task.toml`.
fn resolve_check(server: &Server, c: &OrchestrateConfig, repo: &Path, p: &Value) -> Option<String> {
    if let Some(x) = s(p, "check_command").filter(|x| !x.is_empty()) {
        return Some(x.to_string());
    }
    if !c.best_of_n.check_command.is_empty() {
        return Some(c.best_of_n.check_command.clone());
    }
    let digest = crate::run::vibeke_dir_digest(repo)?;
    if !crate::run::repo_trusted(server, repo, &digest) {
        return None;
    }
    let text = std::fs::read_to_string(repo.join(".vibeke/task.toml")).ok()?;
    let t: toml::Table = text.parse().ok()?;
    t.get("check")?.get("command")?.as_str().map(str::to_string)
}

const PASS_THROUGH: &[&str] = &[
    "isolate",
    "yolo",
    "network",
    "image",
    "setup",
    "ports",
    "isolation",
    "checkout",
    "confirm_host_yolo",
    "code",
    "devcontainer",
    "build",
    "root",
    "fetch",
];

pub async fn best_of_n(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let c = orch::cfg(server);
    require(c.best_of_n.enabled, "best-of-N", "best_of_n")?;
    let title = req(p, "title")?.to_string();
    let specs = resolve_agents(p, c.best_of_n.max_children)?;
    let plans = fam::plan_children(&specs);
    let prompt = read_prompt(p)?;
    let suffix = s(p, "suffix").map(str::to_string).unwrap_or_else(|| {
        vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c.tasks.best_of_n.suffix)
            .unwrap_or_default()
    });
    let repo = orch::repo_param(server, ctx, p);
    let root = vk_tasks::repo_root(Path::new(&repo))
        .map(|i| i.root)
        .ok_or_else(|| invalid(format!("{repo} is not inside a repository")))?;
    let check_command = resolve_check(server, &c, &root, p);
    let id = server.with_core(|core| core.next_task_handle());
    let slug = vk_tasks::slugify(&title, vk_tasks::DEFAULT_SLUG_MAX);
    let mut family = fam::new_family(
        &id,
        &title,
        &root.to_string_lossy(),
        s(p, "base"),
        prompt.as_deref(),
        &suffix,
        check_command.as_deref(),
    );
    let mut tasks = vec![];
    let mut runs = vec![];
    let mut warnings: Vec<Value> = vec![];
    for plan in &plans {
        let mut params = json!({
            "title": fam::child_title(&title, plan),
            "repo": root,
            "slug": format!("{slug}-{}", plan.index),
        });
        if let Some(b) = s(p, "base") {
            params["base"] = json!(b);
        }
        for k in PASS_THROUGH {
            if let Some(v) = p.get(*k) {
                params[*k] = v.clone();
            }
        }
        let mut agent = json!({"harness": plan.harness});
        if let Some(pr) = fam::child_prompt(prompt.as_deref(), &suffix, plan, plans.len() as u32) {
            agent["prompt"] = json!(pr);
        }
        params["agents"] = json!([agent]);
        match call(server, "task.create", params).await {
            Ok(r) => {
                let task: vk_proto::model::Task =
                    serde_json::from_value(r["task"].clone()).map_err(internal)?;
                let handle = fam::child_handle(&id, plan.index);
                rename_task(server, &task.id, &handle)?;
                let run = r["runs"][0]["id"].as_str().map(str::to_string);
                family.children.push(fam::Child {
                    handle: handle.clone(),
                    task: task.id.clone(),
                    harness: plan.harness.clone(),
                    index: plan.index,
                    ordinal: plan.ordinal,
                    run: run.clone(),
                    discarded: false,
                });
                if let Some(w) = r.get("warnings").and_then(Value::as_array) {
                    warnings.extend(w.iter().cloned());
                }
                runs.push(r["runs"][0].clone());
                tasks.push(server.with_core(|core| core.task(&task.id).cloned()));
            }
            Err(e) => {
                // All or nothing: take back the children already created.
                for ch in &family.children {
                    let _ = call(
                        server,
                        "task.finish",
                        json!({"task": ch.task, "remove_worktree": true, "archive": true, "force": true}),
                    )
                    .await;
                }
                return Err(err(
                    ErrorKind::Conflict,
                    format!(
                        "could not create child {} ({}): {}",
                        plan.index, plan.harness, e.message
                    ),
                )
                .details(json!({"created_then_removed": family.children.len()})));
            }
        }
    }
    let kids: Vec<Value> = family
        .children
        .iter()
        .map(|c| json!({"handle": c.handle, "task": c.task, "harness": c.harness}))
        .collect();
    save(
        server,
        &family,
        vec![(
            "family.created",
            json!({"family": family.id}),
            json!({"title": title, "children": kids, "check_command": family.check_command}),
        )],
    )?;
    Ok(json!({"family": family, "tasks": tasks, "runs": runs, "warnings": warnings}))
}

fn rename_task(server: &Server, task: &str, handle: &str) -> Result<(), vk_proto::rpc::RpcError> {
    let mut c = server.core.lock().unwrap();
    let Some(mut t) = c.task(task).cloned() else {
        return Err(not_found("task", task));
    };
    t.handle = handle.to_string();
    t.rev += 1;
    let mut tx = Tx::new();
    tx.task(t);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

/// A child's inputs from the live task and runs.
fn inputs(server: &Server, f: &Family) -> Vec<fam::ChildInput> {
    server.with_core(|core| {
        f.children
            .iter()
            .filter(|ch| !ch.discarded)
            .filter_map(|ch| {
                let t = core.task(&ch.task)?;
                let state = core
                    .model
                    .runs
                    .iter()
                    .find(|r| {
                        r.task.as_deref() == Some(ch.task.as_str()) && r.ended_at_ms.is_none()
                    })
                    .map(|r| r.execution.value.as_str().to_string())
                    .unwrap_or_else(|| t.status.clone());
                Some(fam::ChildInput {
                    handle: ch.handle.clone(),
                    task: ch.task.clone(),
                    harness: ch.harness.clone(),
                    worktree: t.worktree_path.clone()?.into(),
                    branch: t.branch.clone(),
                    base_ref: f.base.clone().or_else(|| t.base_ref.clone()),
                    state,
                })
            })
            .collect()
    })
}

async fn compare(server: &Arc<Server>, p: &Value) -> R {
    let f = find(server, req(p, "family")?)?;
    let ins = inputs(server, &f);
    let stored = f.checks.clone();
    let (reports, ranked) = tokio::task::spawn_blocking(move || {
        let mut reports: Vec<fam::ChildReport> =
            ins.iter().map(|i| fam::collect_report(i, None)).collect();
        for r in &mut reports {
            fam::attach_check(r, stored.get(&r.handle));
        }
        fam::mark_shared(&mut reports);
        let ranked = fam::rank(&reports);
        (reports, ranked)
    })
    .await
    .map_err(internal)?;
    let text = fam::render_compare(&f.id, &reports, &ranked);
    let mut out = json!({"family": f.id, "state": f.state.as_str(), "picked": f.picked, "reports": reports, "ranking": ranked, "text": text});
    if let Some(pair) = p.get("pair").and_then(Value::as_array)
        && pair.len() == 2
    {
        let branch = |h: &str| {
            reports
                .iter()
                .find(|r| r.handle == h)
                .and_then(|r| r.branch.clone())
                .ok_or_else(|| not_found("child", h))
        };
        let (a, bb) = (
            branch(pair[0].as_str().unwrap_or(""))?,
            branch(pair[1].as_str().unwrap_or(""))?,
        );
        let repo = f.repo.clone();
        let d = tokio::task::spawn_blocking(move || fam::pairwise(Path::new(&repo), &a, &bb))
            .await
            .map_err(internal)?
            .map_err(from_orch)?;
        out["pair"] = json!({"a": pair[0], "b": pair[1], "summary": d});
    }
    Ok(out)
}

/// Run the family's check in each child (or one) and store the outcome with the revision.
async fn run_checks(
    server: &Arc<Server>,
    f: &mut Family,
    only: Option<&str>,
    timeout: std::time::Duration,
) -> Result<Vec<Value>, vk_proto::rpc::RpcError> {
    let Some(cmd) = f.check_command.clone() else {
        return Err(err(
            ErrorKind::Conflict,
            "no check command: pass `check_command` to task.best_of_n, set orchestrate.best_of_n.check_command, or trust the repo's `[check]` in .vibeke/task.toml",
        ));
    };
    let ins = inputs(server, f);
    let mut out = vec![];
    for i in ins
        .into_iter()
        .filter(|i| only.is_none_or(|o| o == i.handle || o == i.task))
    {
        let (wt, c2) = (i.worktree.clone(), cmd.clone());
        let (outcome, summary) = tokio::task::spawn_blocking(move || {
            let o = fam::run_check(&wt, &c2, timeout);
            let s = fam::diff_summary(&wt, None).ok();
            (o, s)
        })
        .await
        .map_err(internal)?;
        let stored = fam::StoredCheck {
            outcome: outcome.clone(),
            head: summary.as_ref().and_then(|s| s.head.clone()),
            dirty: summary.as_ref().is_some_and(|s| s.dirty),
            at_ms: vk_store::now_ms(),
        };
        f.checks.insert(i.handle.clone(), stored);
        orch::emit(
            server,
            "family.checked",
            json!({"family": f.id, "child": i.handle}),
            json!({"ok": outcome.ok, "exit_code": outcome.exit_code, "timed_out": outcome.timed_out, "duration_ms": outcome.duration_ms}),
        );
        out.push(json!({"child": i.handle, "outcome": outcome}));
    }
    Ok(out)
}

async fn family_check(server: &Arc<Server>, p: &Value) -> R {
    let c = orch::cfg(server);
    require(c.best_of_n.enabled, "best-of-N", "best_of_n")?;
    let mut f = find(server, req(p, "family")?)?;
    let res = run_checks(server, &mut f, s(p, "child"), c.best_of_n.check_timeout()).await?;
    save(server, &f, vec![])?;
    Ok(json!({"family": f.id, "results": res}))
}

async fn pick(server: &Arc<Server>, p: &Value) -> R {
    let c = orch::cfg(server);
    require(c.best_of_n.enabled, "best-of-N", "best_of_n")?;
    let mut f = find(server, req(p, "family")?)?;
    let child = req(p, "child")?.to_string();
    f.pick(&child).map_err(from_orch)?;
    let picked = f.picked.clone().unwrap_or_default();
    let mut merged = Value::Null;
    if b(p, "merge").unwrap_or(false) {
        require(c.merge.enabled, "merge orchestration", "merge")?;
        let mut params = json!({"task": f.child(&picked).map(|c| c.task.clone()), "run": true});
        if let Some(t) = s(p, "target") {
            params["target"] = json!(t);
        }
        merged = call(server, "merge.queue.add", params).await?;
    }
    let mut discarded = vec![];
    if b(p, "discard").unwrap_or(false) {
        let losers: Vec<(String, String)> = f
            .losers()
            .iter()
            .map(|c| (c.handle.clone(), c.task.clone()))
            .collect();
        for (h, t) in losers {
            let r = call(
                server,
                "task.finish",
                json!({"task": t, "remove_worktree": true, "archive": true, "force": b(p, "force").unwrap_or(false)}),
            )
            .await;
            match r {
                Ok(_) => {
                    f.mark_discarded(&h);
                    discarded.push(json!({"child": h, "ok": true}));
                }
                Err(e) => discarded.push(json!({"child": h, "ok": false, "error": e.message})),
            }
        }
    }
    save(
        server,
        &f,
        vec![(
            "family.picked",
            json!({"family": f.id}),
            json!({"picked": picked, "discarded": discarded.iter().filter(|d| d["ok"] == true).count()}),
        )],
    )?;
    Ok(json!({"family": f, "picked": picked, "merge": merged, "discarded": discarded}))
}

/// Background: run the check in a child that went idle at a revision it has no fresh check for.
pub async fn tick(server: &Arc<Server>, c: &OrchestrateConfig) {
    if !c.best_of_n.check {
        return;
    }
    for mut f in families(server)
        .into_iter()
        .filter(|f| f.state == FamilyState::Running && f.check_command.is_some())
    {
        let ready: Vec<String> = server.with_core(|core| {
            f.children
                .iter()
                .filter(|ch| !ch.discarded)
                .filter(|ch| {
                    let runs: Vec<_> = core
                        .model
                        .runs
                        .iter()
                        .filter(|r| {
                            r.task.as_deref() == Some(ch.task.as_str()) && r.ended_at_ms.is_none()
                        })
                        .collect();
                    !runs.is_empty()
                        && runs.iter().all(|r| {
                            r.execution.value == vk_proto::model::Execution::Idle
                                && r.turns_completed > 0
                        })
                })
                .map(|ch| ch.handle.clone())
                .collect()
        });
        for h in ready {
            let ins = inputs(server, &f);
            let Some(i) = ins.into_iter().find(|i| i.handle == h) else {
                continue;
            };
            let wt = i.worktree.clone();
            let head = tokio::task::spawn_blocking(move || fam::diff_summary(&wt, None).ok())
                .await
                .ok()
                .flatten();
            let fresh = f
                .checks
                .get(&h)
                .zip(head.as_ref())
                .is_some_and(|(s, d)| s.head == d.head && s.dirty == d.dirty);
            if fresh {
                continue;
            }
            if run_checks(server, &mut f, Some(&h), c.best_of_n.check_timeout())
                .await
                .is_ok()
            {
                let _ = save(server, &f, vec![]);
            }
            // One check per pass keeps the load flat.
            return;
        }
    }
}
