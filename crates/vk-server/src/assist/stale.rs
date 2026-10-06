//! Staleness and live revalidation (14 §7.2).
//!
//! A generated result is an interpretation of state at one moment. When a request is created
//! the coordinator records a **fingerprint** of every live object the result is about (the
//! workspace's open interactions, live runs and tasks for a briefing; one interaction for a
//! decision card; a run; a task). Reading the request recomputes them: a difference marks the
//! result **stale** and names which objects changed, and a vanished object says so. The
//! result is never rewritten and never acted on from cache: `live_targets` reports each
//! cited target's *current* state, which is what a client must show before offering an action
//! (answering an interaction still goes through `interaction.answer` and its own checks).

use super::*;
use std::collections::BTreeMap;

fn digest(parts: &[String]) -> String {
    blake3::hash(parts.join("\u{1}").as_bytes()).to_hex()[..16].to_string()
}

/// The current fingerprint of one live object (`None` when it no longer exists).
pub(super) fn fingerprint(server: &Server, kind: &str, id: &str) -> Option<String> {
    server.with_core(|c| match kind {
        "interaction" => c
            .model
            .interactions
            .iter()
            .find(|i| i.id == id)
            .map(|i| format!("{:?}/{}/{:?}", i.status, i.decision_rev, i.delivery)),
        "run" => c.run(id).map(|r| {
            format!(
                "{:?}/{}/{}/{}",
                r.execution.value,
                r.turns_completed,
                r.last_tool.as_deref().unwrap_or(""),
                r.ended_at_ms.is_some()
            )
        }),
        "task" => c.model.tasks.iter().find(|t| t.id == id).map(|t| {
            format!(
                "{}/{}/{:?}/{:?}",
                t.status, t.rev, t.review_label, t.intent_revision
            )
        }),
        "workspace" => {
            c.ws(id)?;
            let panes: Vec<&str> = c
                .model
                .panes
                .iter()
                .filter(|p| p.workspace == id)
                .map(|p| p.id.as_str())
                .collect();
            let mut parts: Vec<String> = vec![];
            for i in
                c.model.interactions.iter().filter(|i| {
                    panes.contains(&i.pane.as_str()) && i.status == InteractionStatus::Open
                })
            {
                parts.push(format!("i:{}:{}", i.id, i.decision_rev));
            }
            for r in c
                .model
                .runs
                .iter()
                .filter(|r| panes.contains(&r.pane.as_str()) && r.ended_at_ms.is_none())
            {
                parts.push(format!("r:{}:{:?}", r.id, r.execution.value));
            }
            for t in c
                .model
                .tasks
                .iter()
                .filter(|t| t.workspace.as_deref() == Some(id))
            {
                parts.push(format!("t:{}:{}:{:?}", t.id, t.status, t.review_label));
            }
            parts.sort();
            Some(digest(&parts))
        }
        _ => None,
    })
}

/// The fingerprints to record for a request with this scope descriptor.
pub(super) fn capture(server: &Server, op: Operation, scope: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut add = |kind: &str, id: Option<&str>| {
        if let Some(id) = id
            && let Some(f) = fingerprint(server, kind, id)
        {
            out.insert(format!("{kind}:{id}"), f);
        }
    };
    add("interaction", scope["interaction"].as_str());
    add("run", scope["run"].as_str());
    add("task", scope["task"].as_str());
    if matches!(
        op,
        Operation::Briefing | Operation::BackgroundSummary | Operation::Navigate
    ) {
        add("workspace", scope["workspace"].as_str());
    }
    out
}

/// Add `stale`, `stale_targets` and `live_targets` to a request view.
pub(super) fn decorate(server: &Server, r: &AssistRequest, v: &mut Value) {
    if r.state != ReqState::Done {
        return;
    }
    let mut changed: Vec<String> = vec![];
    for (key, then) in &r.live {
        let Some((kind, id)) = key.split_once(':') else {
            continue;
        };
        match fingerprint(server, kind, id) {
            Some(now) if now == *then => {}
            Some(_) => changed.push(format!("{kind} {id} changed since this was generated")),
            None => changed.push(format!("{kind} {id} no longer exists")),
        }
    }
    if let Some(o) = v.as_object_mut() {
        o.insert("stale".into(), json!(!changed.is_empty()));
        o.insert("stale_targets".into(), json!(changed));
        if !changed.is_empty() {
            o.insert(
                "refresh_hint".into(),
                json!("generate a new request; this result describes earlier state"),
            );
        }
        o.insert("live_targets".into(), json!(live_targets(server, r)));
    }
}

/// The cited targets of a finished request with their **current** state, read now. A client
/// shows this before offering any action; cached statuses are never authoritative.
pub(super) fn live_targets(server: &Server, r: &AssistRequest) -> Vec<Value> {
    let Some(out) = &r.output else {
        return vec![];
    };
    let mut ids: Vec<String> = vec![];
    let mut push = |s: &str| {
        if !ids.iter().any(|x| x == s) {
            ids.push(s.to_string());
        }
    };
    for it in out["items"].as_array().into_iter().flatten() {
        for t in it["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            push(t);
        }
    }
    for m in out["matches"].as_array().into_iter().flatten() {
        if let Some(t) = m["target"].as_str() {
            push(t);
        }
    }
    if let Some(i) = out["interaction"].as_str() {
        push(i);
    }
    ids.truncate(60);
    server.with_core(|c| {
        ids.iter()
            .map(|id| {
                if let Some(i) = c.model.interactions.iter().find(|i| i.id == *id) {
                    json!({"id": id, "kind": "interaction", "exists": true, "status": format!("{:?}", i.status).to_lowercase(), "answerable": i.answerable && i.status == InteractionStatus::Open, "delivery": format!("{:?}", i.delivery).to_lowercase()})
                } else if let Some(run) = c.run(id) {
                    json!({"id": id, "kind": "run", "exists": true, "status": format!("{:?}", run.execution.value).to_lowercase(), "ended": run.ended_at_ms.is_some()})
                } else if let Some(t) = c.model.tasks.iter().find(|t| t.id == *id) {
                    json!({"id": id, "kind": "task", "exists": true, "status": t.status})
                } else if let Some(p) = c.pane(id) {
                    json!({"id": id, "kind": "pane", "exists": true, "status": if p.exited { "exited" } else { "live" }})
                } else if c.ws(id).is_some() {
                    json!({"id": id, "kind": "workspace", "exists": true})
                } else {
                    json!({"id": id, "exists": false})
                }
            })
            .collect()
    })
}
