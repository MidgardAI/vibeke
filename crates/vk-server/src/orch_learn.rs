//! Learned policy: `policy.learned.list|accept|dismiss` (04 §7.7, 12).
//!
//! The decision log is the interaction history itself: every approval a person answered. The
//! reduction to fingerprints and suggestions is `vk_orchestrate::learn`. Nothing is applied
//! automatically: `accept` goes through `policy.add` (audited like any rule) or appends to the
//! repository's `.vibeke/policy.toml`, which is reviewed like code and only honoured once the
//! repository is trusted.

use crate::Server;
use crate::api::{Ctx, R, internal, invalid, not_found, req, s};
use crate::core::Tx;
use crate::orch::{self, call, kv_get, kv_put, require};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use vk_orchestrate::learn::{self, DecisionRecord, Suggestion, Verdict};
use vk_proto::model::{Decision, Interaction, InteractionKind};

const SCOPE: &str = "orch.learned";

/// Answered approvals as decision records, newest history first (bounded).
pub fn decision_records(server: &Server) -> Vec<DecisionRecord> {
    let its: Vec<Interaction> = server.with_core(|c| {
        let mut v = c.model.interactions.clone();
        v.extend(
            c.store
                .load_closed::<Interaction>("interaction", 5000)
                .unwrap_or_default(),
        );
        v
    });
    let mut seen = HashSet::new();
    let mut out = vec![];
    for it in its {
        if it.kind != InteractionKind::Approval || !seen.insert(it.id.clone()) {
            continue;
        }
        let (Some(a), Some(ans)) = (&it.action, &it.answer) else {
            continue;
        };
        let verdict = match ans.decision {
            Some(Decision::Allow | Decision::AllowAlways) => Verdict::Allow,
            Some(Decision::Deny) => Verdict::Deny,
            None => continue,
        };
        let (harness, workspace) = server.with_core(|c| {
            let run = c.run(&it.run).cloned().or_else(|| {
                c.store
                    .find::<vk_proto::model::AgentRun>("run", &it.run)
                    .ok()
                    .flatten()
            });
            let ws = run
                .as_ref()
                .and_then(|r| r.task.as_deref())
                .and_then(|t| c.task(t).map(|t| t.repo_root.clone()))
                .or_else(|| run.as_ref().and_then(|r| r.cwd.clone()))
                .or_else(|| c.pane(&it.pane).and_then(|p| p.cwd.clone()))
                .unwrap_or_default();
            (run.map(|r| r.harness).unwrap_or_default(), ws)
        });
        if workspace.is_empty() {
            continue;
        }
        out.push(DecisionRecord {
            interaction: it.id.clone(),
            harness,
            tool: a.tool.clone(),
            command: a.command.clone(),
            paths: a.paths.clone(),
            workspace,
            verdict,
            by: it.answered_by.clone().unwrap_or_default(),
            risk: format!("{:?}", a.risk).to_lowercase(),
            at_ms: it.answered_at_ms.unwrap_or(it.opened_at_ms),
        });
    }
    out
}

fn dismissed(server: &Server) -> HashSet<String> {
    kv_get::<Vec<String>>(server, SCOPE, "dismissed")
        .into_iter()
        .collect()
}

/// Current suggestions: thresholds from config, minus dismissed and already-decided actions.
pub fn suggestions(server: &Server, records: &[DecisionRecord]) -> (Vec<Suggestion>, usize) {
    let c = orch::cfg(server).learned_policy;
    let since = vk_store::now_ms() - c.window().as_millis() as i64;
    let aggs = learn::aggregate(records, since);
    let n = aggs.len();
    let covered = |r: &DecisionRecord| {
        let action = crate::policy_api::Action {
            tool: r.tool.clone(),
            command: r.command.clone(),
            paths: r.paths.clone(),
            url: None,
        };
        let ws = Path::new(&r.workspace);
        crate::policy_api::evaluate(server, &action, Some(ws), Some(ws)).effect != "ask"
    };
    (learn::suggest(&aggs, &c, &dismissed(server), &covered), n)
}

pub async fn api(server: &Arc<Server>, _ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "policy.learned.list" => list(server, p),
        "policy.learned.accept" => accept(server, p).await,
        "policy.learned.dismiss" => dismiss(server, p),
        _ => return None,
    })
}

fn gate(server: &Server) -> Result<(), vk_proto::rpc::RpcError> {
    require(
        orch::cfg(server).learned_policy.enabled,
        "learned policy",
        "learned_policy",
    )
}

fn list(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let c = orch::cfg(server).learned_policy;
    let records = decision_records(server);
    let (mut sug, fingerprints) = suggestions(server, &records);
    if let Some(repo) = s(p, "repo") {
        let repo = repo.trim_end_matches('/');
        sug.retain(|x| x.workspace.trim_end_matches('/') == repo);
    }
    Ok(json!({
        "suggestions": sug,
        "stats": {
            "decisions": records.len(),
            "fingerprints": fingerprints,
            "min_approvals": c.min_approvals,
            "max_denials": c.max_denials,
            "window_ms": c.window().as_millis() as u64,
            "suggest_deny": c.suggest_deny,
            "dismissed": dismissed(server).len(),
        },
    }))
}

async fn accept(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let id = req(p, "id")?;
    let records = decision_records(server);
    let (sug, _) = suggestions(server, &records);
    let Some(s0) = sug.into_iter().find(|x| x.id == id) else {
        return Err(not_found("suggestion", id));
    };
    let target = s(p, "target").unwrap_or("user");
    let out = match target {
        "user" => {
            let r = call(server, "policy.add", json!({"rule": s0.rule})).await?;
            json!({"target": "user", "rule": r["rule"]})
        }
        "repo" => {
            let ws = Path::new(&s0.workspace);
            let dir = ws.join(".vibeke");
            let file = dir.join("policy.toml");
            let existing = std::fs::read_to_string(&file).unwrap_or_default();
            let changed = learn::append_repo_rule(&existing, &s0);
            if let Some(text) = &changed {
                std::fs::create_dir_all(&dir).map_err(internal)?;
                let tmp = dir.join("policy.toml.tmp");
                std::fs::write(&tmp, text).map_err(internal)?;
                std::fs::rename(&tmp, &file).map_err(internal)?;
            }
            json!({
                "target": "repo",
                "file": file,
                "changed": changed.is_some(),
                "note": "review and commit the file; a repository `allow` rule only applies after `vibeke policy trust --allow-policy-grants`",
            })
        }
        o => return Err(invalid(format!("target `{o}` is not user or repo"))),
    };
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    let mut acc: Vec<Value> = kv_get(server, SCOPE, "accepted");
    acc.push(json!({"id": s0.id, "target": target, "at_ms": vk_store::now_ms()}));
    kv_put(&mut tx, SCOPE, "accepted", &acc);
    tx.event(
        "policy.learned_accepted",
        json!({"suggestion": s0.id}),
        json!({"target": target, "effect": s0.effect, "pattern": s0.pattern, "workspace": s0.workspace, "approvals": s0.approvals}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    Ok(json!({"accepted": s0, "result": out}))
}

fn dismiss(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let id = req(p, "id")?.to_string();
    let mut d: Vec<String> = kv_get(server, SCOPE, "dismissed");
    if !d.contains(&id) {
        d.push(id.clone());
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    kv_put(&mut tx, SCOPE, "dismissed", &d);
    tx.event(
        "policy.learned_dismissed",
        json!({"suggestion": id}),
        json!({}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"dismissed": id, "total": d.len()}))
}
