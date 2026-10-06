//! Goal planner (12 "Goal -> plan -> tasks"): `goal.*`.
//!
//! A goal is planned (heuristic planner, a planning agent, or an externally submitted plan),
//! a person approves the plan, and only then are steps started as tasks, routed to a harness by
//! fit, cost and quota. Editing a plan clears its approval. Step completion is observed from
//! the step's task (finished/archived, or its branch merged through the merge queue) or marked
//! with `goal.step_done`. All the rules are `vk_orchestrate::plan`.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s};
use crate::core::Tx;
use crate::orch::{self, call, from_orch, load_all, load_one, next_counter, put, require};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use vk_orchestrate::OrchestrateConfig;
use vk_orchestrate::plan::{self, Goal, GoalInput, GoalState, Planner, Step, StepStatus};
use vk_orchestrate::quota::AccountUse;
use vk_proto::rpc::ErrorKind;

const KIND: &str = "orch_goal";

pub fn goals(server: &Server) -> Vec<Goal> {
    load_all(server, KIND)
}

fn find(server: &Server, id: &str) -> Result<Goal, vk_proto::rpc::RpcError> {
    load_one(server, KIND, id).ok_or_else(|| not_found("goal", id))
}

fn save(server: &Server, g: &Goal, events: Vec<(&str, Value, Value)>) -> Result<(), vk_proto::rpc::RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    if g.state.closed() {
        tx.m.close(KIND, &g.id, Some(&g.handle), g);
    } else {
        put(&mut tx, KIND, &g.id, Some(&g.handle), g);
    }
    for (k, s, d) in events {
        tx.event(k, s, d);
    }
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

fn gate(server: &Server) -> Result<OrchestrateConfig, vk_proto::rpc::RpcError> {
    let c = orch::cfg(server);
    require(c.planner.enabled, "the goal planner", "planner")?;
    Ok(c)
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "goal.create" => create(server, ctx, p).await,
        "goal.list" => list(server),
        "goal.get" => get(server, p),
        "goal.plan" => plan_goal(server, p).await,
        "goal.plan_submit" => plan_submit(server, p),
        "goal.approve" => approve(server, p).await,
        "goal.start" => start(server, p).await,
        "goal.step_done" => step_done(server, p).await,
        "goal.cancel" => cancel(server, p).await,
        "goal.briefing" => briefing(server, p),
        _ => return None,
    })
}

fn view(g: &Goal) -> Value {
    let (done, total) = g.progress();
    json!({"goal": g, "progress": {"done": done, "total": total}})
}

fn list(server: &Server) -> R {
    gate(server)?;
    let mut all = goals(server);
    all.extend(server.with_core(|c| c.store.load_closed::<Goal>(KIND, 50).unwrap_or_default()));
    let mut v: Vec<Value> = all.iter().map(view).collect();
    v.reverse();
    Ok(json!({"goals": v}))
}

fn get(server: &Server, p: &Value) -> R {
    gate(server)?;
    Ok(view(&find(server, req(p, "goal")?)?))
}

fn harness_ids() -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path).chain(["/usr/local/bin".into(), "/opt/homebrew/bin".into()]).collect();
    let found: Vec<String> = ["claude", "codex", "pi", "omp", "opencode", "gemini", "hermes"]
        .iter()
        .filter(|h| dirs.iter().any(|d| d.join(h).is_file()))
        .map(|h| h.to_string())
        .collect();
    if found.is_empty() { vec!["claude".into()] } else { found }
}

fn input(g: &Goal, c: &OrchestrateConfig) -> GoalInput {
    GoalInput {
        title: g.title.clone(),
        text: g.text.clone(),
        repo: g.repo.clone(),
        harnesses: harness_ids(),
        max_steps: c.planner.max_steps,
    }
}

async fn create(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let c = gate(server)?;
    let title = req(p, "title")?.trim().to_string();
    if title.is_empty() {
        return Err(invalid("`title` is empty"));
    }
    let text = match s(p, "text_file") {
        Some(f) => {
            let md = std::fs::metadata(f).map_err(|e| invalid(format!("text_file {f}: {e}")))?;
            if md.len() > 1 << 20 {
                return Err(invalid("text_file is larger than 1 MiB"));
            }
            std::fs::read_to_string(f).map_err(|e| invalid(format!("text_file {f}: {e}")))?
        }
        None => s(p, "text").unwrap_or("").to_string(),
    };
    let repo = orch::repo_param(server, ctx, p);
    let root = vk_tasks::repo_root(std::path::Path::new(&repo))
        .map(|i| i.root.to_string_lossy().into_owned())
        .ok_or_else(|| invalid(format!("{repo} is not inside a repository")))?;
    let goal = {
        let mut core = server.core.lock().unwrap();
        let mut tx = Tx::new();
        let n = next_counter(&mut core, &mut tx, "goal");
        let id = format!("goal-{}", &crate::core::ulid().to_lowercase()[16..]);
        let g = Goal::new(&id, &format!("G{n}"), &title, &text, &root, s(p, "base"));
        put(&mut tx, KIND, &g.id, Some(&g.handle), &g);
        tx.event("goal.created", json!({"goal": g.id}), json!({"handle": g.handle, "title": g.title}));
        server.commit(&mut core, tx).map_err(internal)?;
        g
    };
    if b(p, "plan").unwrap_or(true) {
        // A failed automatic plan leaves a draft the person can plan again or submit to.
        if let Ok(v) = plan_goal(server, &json!({"goal": goal.id})).await {
            return Ok(v);
        }
    }
    let _ = c;
    Ok(view(&goal))
}

async fn plan_goal(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let mut g = find(server, req(p, "goal")?)?;
    let backend = s(p, "backend").unwrap_or(&c.planner.backend).to_string();
    match backend.as_str() {
        "heuristic" => {
            let pl = plan::HeuristicPlanner.plan(&input(&g, &c)).map_err(from_orch)?;
            set_plan(server, &mut g, pl, &c)?;
            Ok(view(&g))
        }
        "agent" | "external" => {
            let plan_file = crate::paths::state_root().join("goals").join(&g.id).join("plan.json");
            let submit = format!("vibeke goal plan-submit {} --file {}", g.handle, plan_file.display());
            let prompt = plan::planner_prompt(&input(&g, &c), &submit);
            if let Some(d) = plan_file.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let mut out = json!({"goal": g, "backend": backend, "prompt": prompt, "submit_command": submit, "plan_file": plan_file});
            if backend == "agent" {
                // Live harness run: needs a logged-in harness and is gated by the user's config.
                match call(server, "agent.spawn", json!({"harness": c.planner.planner_harness, "cwd": g.repo, "prompt": prompt})).await {
                    Ok(r) => {
                        g.planning_run = r["run"]["id"].as_str().map(str::to_string);
                        out["goal"] = json!(g);
                        out["planning_run"] = json!(g.planning_run);
                        save(server, &g, vec![("goal.planning_started", json!({"goal": g.id}), json!({"harness": c.planner.planner_harness, "run": g.planning_run}))])?;
                    }
                    Err(e) => out["agent_error"] = json!(e.message),
                }
            }
            Ok(out)
        }
        o => Err(invalid(format!("planner backend `{o}` is not heuristic, agent or external"))),
    }
}

fn set_plan(server: &Server, g: &mut Goal, pl: plan::Plan, c: &OrchestrateConfig) -> Result<(), vk_proto::rpc::RpcError> {
    let steps = pl.steps.len();
    let by = pl.planner.clone();
    g.set_plan(pl, c.planner.max_steps).map_err(from_orch)?;
    save(server, g, vec![("goal.planned", json!({"goal": g.id}), json!({"steps": steps, "planner": by, "rev": g.plan_rev}))])
}

fn plan_submit(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let mut g = find(server, req(p, "goal")?)?;
    let text = match p.get("plan") {
        Some(Value::String(t)) => t.clone(),
        Some(v @ Value::Object(_)) => v.to_string(),
        _ => match s(p, "file") {
            Some(f) => std::fs::read_to_string(f).map_err(|e| invalid(format!("file {f}: {e}")))?,
            None => return Err(invalid("missing param `plan` (a plan object or JSON text) or `file`")),
        },
    };
    let pl = plan::parse_plan_json(&text, c.planner.max_steps, s(p, "planner").unwrap_or("external")).map_err(from_orch)?;
    set_plan(server, &mut g, pl, &c)?;
    Ok(view(&g))
}

async fn approve(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let mut g = find(server, req(p, "goal")?)?;
    let by = s(p, "by").unwrap_or("user").to_string();
    g.approve(&by, c.planner.max_steps).map_err(from_orch)?;
    save(server, &g, vec![("goal.approved", json!({"goal": g.id}), json!({"by": by, "rev": g.plan_rev}))])?;
    if b(p, "start").unwrap_or(true) {
        fan_out(server, &g.id, &c).await?;
        g = find(server, &g.id)?;
    }
    Ok(view(&g))
}

async fn start(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let g = find(server, req(p, "goal")?)?;
    if !g.may_start(c.planner.approval_required) {
        return Err(err(ErrorKind::Conflict, "the plan is not approved (run `goal approve`)").details(json!({"reason": "not_approved"})));
    }
    let started = fan_out(server, &g.id, &c).await?;
    let mut v = view(&find(server, &g.id)?);
    v["started"] = json!(started);
    Ok(v)
}

fn quota_accounts(server: &Server, c: &OrchestrateConfig) -> Vec<AccountUse> {
    crate::orch_quota::observe(server, c)
}

/// Start every ready step of the goal; returns the step ids started.
async fn fan_out(server: &Arc<Server>, id: &str, c: &OrchestrateConfig) -> Result<Vec<String>, vk_proto::rpc::RpcError> {
    let mut started = vec![];
    loop {
        let mut g = find(server, id)?;
        if !g.may_start(c.planner.approval_required) || g.state.closed() {
            break;
        }
        let ready: Vec<Step> = g.ready_steps().into_iter().cloned().collect();
        let Some(step) = ready.into_iter().next() else { break };
        let profiles: Vec<plan::HarnessProfile> = harness_ids().iter().map(|h| plan::default_profile(h)).collect();
        let accounts: Vec<plan::AccountState> = quota_accounts(server, c)
            .into_iter()
            .map(|a| plan::AccountState { harness: a.harness, used_fraction: a.used_fraction, limited: a.limited, resets_at_ms: a.resets_at_ms })
            .collect();
        let mut running: BTreeMap<String, u32> = BTreeMap::new();
        server.with_core(|core| {
            for r in core.model.runs.iter().filter(|r| r.ended_at_ms.is_none()) {
                *running.entry(r.harness.clone()).or_default() += 1;
            }
        });
        let route = match plan::route(&step, &profiles, &accounts, &running, vk_store::now_ms()) {
            Ok(r) => r,
            Err(e) => {
                // Try again on a later pass (limits reset, runs finish).
                orch::emit(server, "goal.step_waiting", json!({"goal": g.id, "step": step.id}), json!({"reason": e.to_string()}));
                break;
            }
        };
        let slug = vk_tasks::slugify(&format!("{} {}", g.handle, step.title), vk_tasks::DEFAULT_SLUG_MAX);
        let mut params = json!({
            "title": format!("{}: {}", g.title, step.title),
            "repo": g.repo,
            "slug": slug,
            "agents": [{"harness": route.harness, "prompt": format!("{}\n\n(Goal {}, step {}.)", step.prompt, g.handle, step.id)}],
        });
        if let Some(base) = &g.base {
            params["base"] = json!(base);
        }
        let created = call(server, "task.create", params).await?;
        let task_id = created["task"]["id"].as_str().unwrap_or("").to_string();
        if c.merge.enabled {
            for path in &step.paths {
                let _ = call(server, "task.claim", json!({"task": task_id, "glob": path, "note": format!("goal {} step {}", g.handle, step.id)})).await;
            }
        }
        g.step_started(&step.id, &task_id, &route.harness).map_err(from_orch)?;
        save(
            server,
            &g,
            vec![("goal.step_started", json!({"goal": g.id, "step": step.id}), json!({"task": task_id, "harness": route.harness, "reasons": route.reasons}))],
        )?;
        started.push(step.id.clone());
    }
    Ok(started)
}

async fn step_done(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let mut g = find(server, req(p, "goal")?)?;
    let step = req(p, "step")?.to_string();
    let ok = b(p, "ok").unwrap_or(true);
    finish_step(server, &mut g, &step, ok, s(p, "error"))?;
    if ok {
        fan_out(server, &g.id, &c).await?;
    }
    Ok(view(&find(server, &g.id)?))
}

fn finish_step(server: &Server, g: &mut Goal, step: &str, ok: bool, error: Option<&str>) -> Result<(), vk_proto::rpc::RpcError> {
    g.step_finished(step, ok, error).map_err(from_orch)?;
    let mut ev = vec![("goal.step_finished", json!({"goal": g.id, "step": step}), json!({"status": if ok { "done" } else { "failed" }, "error": error}))];
    if g.state.closed() {
        ev.push(("goal.finished", json!({"goal": g.id}), json!({"state": g.state.as_str()})));
    }
    save(server, g, ev)
}

async fn cancel(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let mut g = find(server, req(p, "goal")?)?;
    let running: Vec<String> = g.runs.values().filter(|r| r.status == StepStatus::Running).filter_map(|r| r.task.clone()).collect();
    g.cancel().map_err(from_orch)?;
    save(server, &g, vec![("goal.cancelled", json!({"goal": g.id}), json!({"stop_tasks": b(p, "stop_tasks").unwrap_or(false)}))])?;
    let mut parked = vec![];
    if b(p, "stop_tasks").unwrap_or(false) {
        for t in running {
            if call(server, "task.park", json!({"task": t})).await.is_ok() {
                parked.push(t);
            }
        }
    }
    let mut v = view(&g);
    v["parked"] = json!(parked);
    Ok(v)
}

fn since_param(p: &Value, default: std::time::Duration, now: i64) -> i64 {
    match p.get("since") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(t)) => vk_orchestrate::parse_duration(t).map(|d| now - d.as_millis() as i64).unwrap_or(now - default.as_millis() as i64),
        _ => now - default.as_millis() as i64,
    }
}

fn briefing(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let now = vk_store::now_ms();
    let since = since_param(p, c.planner.briefing_window(), now);
    let until = p.get("until").and_then(Value::as_i64).unwrap_or(now);
    let (events, names) = server.with_core(|core| {
        let last = core.store.last_seq().unwrap_or(0);
        let evs = core.store.events_after((last - 5000).max(0), 5000, &[]).unwrap_or_default();
        let names: BTreeMap<String, String> = core.model.tasks.iter().map(|t| (t.id.clone(), t.handle.clone())).collect();
        (evs, names)
    });
    let evs: Vec<plan::BriefEvent> = events.into_iter().map(|e| plan::BriefEvent { ts_ms: e.ts, kind: e.kind, subject: e.subject, data: e.data }).collect();
    let br = plan::briefing(&evs, since, until, &names);
    Ok(json!({"briefing": br}))
}

/// Background: observe step tasks and start newly ready steps.
pub async fn tick(server: &Arc<Server>, c: &OrchestrateConfig) {
    for mut g in goals(server).into_iter().filter(|g| matches!(g.state, GoalState::Running)) {
        let running: Vec<(String, String)> = g.runs.iter().filter(|(_, r)| r.status == StepStatus::Running).filter_map(|(k, r)| r.task.clone().map(|t| (k.clone(), t))).collect();
        let merged = merged_tasks(server);
        let mut changed = false;
        for (step, task) in running {
            let st = server.with_core(|core| core.task(&task).map(|t| t.status.clone()).or_else(|| core.store.find::<vk_proto::model::Task>("task", &task).ok().flatten().map(|t| t.status)));
            let done = merged.contains(&task) || matches!(st.as_deref(), Some("finished" | "archived"));
            let gone = st.is_none() || st.as_deref() == Some("forgotten");
            if (done || gone) && finish_step(server, &mut g, &step, done, gone.then_some("the task disappeared")).is_ok() {
                changed = true;
            }
        }
        if changed {
            let _ = fan_out(server, &g.id, c).await;
        } else {
            // Steps may be waiting on a limit that reset.
            let _ = fan_out(server, &g.id, c).await;
        }
    }
}

fn merged_tasks(server: &Server) -> Vec<String> {
    let q: vk_orchestrate::merge::MergeQueue = load_one(server, "orch_queue", "main").unwrap_or_default();
    q.entries
        .iter()
        .filter(|e| e.state == vk_orchestrate::merge::EntryState::Merged)
        .map(|e| e.task.clone())
        .collect()
}
