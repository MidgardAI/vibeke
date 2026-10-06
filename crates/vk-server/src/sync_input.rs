//! `pane.sync_input` (07 §2.6, 08 §5): server-side synchronized input groups.
//!
//! A group is a set of terminal panes. While it exists, input a **user** sends to one member —
//! keys, pastes and mouse from an attached client (`render.rs`) and `pane.send_text|keys|bytes`
//! from a full-scope caller — is written to every other member too. Input from pane-scoped
//! callers (agents) is never fanned out.
//!
//! Agent exclusion is enforced here: a pane with a live agent run joins a group only when the
//! caller passes `include_agents: true`, and a member that starts an agent later stops
//! receiving mirrored input unless it was included that way. Browser panes, plugin surfaces and
//! exited panes never join; a member with an open interaction (input lock) is skipped.
//!
//! The TUI's own synchronized input (`vk-tui::sync_input`, prefix+shift+s) fans out on the
//! client and is independent of these groups; using both on the same panes mirrors twice.
//! Groups live in server memory (a restart ends them). A pane belongs to at most one group:
//! starting a group takes its panes out of older groups, and a group left with fewer than two
//! members ends. `pane.sync_input` is full scope only.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, resolve_pane, resolve_tab, s};
use crate::core::Tx;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use vk_proto::model::*;
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[("pane.sync_input", true)];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Group {
    pub id: String,
    /// Members, in the order they were given (pane ids).
    pub panes: Vec<String>,
    /// The tab the group was started for, if any.
    pub tab: Option<String>,
    /// Agent panes added explicitly with `include_agents`.
    pub agents: Vec<String>,
    pub created_at_ms: i64,
}

fn groups() -> &'static Mutex<Vec<Group>> {
    static G: OnceLock<Mutex<Vec<Group>>> = OnceLock::new();
    G.get_or_init(Mutex::default)
}

/// Groups whose members exist on this server (members that went away are dropped from the
/// view; a group with fewer than two live members is not shown).
fn live_groups(server: &Server) -> Vec<Group> {
    let all = groups().lock().unwrap().clone();
    server.with_core(|c| {
        all.into_iter()
            .filter_map(|mut g| {
                g.panes.retain(|p| c.pane(p).is_some());
                g.agents.retain(|p| g.panes.contains(p));
                (g.panes.len() >= 2).then_some(g)
            })
            .collect()
    })
}

fn group_json(server: &Server, g: &Group) -> Value {
    let handles: Vec<Value> = server.with_core(|c| {
        g.panes
            .iter()
            .map(|p| {
                json!({"pane": p, "handle": c.pane(p).map(|x| x.handle.clone()), "agent": c.run_for_pane(p).is_some()})
            })
            .collect()
    });
    json!({"id": g.id, "panes": g.panes, "members": handles, "tab": g.tab, "agents": g.agents, "created_at_ms": g.created_at_ms})
}

fn can_join(p: &Pane) -> Result<(), &'static str> {
    if p.is_browser() {
        return Err("browser_pane");
    }
    if p.plugin_surface().is_some() {
        return Err("plugin_surface");
    }
    if p.exited {
        return Err("exited");
    }
    Ok(())
}

/// The panes named by `panes` (array or comma-separated string).
fn pane_list(p: &Value) -> Option<Vec<String>> {
    match p.get("panes") {
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        Some(Value::String(s)) => Some(
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
        ),
        _ => None,
    }
}

fn emit(
    server: &Server,
    g: &Group,
    enabled: bool,
    reason: &str,
) -> Result<(), vk_proto::rpc::RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "pane.sync_input_changed",
        json!({"group": g.id}),
        json!({"enabled": enabled, "panes": g.panes, "tab": g.tab, "reason": reason}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

fn start(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let include_agents = b(p, "include_agents").unwrap_or(false);
    let (candidates, tab): (Vec<Pane>, Option<String>) = match pane_list(p) {
        Some(list) => {
            let mut v = vec![];
            for t in list {
                v.push(resolve_pane(server, ctx, Some(&t))?);
            }
            (v, None)
        }
        None => {
            let tab = resolve_tab(server, ctx, s(p, "tab"))?;
            let ids: Vec<String> = tab
                .layout
                .panes()
                .into_iter()
                .chain(tab.floating.iter().map(|f| f.pane.clone()))
                .collect();
            let panes = server.with_core(|c| {
                ids.iter()
                    .filter_map(|i| c.pane(i).cloned())
                    .collect::<Vec<_>>()
            });
            (panes, Some(tab.id))
        }
    };
    let mut members: Vec<String> = vec![];
    let mut agents: Vec<String> = vec![];
    let mut excluded: Vec<Value> = vec![];
    let mut seen = HashSet::new();
    for pane in candidates {
        if !seen.insert(pane.id.clone()) {
            continue;
        }
        if let Err(reason) = can_join(&pane) {
            excluded.push(json!({"pane": pane.id, "handle": pane.handle, "reason": reason}));
            continue;
        }
        let is_agent = server.with_core(|c| c.run_for_pane(&pane.id).is_some());
        if is_agent && !include_agents {
            excluded.push(json!({"pane": pane.id, "handle": pane.handle, "reason": "agent"}));
            continue;
        }
        if is_agent {
            agents.push(pane.id.clone());
        }
        members.push(pane.id);
    }
    if members.len() < 2 {
        return Err(err(
            ErrorKind::Conflict,
            "too_few_panes: synchronized input needs at least two eligible panes",
        )
        .details(json!({"reason": "too_few_panes", "excluded": excluded})));
    }
    let g = Group {
        id: format!("sg-{}", &crate::core::ulid()[16..]),
        panes: members.clone(),
        tab,
        agents,
        created_at_ms: vk_store::now_ms(),
    };
    // Take the members out of older groups; groups left with one member end.
    let ended: Vec<Group> = {
        let mut all = groups().lock().unwrap();
        for old in all.iter_mut() {
            old.panes.retain(|x| !members.contains(x));
            old.agents.retain(|x| !members.contains(x));
        }
        let (keep, ended): (Vec<Group>, Vec<Group>) =
            all.drain(..).partition(|x| x.panes.len() >= 2);
        *all = keep;
        all.push(g.clone());
        ended
    };
    for e in &ended {
        emit(server, e, false, "superseded")?;
    }
    emit(server, &g, true, "started")?;
    Ok(
        json!({"group_id": g.id, "group": group_json(server, &g), "excluded": excluded, "groups": live_groups(server).iter().map(|x| group_json(server, x)).collect::<Vec<_>>()}),
    )
}

fn stop(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let all = b(p, "all") == Some(true);
    let by_group = s(p, "group").map(str::to_string);
    let by_pane = match s(p, "pane") {
        Some(t) => Some(resolve_pane(server, ctx, Some(t))?.id),
        None => None,
    };
    let by_tab = match (s(p, "tab"), &by_group, &by_pane, all) {
        (Some(t), ..) => Some(resolve_tab(server, ctx, Some(t))?.id),
        // Nothing named: the caller's focused tab.
        (None, None, None, false) => Some(resolve_tab(server, ctx, None)?.id),
        _ => None,
    };
    let tab_panes: Vec<String> = match &by_tab {
        Some(t) => server.with_core(|c| c.panes_of_tab(t).iter().map(|x| x.id.clone()).collect()),
        None => vec![],
    };
    let live: HashSet<String> = live_groups(server).into_iter().map(|g| g.id).collect();
    let stopped: Vec<Group> = {
        let mut gs = groups().lock().unwrap();
        let (hit, keep): (Vec<Group>, Vec<Group>) = gs.drain(..).partition(|g| {
            live.contains(&g.id)
                && (all
                    || by_group.as_deref() == Some(g.id.as_str())
                    || by_pane.as_ref().is_some_and(|x| g.panes.contains(x))
                    || by_tab
                        .as_ref()
                        .is_some_and(|t| g.tab.as_deref() == Some(t.as_str()))
                    || g.panes.iter().any(|x| tab_panes.contains(x)))
        });
        *gs = keep;
        hit
    };
    for g in &stopped {
        emit(server, g, false, "stopped")?;
    }
    Ok(json!({
        "group_id": stopped.first().map(|g| g.id.clone()),
        "stopped": stopped.iter().map(|g| g.id.clone()).collect::<Vec<_>>(),
        "groups": live_groups(server).iter().map(|x| group_json(server, x)).collect::<Vec<_>>(),
    }))
}

fn status(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let gs = live_groups(server);
    let pane = match s(p, "pane") {
        Some(t) => Some(resolve_pane(server, ctx, Some(t))?.id),
        None => None,
    };
    let mine = pane
        .as_ref()
        .and_then(|x| gs.iter().find(|g| g.panes.contains(x)));
    Ok(json!({
        "group_id": mine.map(|g| g.id.clone()),
        "groups": gs.iter().map(|x| group_json(server, x)).collect::<Vec<_>>(),
    }))
}

fn sync_input(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let action = match (s(p, "action"), b(p, "enabled")) {
        (Some(a), _) => a,
        (None, Some(true)) => "start",
        (None, Some(false)) => "stop",
        (None, None) => "status",
    };
    match action {
        "start" | "on" => start(server, ctx, p),
        "stop" | "off" => stop(server, ctx, p),
        "status" => status(server, ctx, p),
        other => Err(invalid(format!(
            "action must be start|stop|status, not {other}"
        ))),
    }
}

/// Mirror a user's input to `pane` into the other members of its group. Called with the
/// bytes already encoded for `pane` (the same bytes go to every member).
pub fn mirror(server: &Server, pane: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let group = {
        let gs = groups().lock().unwrap();
        match gs.iter().find(|g| g.panes.iter().any(|x| x == pane)) {
            Some(g) => g.clone(),
            None => return,
        }
    };
    for member in group.panes.iter().filter(|x| x.as_str() != pane) {
        // Agent exclusion: a member that is running an agent now gets input only when it was
        // included explicitly.
        let agent = server.with_core(|c| c.run_for_pane(member).is_some());
        if agent && !group.agents.contains(member) {
            continue;
        }
        if server.agents.input_blocked(member).is_some() {
            continue;
        }
        if let Some(rt) = server.pane_rt(member) {
            rt.send(crate::pane::PaneCmd::Input {
                id: server.next_internal_input_id(),
                bytes: bytes.to_vec(),
                ack: None,
            });
        }
    }
}

/// The `sync_input` status segment for `pane` (08 §4).
pub fn segment(server: &Server, pane: Option<&str>) -> Value {
    let gs = live_groups(server);
    match pane.and_then(|p| gs.iter().find(|g| g.panes.iter().any(|x| x == p))) {
        Some(g) => json!({"enabled": true, "group": g.id, "panes": g.panes.len()}),
        None => json!({"enabled": false}),
    }
}

/// Dispatch hook for `pane.sync_input`.
pub fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    (method == "pane.sync_input").then(|| sync_input(server, ctx, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_list_accepts_arrays_and_comma_strings() {
        assert_eq!(
            pane_list(&json!({"panes": ["a", "b"]})),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            pane_list(&json!({"panes": "w1:p1, w1:p2"})),
            Some(vec!["w1:p1".to_string(), "w1:p2".to_string()])
        );
        assert_eq!(pane_list(&json!({})), None);
    }
}
