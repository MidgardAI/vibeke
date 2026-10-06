//! Quota and cost scheduling (12): `quota.*`.
//!
//! Observations come from what the adapters already extract (`AgentRun.rate_limit`: Codex
//! windows, Claude `StopFailure`/`quota_auto_resume_*`, pi/omp/OpenCode retry messages). Near a
//! limit, low-priority tasks are parked (`task.park`; a run outside any task is interrupted) and
//! resumed after the window resets. The decisions are `vk_orchestrate::quota::tick`; this
//! module observes, applies them through the normal API, and records them.

use crate::Server;
use crate::api::{Ctx, R, b, internal, invalid, req, s};
use crate::core::Tx;
use crate::orch::{self, call, kv_get, kv_put};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use vk_orchestrate::OrchestrateConfig;
use vk_orchestrate::quota::{self, AccountUse, Action, RunView, State};
use vk_proto::model::Execution;

const SCOPE: &str = "orch.quota";

/// One observation per account: the newest rate-limit report among live runs on it.
pub fn observe(server: &Server, c: &OrchestrateConfig) -> Vec<AccountUse> {
    let mut best: BTreeMap<String, AccountUse> = BTreeMap::new();
    server.with_core(|core| {
        for r in core.model.runs.iter().filter(|r| r.ended_at_ms.is_none()) {
            let Some(rl) = &r.rate_limit else { continue };
            let account = c.quota.account_of(&r.harness);
            let a = AccountUse {
                account: account.clone(),
                harness: r.harness.clone(),
                scope: rl.scope.clone(),
                used_fraction: rl.used_percent.map(|p| f64::from(p) / 100.0),
                limited: rl.limited,
                resets_at_ms: rl.resets_at_ms,
                observed_at_ms: rl.observed_at_ms,
            };
            match best.get(&account) {
                Some(old) if old.observed_at_ms >= a.observed_at_ms => {}
                _ => {
                    best.insert(account, a);
                }
            }
        }
    });
    best.into_values().collect()
}

fn run_views(server: &Server) -> Vec<RunView> {
    server.with_core(|core| {
        core.model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none())
            .map(|r| {
                let task = r.task.as_deref().and_then(|t| core.task(t));
                RunView {
                    run: r.id.clone(),
                    task: task.map(|t| t.id.clone()),
                    handle: task
                        .map(|t| t.handle.clone())
                        .unwrap_or_else(|| r.handle.clone()),
                    harness: r.harness.clone(),
                    priority: task.and_then(|t| t.priority).unwrap_or(0),
                    working: r.execution.value == Execution::Working,
                }
            })
            .collect()
    })
}

fn load_state(server: &Server) -> State {
    kv_get(server, SCOPE, "state")
}

fn save_state(
    server: &Server,
    st: &State,
    events: Vec<(&str, Value, Value)>,
) -> Result<(), vk_proto::rpc::RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    kv_put(&mut tx, SCOPE, "state", st);
    for (k, s, d) in events {
        tx.event(k, s, d);
    }
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

pub async fn api(server: &Arc<Server>, _ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "quota.status" => Ok(status(server)),
        "quota.tick" => tick_api(server, p).await,
        "quota.route" => Ok(route(server, p)),
        "quota.resume" => resume(server, p).await,
        _ => return None,
    })
}

fn status(server: &Server) -> Value {
    let c = orch::cfg(server);
    let st = load_state(server);
    json!({
        "enabled": c.quota.enabled,
        "accounts": observe(server, &c),
        "paused": st.paused.values().collect::<Vec<_>>(),
        "config": {"pause_at": c.quota.pause_at, "resume_below": c.quota.resume_below, "protect_priority": c.quota.protect_priority, "max_wait_ms": c.quota.max_wait().as_millis() as u64, "interval_ms": c.quota.interval().as_millis() as u64},
    })
}

fn route(server: &Server, p: &Value) -> Value {
    let c = orch::cfg(server);
    let cand: Vec<String> = match p.get("harnesses").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        None => [
            "claude", "codex", "pi", "omp", "opencode", "gemini", "hermes",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    };
    let ranked = quota::rank_for_routing(vk_store::now_ms(), &cand, &observe(server, &c), &c.quota);
    json!({"ranking": ranked.iter().map(|(h, r)| json!({"harness": h, "headroom": r, "account": c.quota.account_of(h)})).collect::<Vec<_>>()})
}

async fn tick_api(server: &Arc<Server>, p: &Value) -> R {
    let c = orch::cfg(server);
    orch::require(c.quota.enabled, "quota scheduling", "quota")?;
    let dry = b(p, "dry_run").unwrap_or(false);
    let actions = tick_apply(server, &c, dry).await;
    Ok(json!({"dry_run": dry, "actions": actions}))
}

/// One scheduling pass; returns the actions decided (applied unless `dry`).
pub async fn tick_apply(server: &Arc<Server>, c: &OrchestrateConfig, dry: bool) -> Vec<Action> {
    let now = vk_store::now_ms();
    let st = load_state(server);
    let actions = quota::tick(now, &observe(server, c), &run_views(server), &st, &c.quota);
    if dry {
        return actions;
    }
    for a in &actions {
        let _ = apply_one(server, a, now).await;
    }
    actions
}

async fn apply_one(
    server: &Arc<Server>,
    a: &Action,
    now: i64,
) -> Result<(), vk_proto::rpc::RpcError> {
    match a {
        Action::Pause {
            task,
            runs,
            handle,
            account,
            reason,
            resumes_at_ms,
            key,
            ..
        } => {
            match task {
                Some(t) => {
                    call(server, "task.park", json!({"task": t})).await?;
                }
                None => {
                    for r in runs {
                        let _ = call(server, "agent.interrupt", json!({"target": r})).await;
                    }
                }
            }
            let mut st = load_state(server);
            quota::apply(&mut st, a, now);
            save_state(
                server,
                &st,
                vec![(
                    "quota.paused",
                    json!({"key": key}),
                    json!({"handle": handle, "account": account, "reason": reason, "resumes_at_ms": resumes_at_ms, "task": task}),
                )],
            )
        }
        Action::Resume {
            key,
            task,
            handle,
            reason,
        } => {
            let st0 = load_state(server);
            match task {
                Some(t) => {
                    call(server, "task.resume", json!({"task": t})).await?;
                }
                None => {
                    if let Some(p) = st0.paused.get(key) {
                        for r in &p.runs {
                            let _ = call(server, "agent.prompt", json!({"target": r, "text": "The usage limit has reset. Continue where you left off."})).await;
                        }
                    }
                }
            }
            let mut st = st0;
            quota::apply(&mut st, a, now);
            save_state(
                server,
                &st,
                vec![(
                    "quota.resumed",
                    json!({"key": key}),
                    json!({"handle": handle, "reason": reason, "task": task}),
                )],
            )
        }
        Action::Forget { .. } => {
            let mut st = load_state(server);
            quota::apply(&mut st, a, now);
            save_state(server, &st, vec![])
        }
    }
}

async fn resume(server: &Arc<Server>, p: &Value) -> R {
    let key = req(p, "key")
        .or_else(|_| {
            s(p, "task").ok_or_else(|| invalid("missing param `key` (a task id or run id)"))
        })?
        .to_string();
    let st = load_state(server);
    let Some(e) = st.paused.get(&key).cloned() else {
        return Err(crate::api::not_found("paused entry", &key));
    };
    let a = Action::Resume {
        key: e.key.clone(),
        task: e.task.clone(),
        handle: e.handle.clone(),
        reason: "resumed by hand".into(),
    };
    apply_one(server, &a, vk_store::now_ms()).await?;
    Ok(json!({"resumed": e.key, "handle": e.handle}))
}
