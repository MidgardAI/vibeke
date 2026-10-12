//! Mention names (spec 19 §2): at most one per pane, unique in its workspace, frozen once given.
//! Naming a pane is the consent for it to receive messages from the other panes of its
//! workspace (§2.3): a person names any pane, an agent only its own.

use super::*;
use vk_proto::mention::MentionName;
use vk_proto::model::{Execution, InteractionStatus};

/// How `mention.list` and the roster describe a pane: `idle`, `busy`, `needs_input`, `exited`
/// (the pane's process ended) or `shell` (no agent runs in it).
pub(crate) fn pane_state(c: &Core, pane: &Pane) -> &'static str {
    if pane.exited {
        return "exited";
    }
    let Some(run) = c.run_for_pane(&pane.id) else {
        return "shell";
    };
    if c.model
        .interactions
        .iter()
        .any(|i| i.pane == pane.id && i.status == InteractionStatus::Open)
    {
        return "needs_input";
    }
    match run.execution.value {
        Execution::Idle => "idle",
        Execution::Exited => "exited",
        _ => "busy",
    }
}

/// The sidebar label (§2.2): the run's label for an agent pane, else the pane title.
pub(crate) fn label_of(c: &Core, pane: &Pane) -> String {
    match c.run_for_pane(&pane.id) {
        Some(r) => r.label().to_string(),
        None => pane.display_title().to_string(),
    }
}

/// A proposed name for a pane without one (§2.2).
pub(crate) fn suggest_for(c: &Core, pane: &Pane) -> String {
    let run = c.run_for_pane(&pane.id);
    let label = label_of(c, pane);
    let fallback = match run {
        Some(r) => r.harness.clone(),
        None => pane
            .cwd
            .as_deref()
            .and_then(|d| std::path::Path::new(d).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    vm::suggest(&label, &fallback, |n| {
        c.model
            .mentions
            .iter()
            .any(|m| m.workspace == pane.workspace && m.name == n && m.pane != pane.id)
    })
}

/// One row of `mention.list`: the name plus label, harness and state.
pub(crate) fn entry(c: &Core, m: &MentionName) -> Value {
    let pane = c.pane(&m.pane);
    let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
    v["pane_handle"] = json!(pane.map(|p| p.handle.clone()));
    v["label"] = json!(pane.map(|p| label_of(c, p)).unwrap_or_default());
    v["harness"] = json!(c.run_for_pane(&m.pane).map(|r| r.harness.clone()));
    v["state"] = json!(pane.map(|p| pane_state(c, p)).unwrap_or("exited"));
    v
}

/// The panes a name can go to: no browser panes or plugin surfaces (§5).
pub(crate) fn nameable(p: &Pane) -> bool {
    !p.is_browser() && p.plugin_surface().is_none()
}

/// The roster (§4.4): one line per named pane of the workspace, the caller's own marked
/// `(you)`. Never message bodies.
pub(crate) fn roster(c: &Core, workspace: &str, me: Option<&str>) -> Vec<String> {
    let mut v: Vec<&MentionName> = c
        .model
        .mentions
        .iter()
        .filter(|m| m.workspace == workspace)
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v.iter()
        .map(|m| {
            let state = match c.pane(&m.pane).map(|p| pane_state(c, p)) {
                Some("needs_input") => "needs input",
                Some("shell") => "no agent",
                Some(s) => s,
                None => "exited",
            };
            let kind = match m.kind {
                MentionKind::Agent => "agent",
                MentionKind::Terminal => "terminal",
            };
            let harness = c
                .run_for_pane(&m.pane)
                .map(|r| r.harness.clone())
                .unwrap_or_else(|| "-".into());
            let you = if me == Some(m.pane.as_str()) {
                " (you)"
            } else {
                ""
            };
            format!("@{}{you} · {kind} · {harness} · {state}", m.name)
        })
        .collect()
}

/// `@name` as a target (§3.2): the named pane in the caller's workspace; outside a pane, the
/// one pane with that name across workspaces.
pub fn resolve_target(server: &Server, ctx: &Ctx, name: &str) -> Result<String, RpcError> {
    server.with_core(|c| {
        let ws = ctx
            .pane_scope
            .as_deref()
            .and_then(|p| c.pane(p))
            .map(|p| p.workspace.clone());
        let hits: Vec<&MentionName> = c
            .model
            .mentions
            .iter()
            .filter(|m| m.name == name && ws.as_ref().is_none_or(|w| &m.workspace == w))
            .collect();
        match hits.as_slice() {
            [m] => Ok(m.pane.clone()),
            [] => Err(not_found("pane", &format!("@{name}"))),
            _ => Err(err(
                ErrorKind::AmbiguousTarget,
                format!("@{name} names panes in several workspaces; use a pane id"),
            )
            .details(json!({
                "target": format!("@{name}"),
                "panes": hits.iter().map(|m| m.pane.clone()).collect::<Vec<_>>(),
            }))),
        }
    })
}

/// The pane a naming call acts on: a pane-scoped caller only its own (§2.3).
fn target_pane(server: &Server, ctx: &Ctx, p: &Value, method: &str) -> Result<Pane, RpcError> {
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    if let Some(own) = &ctx.pane_scope
        && &pane.id != own
    {
        return Err(denied(format!(
            "{method}: an agent may name only its own pane"
        )));
    }
    Ok(pane)
}

/// Why `name` cannot go to `pane`, if it cannot.
fn check(c: &Core, pane: &Pane, name: &str) -> Result<(), RpcError> {
    if !vm::valid(name) {
        return Err(invalid(format!(
            "`{name}`: a mention name matches [a-z][a-z0-9_-]{{0,31}}, does not end with - or _, and is not one of {}",
            vm::RESERVED.join(", ")
        )));
    }
    if let Some(other) = c
        .model
        .mentions
        .iter()
        .find(|m| m.workspace == pane.workspace && m.name == name && m.pane != pane.id)
    {
        return Err(err(
            ErrorKind::Conflict,
            format!("name_taken: @{name} already names another pane of this workspace"),
        )
        .details(json!({"reason": "name_taken", "name": name, "pane": other.pane})));
    }
    Ok(())
}

pub(super) fn list(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let ws = match s(p, "workspace") {
        Some(w) => Some(crate::api::resolve_ws(server, ctx, Some(w))?.id),
        None => match &ctx.pane_scope {
            Some(own) => server.with_core(|c| c.pane(own).map(|p| p.workspace.clone())),
            None => None,
        },
    };
    let unnamed = b(p, "unnamed").unwrap_or(false);
    Ok(server.with_core(|c| {
        let mut v: Vec<&MentionName> = c
            .model
            .mentions
            .iter()
            .filter(|m| ws.as_ref().is_none_or(|w| &m.workspace == w))
            .collect();
        v.sort_by(|a, b| (&a.workspace, &a.name).cmp(&(&b.workspace, &b.name)));
        let mut out = json!({"mentions": v.iter().map(|m| entry(c, m)).collect::<Vec<_>>()});
        if unnamed {
            let rest: Vec<Value> = c
                .model
                .panes
                .iter()
                .filter(|p| ws.as_ref().is_none_or(|w| &p.workspace == w))
                .filter(|p| nameable(p) && c.model.mention_of(&p.id).is_none())
                .map(|p| {
                    json!({
                        "pane": p.id,
                        "pane_handle": p.handle,
                        "workspace": p.workspace,
                        "suggested": suggest_for(c, p),
                        "label": label_of(c, p),
                        "harness": c.run_for_pane(&p.id).map(|r| r.harness.clone()),
                        "state": pane_state(c, p),
                    })
                })
                .collect();
            out["unnamed"] = json!(rest);
        }
        out
    }))
}

pub(super) fn suggest(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    Ok(server.with_core(|c| match c.model.mention_of(&pane.id) {
        Some(m) => json!({"name": m.name, "named": true}),
        None => json!({"name": suggest_for(c, &pane), "named": false}),
    }))
}

pub(super) fn set(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let pane = target_pane(server, ctx, p, "mention.set")?;
    if !nameable(&pane) {
        return Err(invalid(
            "browser panes and plugin surfaces cannot have a mention name",
        ));
    }
    let kind = match s(p, "kind") {
        None => None,
        Some("agent") => Some(MentionKind::Agent),
        Some("terminal") => Some(MentionKind::Terminal),
        Some(k) => return Err(invalid(format!("kind `{k}`: expected agent or terminal"))),
    };
    let mut c = server.core.lock().unwrap();
    let cur = c.model.mention_of(&pane.id).cloned();
    let name = match s(p, "name") {
        Some(n) => n.trim().trim_start_matches('@').to_string(),
        None => match &cur {
            Some(m) => m.name.clone(),
            None => suggest_for(&c, &pane),
        },
    };
    check(&c, &pane, &name)?;
    let m = MentionName {
        name: name.clone(),
        workspace: pane.workspace.clone(),
        pane: pane.id.clone(),
        kind: kind.or(cur.as_ref().map(|m| m.kind)).unwrap_or_default(),
        delivery: b(p, "delivery")
            .or(cur.as_ref().map(|m| m.delivery))
            .unwrap_or(true),
        created_at_ms: cur
            .as_ref()
            .filter(|m| m.name == name)
            .map(|m| m.created_at_ms)
            .unwrap_or_else(now),
    };
    if cur.as_ref() == Some(&m) {
        return Ok(serde_json::to_value(&m).unwrap_or(Value::Null));
    }
    let mut tx = Tx::new();
    tx.event(
        "mention.set",
        json!({"pane": m.pane, "workspace": m.workspace}),
        json!({
            "name": m.name,
            "kind": m.kind,
            "delivery": m.delivery,
            "previous": cur.as_ref().map(|x| x.name.clone()),
            "via": if ctx.pane_scope.is_some() { "agent" } else { "user" },
        }),
    );
    tx.mention(m.clone());
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    server.mentions.poke();
    Ok(serde_json::to_value(&m).unwrap_or(Value::Null))
}

pub(super) fn clear(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let pane = target_pane(server, ctx, p, "mention.clear")?;
    let mut c = server.core.lock().unwrap();
    let Some(m) = c.model.mention_of(&pane.id).cloned() else {
        return Ok(json!({}));
    };
    let mut tx = Tx::new();
    tx.clear_mention(&m);
    tx.event(
        "mention.cleared",
        json!({"pane": m.pane, "workspace": m.workspace}),
        json!({"name": m.name, "reason": "cleared"}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    server.mentions.poke();
    Ok(json!({}))
}

/// `agent.spawn` / `agent.start` with a `name` (§3.2): the pane takes it as its mention name
/// when it is valid, free in the workspace and the pane has no name yet.
pub(crate) fn adopt(server: &Server, pane: &str, name: &str) {
    if !vm::valid(name) {
        return;
    }
    let mut c = server.core.lock().unwrap();
    let Some(p) = c.pane(pane).cloned() else {
        return;
    };
    if !nameable(&p) || c.model.mention_of(&p.id).is_some() || check(&c, &p, name).is_err() {
        return;
    }
    let m = MentionName {
        name: name.to_string(),
        workspace: p.workspace.clone(),
        pane: p.id.clone(),
        kind: MentionKind::Agent,
        delivery: true,
        created_at_ms: now(),
    };
    let mut tx = Tx::new();
    tx.event(
        "mention.set",
        json!({"pane": m.pane, "workspace": m.workspace}),
        json!({"name": m.name, "kind": m.kind, "delivery": true, "previous": null, "via": "agent.start"}),
    );
    tx.mention(m);
    let _ = server.commit(&mut c, tx);
    drop(c);
    server.mentions.poke();
}
