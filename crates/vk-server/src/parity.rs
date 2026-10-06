//! M4 parity polish, server/API side (11 §M4): dispatch for layouts, search, notifications and
//! theme, plus the model/API parts of groups (07 §2.4), floating panes (07 §2.6 `pane.float`,
//! 08 §5) and status-bar segment data (08 §4). Drawing all of these is TUI work.

use crate::api::{
    self, Ctx, R, b, cursor, err, internal, invalid, not_found, resolve_pane, resolve_tab,
    resolve_ws, s,
};
use crate::core::{Tx, ulid};
use crate::{Server, layouts, notify, search, theme};
use serde_json::{Value, json};
use std::sync::Arc;
use vk_proto::layout::{self, Direction};
use vk_proto::model::*;
use vk_proto::rpc::ErrorKind;

#[cfg(test)]
#[path = "parity_tests.rs"]
mod tests;

/// Methods added by this module (and the ones it dispatches to) for `api.methods`.
pub const METHODS: &[(&str, bool)] = &[
    ("layout.apply", true),
    ("layout.list", false),
    ("layout.get", false),
    ("client.appearance", true),
    ("client.focus", true),
    ("theme.get", false),
    ("theme.set_mode", true),
    ("notification.config", false),
    ("group.list", false),
    ("group.create", true),
    ("group.rename", true),
    ("group.move", true),
    ("group.delete", true),
    ("group.collapse", true),
    ("group.add", true),
    ("group.remove", true),
    ("pane.float", true),
    ("pane.embed", true),
    ("tab.floats", true),
    ("status.segments", false),
];

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    match method {
        "pane.read" if s(p, "source") == Some("archive") => {
            return Some(search::read_archive(server, ctx, p));
        }
        "search.query" => return Some(search::query(server, ctx, p)),
        "workspace.create" if p.get("group").is_some_and(|g| !g.is_null()) => {
            return Some(create_in_group(server, ctx, p).await);
        }
        "workspace.list" if p.get("_inner").is_none() => {
            return Some(workspace_list(server, ctx, p).await);
        }
        "workspace.move" if p.get("group").is_some() => {
            return Some(move_to_group(server, ctx, p));
        }
        _ => {}
    }
    if method.starts_with("group.") {
        return groups_api(server, ctx, method, p);
    }
    if let Some(r) = layouts::api(server, ctx, method, p) {
        return Some(r);
    }
    if let Some(r) = theme::api(server, ctx, method, p) {
        return Some(r);
    }
    if let Some(r) = notify::api(server, ctx, method, p) {
        return Some(r);
    }
    Some(match method {
        "pane.float" => pane_float(server, ctx, p),
        "pane.embed" => pane_embed(server, ctx, p),
        "tab.floats" => tab_floats(server, ctx, p),
        "status.segments" => status_segments(server, ctx, p),
        _ => return None,
    })
}

/// Re-dispatch a call through the full API (with `_inner` so this module passes through).
async fn inner(server: &Arc<Server>, ctx: &Ctx, method: &str, mut p: Value) -> R {
    if let Some(o) = p.as_object_mut() {
        o.remove("group");
        o.insert("_inner".into(), json!(true));
    }
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": p}).to_string();
    let resp: Value = serde_json::from_str(&Box::pin(api::handle_line(server, ctx, &line)).await)
        .map_err(internal)?;
    if let Some(e) = resp.get("error") {
        let e: vk_proto::rpc::RpcError = serde_json::from_value(e.clone()).map_err(internal)?;
        return Err(e);
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

// ---- groups (02 §1.1, 07 §2.4) --------------------------------------------------------------

fn resolve_group(server: &Server, t: &str) -> Result<Group, vk_proto::rpc::RpcError> {
    server
        .with_core(|c| c.group(t).cloned())
        .ok_or_else(|| not_found("group", t))
}

fn deny_pane(ctx: &Ctx, method: &str) -> Result<(), vk_proto::rpc::RpcError> {
    if ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is not allowed from a pane (workspace organisation is the user's)"),
        )
        .details(json!({"scope": "pane"})));
    }
    Ok(())
}

/// Put `ws` into `group` (None = ungrouped) at `index`, removing it from any other group.
fn set_membership(
    c: &crate::core::Core,
    tx: &mut Tx,
    ws: &str,
    group: Option<&str>,
    index: Option<usize>,
) {
    for g in &c.model.groups {
        let has = g.workspaces.iter().any(|w| w == ws);
        let target = group == Some(g.id.as_str());
        if !has && !target {
            continue;
        }
        let mut g2 = g.clone();
        g2.workspaces.retain(|w| w != ws && c.ws(w).is_some());
        if target {
            let i = index
                .unwrap_or(g2.workspaces.len())
                .min(g2.workspaces.len());
            g2.workspaces.insert(i, ws.to_string());
        }
        if g2 != *g {
            tx.group(g2);
        }
    }
}

fn group_json(c: &crate::core::Core, g: &Group) -> Value {
    let mut v = serde_json::to_value(g).unwrap_or(Value::Null);
    let live: Vec<&String> = g.workspaces.iter().filter(|w| c.ws(w).is_some()).collect();
    v["workspaces"] = json!(live);
    // Aggregate badge (08 §2.1): open interactions and working agents across members.
    let panes: Vec<&Pane> = c
        .model
        .panes
        .iter()
        .filter(|p| live.contains(&&p.workspace))
        .collect();
    let runs: Vec<&AgentRun> = c
        .model
        .runs
        .iter()
        .filter(|r| panes.iter().any(|p| p.id == r.pane))
        .collect();
    v["agent_summary"] = json!({
        "working": runs.iter().filter(|r| r.execution.value == Execution::Working).count(),
        "needs_input": c.model.interactions.iter().filter(|i| i.status == InteractionStatus::Open && panes.iter().any(|p| p.id == i.pane)).count(),
    });
    v
}

fn groups_api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if method != "group.list"
        && let Err(e) = deny_pane(ctx, method)
    {
        return Some(Err(e));
    }
    Some((|| {
        match method {
        "group.list" => Ok(server.with_core(|c| {
            let grouped: Vec<&String> = c
                .model
                .groups
                .iter()
                .flat_map(|g| g.workspaces.iter())
                .collect();
            let ungrouped: Vec<&String> = c
                .model
                .workspaces
                .iter()
                .map(|w| &w.id)
                .filter(|w| !grouped.contains(w))
                .collect();
            json!({"groups": c.model.groups.iter().map(|g| group_json(c, g)).collect::<Vec<_>>(), "ungrouped": ungrouped})
        })),
        "group.create" => {
            let name = s(p, "name")
                .filter(|n| !n.trim().is_empty())
                .ok_or_else(|| invalid("missing param `name`"))?;
            let parent = match s(p, "parent") {
                Some(t) => Some(resolve_group(server, t)?.id),
                None => None,
            };
            let mut c = server.core.lock().unwrap();
            if c.model.groups.iter().any(|g| g.name == name) {
                return Err(err(ErrorKind::Conflict, format!("name_taken: group {name}")));
            }
            let order = c.model.groups.iter().map(|g| g.order).fold(0.0, f64::max) + 1.0;
            let g = Group {
                id: ulid(),
                handle: c.next_group_handle(),
                name: name.to_string(),
                parent,
                collapsed: false,
                order,
                workspaces: vec![],
            };
            let mut tx = Tx::new();
            tx.counters = true;
            tx.group(g.clone());
            tx.event("group.created", json!({"group": g.id}), json!({"name": g.name, "parent": g.parent}));
            server.commit(&mut c, tx).map_err(internal)?;
            drop(c);
            Ok(json!({"group": g, "cursor": cursor(server, None)}))
        }
        "group.rename" | "group.collapse" | "group.move" => {
            let g = resolve_group(server, api::req(p, "group")?)?;
            let mut g2 = g.clone();
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            match method {
                "group.rename" => {
                    let name = s(p, "name")
                        .filter(|n| !n.trim().is_empty())
                        .ok_or_else(|| invalid("missing param `name`"))?;
                    if c.model.groups.iter().any(|x| x.name == name && x.id != g.id) {
                        return Err(err(ErrorKind::Conflict, format!("name_taken: group {name}")));
                    }
                    g2.name = name.to_string();
                    tx.event("group.renamed", json!({"group": g.id}), json!({"name": name}));
                }
                "group.collapse" => {
                    g2.collapsed = b(p, "collapsed").unwrap_or(!g.collapsed);
                    tx.event("group.collapsed", json!({"group": g.id}), json!({"collapsed": g2.collapsed}));
                }
                _ => {
                    if p.get("parent").is_some() {
                        g2.parent = match s(p, "parent") {
                            Some(t) => {
                                let par = c.group(t).cloned().ok_or_else(|| not_found("group", t))?;
                                // No cycles: the new parent must not be this group or below it.
                                let mut cur = Some(par.id.clone());
                                while let Some(id) = cur {
                                    if id == g.id {
                                        return Err(err(ErrorKind::Conflict, "group can't be moved under itself"));
                                    }
                                    cur = c.group(&id).and_then(|x| x.parent.clone());
                                }
                                Some(par.id)
                            }
                            None => None,
                        };
                    }
                    let mut sibs: Vec<Group> = c
                        .model
                        .groups
                        .iter()
                        .filter(|x| x.parent == g2.parent && x.id != g.id)
                        .cloned()
                        .collect();
                    let cur_i = c.model.groups.iter().filter(|x| x.parent == g2.parent).position(|x| x.id == g.id);
                    let idx = match (api::u(p, "index"), p.get("delta").and_then(Value::as_i64)) {
                        (Some(i), _) => i as usize,
                        (None, Some(d)) => (cur_i.unwrap_or(sibs.len()) as i64 + d).max(0) as usize,
                        _ => sibs.len(),
                    }
                    .min(sibs.len());
                    sibs.insert(idx, g2.clone());
                    for (k, mut x) in sibs.into_iter().enumerate() {
                        x.order = k as f64 + 1.0;
                        if x.id == g.id {
                            g2.order = x.order;
                        } else {
                            tx.group(x);
                        }
                    }
                    tx.event("group.moved", json!({"group": g.id}), json!({"parent": g2.parent, "index": idx}));
                }
            }
            tx.group(g2.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"group": g2}))
        }
        "group.delete" => {
            let g = resolve_group(server, api::req(p, "group")?)?;
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            // Members and child groups move up to the parent (nothing is closed).
            if let Some(par) = g.parent.as_deref().and_then(|x| c.group(x).cloned()) {
                let mut par = par;
                par.workspaces.extend(g.workspaces.iter().cloned());
                tx.group(par);
            }
            for child in c.model.groups.iter().filter(|x| x.parent.as_deref() == Some(&g.id)) {
                let mut ch = child.clone();
                ch.parent = g.parent.clone();
                tx.group(ch);
            }
            tx.close_group(&g);
            tx.event("group.closed", json!({"group": g.id}), json!({}));
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({}))
        }
        "group.add" | "group.remove" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let group = if method == "group.add" {
                Some(resolve_group(server, api::req(p, "group")?)?.id)
            } else {
                None
            };
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            set_membership(&c, &mut tx, &ws.id, group.as_deref(), api::u(p, "index").map(|i| i as usize));
            tx.event("workspace.moved", json!({"workspace": ws.id}), json!({"group": group}));
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"workspace": ws, "group": group}))
        }
        _ => Err(err(ErrorKind::MethodNotFound, format!("unknown method {method}"))),
    }
    })())
}

async fn create_in_group(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    deny_pane(ctx, "workspace.create {group}")?;
    let g = resolve_group(server, s(p, "group").unwrap_or_default())?;
    let r = inner(server, ctx, "workspace.create", p.clone()).await?;
    let ws = r["workspace"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    set_membership(&c, &mut tx, &ws, Some(&g.id), None);
    server.commit(&mut c, tx).map_err(internal)?;
    let mut r = r;
    r["group"] = json!(g.id);
    Ok(r)
}

fn move_to_group(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    deny_pane(ctx, "workspace.move")?;
    let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
    let group = match s(p, "group").filter(|g| !g.is_empty()) {
        Some(t) => Some(resolve_group(server, t)?.id),
        None => None,
    };
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    set_membership(
        &c,
        &mut tx,
        &ws.id,
        group.as_deref(),
        api::u(p, "index").map(|i| i as usize),
    );
    tx.event(
        "workspace.moved",
        json!({"workspace": ws.id}),
        json!({"group": group}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"workspace": ws, "group": group}))
}

async fn workspace_list(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let filter = match s(p, "group") {
        Some(t) => Some(resolve_group(server, t)?),
        None => None,
    };
    let mut r = inner(server, ctx, "workspace.list", p.clone()).await?;
    let groups = server.with_core(|c| c.model.groups.clone());
    if let Some(list) = r.get_mut("workspaces").and_then(Value::as_array_mut) {
        for w in list.iter_mut() {
            let id = w["id"].as_str().unwrap_or_default().to_string();
            w["group"] = json!(
                groups
                    .iter()
                    .find(|g| g.workspaces.contains(&id))
                    .map(|g| g.id.clone())
            );
        }
        if let Some(g) = &filter {
            list.retain(|w| w["group"].as_str() == Some(g.id.as_str()));
        }
    }
    Ok(r)
}

// ---- floating panes (02 §1.1, 08 §5) ---------------------------------------------------------

fn rect_from(p: &Value, base: Option<&FloatingPane>, pane: &str, z: u32) -> FloatingPane {
    let mut f = base
        .cloned()
        .unwrap_or_else(|| FloatingPane::centred(pane, z));
    if let Some(r) = p.get("rect") {
        let g = |k: &str| r.get(k).and_then(Value::as_f64).map(|v| v as f32);
        f.x = g("x").unwrap_or(f.x).clamp(0.0, 99.0);
        f.y = g("y").unwrap_or(f.y).clamp(0.0, 99.0);
        f.w = g("w").unwrap_or(f.w).clamp(1.0, 100.0);
        f.h = g("h").unwrap_or(f.h).clamp(1.0, 100.0);
    }
    f.z = z;
    f
}

fn top_z(tab: &Tab) -> u32 {
    tab.floating.iter().map(|f| f.z).max().unwrap_or(0) + 1
}

/// `pane.float {pane}`: float a tiled pane (or move/resize/raise a floating one).
/// `pane.float {tab?, cwd?, command?, rect?, focus?}` without `pane`: new floating pane.
fn pane_float(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if let Some(t) = s(p, "pane") {
        let pane = resolve_pane(server, ctx, Some(t))?;
        let mut c = server.core.lock().unwrap();
        let mut tab = c
            .tab(&pane.tab)
            .cloned()
            .ok_or_else(|| not_found("tab", &pane.tab))?;
        let z = top_z(&tab);
        if let Some(i) = tab.floating.iter().position(|f| f.pane == pane.id) {
            let f = rect_from(p, Some(&tab.floating[i]), &pane.id, z);
            tab.floating[i] = f;
        } else {
            let rest = layout::remove(&tab.layout, &pane.id).ok_or_else(|| {
                err(
                    ErrorKind::Conflict,
                    "the last tiled pane of a tab can't float",
                )
            })?;
            tab.layout = rest;
            if tab.zoomed_pane.as_deref() == Some(&pane.id) {
                tab.zoomed_pane = None;
            }
            tab.floating.push(rect_from(p, None, &pane.id, z));
        }
        tab.floats_hidden = false;
        let mut tx = Tx::new();
        tx.event(
            "tab.layout_changed",
            json!({"tab": tab.id}),
            json!({"floated": pane.id}),
        );
        tx.tab(tab.clone());
        server.commit(&mut c, tx).map_err(internal)?;
        drop(c);
        if b(p, "focus").unwrap_or(false) {
            server.focus_pane(&ctx.client_id, &pane.id);
        }
        return Ok(json!({"pane": pane, "tab": tab}));
    }
    let tab = resolve_tab(server, ctx, s(p, "tab"))?;
    let cwd = s(p, "cwd").map(str::to_string).or_else(|| {
        tab.focused_pane
            .as_deref()
            .and_then(|fp| server.pane_cwd(fp))
    });
    let created_by = match &ctx.pane_scope {
        Some(x) => format!("agent:{x}"),
        None => "user".into(),
    };
    let mut c = server.core.lock().unwrap();
    let mut tab = c
        .tab(&tab.id)
        .cloned()
        .ok_or_else(|| not_found("tab", &tab.id))?;
    let ws = c
        .ws(&tab.workspace)
        .cloned()
        .ok_or_else(|| not_found("workspace", &tab.workspace))?;
    let cwd = cwd.unwrap_or_else(|| ws.root_path.clone());
    let mut tx = Tx::new();
    let handle = tab.handle.clone();
    let pane = server
        .new_pane(
            &mut c,
            &mut tx,
            &ws,
            &tab.id,
            &handle,
            &cwd,
            api::argv(p, "command"),
            s(p, "title").map(str::to_string),
            &created_by,
        )
        .map_err(internal)?;
    let z = top_z(&tab);
    tab.floating.push(rect_from(p, None, &pane.id, z));
    tab.floats_hidden = false;
    tx.event(
        "tab.layout_changed",
        json!({"tab": tab.id}),
        json!({"floated": pane.id}),
    );
    tx.tab(tab.clone());
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    if b(p, "focus").unwrap_or(false) {
        server.focus_pane(&ctx.client_id, &pane.id);
    }
    Ok(json!({"pane": pane, "tab": tab, "cursor": cursor(server, None)}))
}

/// `pane.embed {pane, target?, direction?, ratio?}`: a floating pane back into the tiling.
fn pane_embed(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    let dir = Direction::parse(s(p, "direction").unwrap_or("right"))
        .ok_or_else(|| invalid("direction must be right|down|left|up"))?;
    let ratio = p.get("ratio").and_then(Value::as_f64).unwrap_or(0.5) as f32;
    let mut c = server.core.lock().unwrap();
    let mut tab = c
        .tab(&pane.tab)
        .cloned()
        .ok_or_else(|| not_found("tab", &pane.tab))?;
    let Some(i) = tab.floating.iter().position(|f| f.pane == pane.id) else {
        return Err(err(ErrorKind::Conflict, "pane is not floating"));
    };
    let tiled = tab.layout.panes();
    let target = s(p, "target")
        .and_then(|t| c.pane(t).map(|x| x.id.clone()))
        .filter(|t| tiled.contains(t))
        .or_else(|| tab.focused_pane.clone().filter(|f| tiled.contains(f)))
        .or_else(|| tiled.first().cloned())
        .ok_or_else(|| err(ErrorKind::Conflict, "tab has no tiled pane"))?;
    tab.floating.remove(i);
    layout::split(&mut tab.layout, &target, &pane.id, dir, ratio);
    let mut tx = Tx::new();
    tx.event(
        "tab.layout_changed",
        json!({"tab": tab.id}),
        json!({"embedded": pane.id}),
    );
    tx.tab(tab.clone());
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"pane": pane, "tab": tab}))
}

/// `tab.floats {tab?, visible?}`: show/hide all floats (toggle without `visible`).
fn tab_floats(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let tab = resolve_tab(server, ctx, s(p, "tab"))?;
    let mut c = server.core.lock().unwrap();
    let mut t = c
        .tab(&tab.id)
        .cloned()
        .ok_or_else(|| not_found("tab", &tab.id))?;
    t.floats_hidden = match b(p, "visible") {
        Some(v) => !v,
        None => !t.floats_hidden,
    };
    let mut tx = Tx::new();
    tx.event(
        "tab.layout_changed",
        json!({"tab": t.id}),
        json!({"floats_hidden": t.floats_hidden}),
    );
    tx.tab(t.clone());
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"tab": t}))
}

// ---- status bar segment data (08 §4) ---------------------------------------------------------

fn load_avg() -> Option<[f64; 3]> {
    let mut l = [0f64; 3];
    // SAFETY: getloadavg writes at most 3 doubles into the buffer.
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    (n == 3).then_some(l)
}

fn local_hhmm(ms: i64) -> String {
    // SAFETY: localtime_r writes into the provided struct only.
    unsafe {
        let t: libc::time_t = (ms / 1000) as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// Data for the built-in segments, from the point of view of a client (its focus). Rendering,
/// `mode` and `prefix_indicator` are client-side.
fn status_segments(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let client = s(p, "client")
        .map(str::to_string)
        .unwrap_or_else(|| ctx.client_id.clone());
    let mut focus = server.client_focus(&client);
    if focus.pane.is_none()
        && let Some(rc) = notify::recent_client(server)
    {
        focus = server.client_focus(&rc);
    }
    if let Some(t) = s(p, "pane") {
        let pn = resolve_pane(server, ctx, Some(t))?;
        focus = ClientFocus {
            workspace: Some(pn.workspace.clone()),
            tab: Some(pn.tab.clone()),
            pane: Some(pn.id.clone()),
        };
    }
    let now = vk_store::now_ms();
    let v = server.with_core(|c| {
        let ws = focus.workspace.as_deref().and_then(|w| c.ws(w)).cloned();
        let task = ws
            .as_ref()
            .and_then(|w| w.task.as_deref())
            .and_then(|t| c.task(t))
            .cloned();
        let seen: std::collections::HashMap<String, u64> =
            c.store.reads("local").unwrap_or_default().into_iter().collect();
        let open: Vec<&Interaction> = c
            .model
            .interactions
            .iter()
            .filter(|i| i.status == InteractionStatus::Open)
            .collect();
        let unfocused: Vec<&&Interaction> = open
            .iter()
            .filter(|i| focus.pane.as_deref() != Some(&i.pane))
            .collect();
        let oldest = unfocused.iter().min_by_key(|i| i.opened_at_ms);
        let runs = &c.model.runs;
        let done = runs
            .iter()
            .filter(|r| {
                r.execution.value == Execution::Idle
                    && r.done_rev > seen.get(&r.pane).copied().unwrap_or(0)
            })
            .count();
        let ports_previews: Vec<Value> = c
            .model
            .previews
            .iter()
            .filter(|pv| {
                matches!(pv.status, PreviewStatus::Up | PreviewStatus::Declared)
                    && (task.as_ref().is_some_and(|t| pv.task.as_deref() == Some(&t.id))
                        || ws.as_ref().is_some_and(|w| {
                            pv.pane
                                .as_deref()
                                .and_then(|pp| c.pane(pp))
                                .is_some_and(|pp| pp.workspace == w.id)
                        }))
            })
            .map(|pv| json!({"handle": pv.handle, "port": pv.port, "url": pv.url, "status": pv.status.as_str()}))
            .collect();
        json!({
            "machine": {"label": c.model.machine},
            "session": {"name": c.model.session},
            "workspace": ws.as_ref().map(|w| json!({"id": w.id, "handle": w.handle, "name": w.display_name()})),
            "task": task.as_ref().map(|t| json!({"id": t.id, "handle": t.handle, "title": t.title, "status": t.status, "review_label": t.review_label})),
            "branch": focus.pane.as_deref().and_then(|p| c.pane(p)).and_then(|p| p.jj.clone()).or_else(|| ws.as_ref().and_then(|w| w.branch.clone())).or_else(|| task.as_ref().and_then(|t| t.branch.clone())).map(|b| json!({"name": b})),
            "ports": {"range": task.as_ref().and_then(|t| t.port_range), "previews": ports_previews},
            "attention": {
                "count": unfocused.len(),
                "oldest": oldest.map(|i| json!({"interaction": i.id, "handle": i.handle, "pane": i.pane, "title": i.title, "kind": i.kind.as_str(), "opened_at_ms": i.opened_at_ms})),
            },
            "agents_summary": {
                "working": runs.iter().filter(|r| r.execution.value == Execution::Working).count(),
                "done": done,
                "needs_input": open.iter().map(|i| &i.run).collect::<std::collections::HashSet<_>>().len(),
                "error": runs.iter().filter(|r| matches!(r.execution.value, Execution::Error | Execution::RateLimited)).count(),
                "total": runs.len(),
            },
            "sync_input": {"enabled": false},
        })
    });
    let mut segs = v;
    segs["cpu"] = json!({"load": load_avg()});
    segs["clock"] = json!({"ms": now, "local": local_hhmm(now)});
    let theme = server.theme.current();
    Ok(json!({
        "segments": segs,
        "focus": focus,
        "appearance": theme,
        "client_side": ["mode", "prefix_indicator"],
    }))
}

// ---- task checkouts: worktree | jj_workspace | none (05 §4) ----------------------------------

/// The chosen code-isolation backend and the repo root it works from.
#[derive(Debug, Clone)]
pub struct CheckoutChoice {
    pub kind: &'static str,
    pub root: std::path::PathBuf,
}

/// `task.create {isolation | checkout}` (default `tasks.checkout`): `auto` picks a jj workspace
/// when the repo has `.jj` and `jj` is installed, else a worktree.
pub fn resolve_checkout(repo: &str, p: &Value) -> Result<CheckoutChoice, vk_proto::rpc::RpcError> {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.tasks)
        .unwrap_or_default();
    let want = s(p, "isolation")
        .or_else(|| s(p, "checkout"))
        .map(str::to_string)
        .unwrap_or_else(|| match cfg.vcs {
            vk_config::Vcs::Jj => "jj".into(),
            vk_config::Vcs::Git if cfg.checkout == vk_config::Checkout::Auto => "worktree".into(),
            _ => cfg.checkout.as_str().to_string(),
        });
    let path = std::path::Path::new(repo);
    let jj_root = vk_tasks::jj_root(path);
    let vcs_root = vk_tasks::repo_root(path).map(|i| i.root);
    let jj_ok = || vk_tasks::Jj::default().available();
    let no_repo = || invalid(format!("{repo} is not inside a repository"));
    Ok(match want.as_str() {
        "jj_workspace" | "jj" => {
            let root = jj_root
                .ok_or_else(|| invalid(format!("{repo} is not inside a jj repository (no .jj)")))?;
            if !jj_ok() {
                return Err(err(
                    ErrorKind::Unsupported,
                    "jj is not installed (or not on PATH; set VIBEKE_JJ)",
                ));
            }
            CheckoutChoice {
                kind: "jj_workspace",
                root,
            }
        }
        "worktree" => CheckoutChoice {
            kind: "worktree",
            root: vcs_root.ok_or_else(no_repo)?,
        },
        "none" => CheckoutChoice {
            kind: "none",
            root: vcs_root
                .or(jj_root)
                .or_else(|| path.canonicalize().ok())
                .ok_or_else(no_repo)?,
        },
        "auto" => match (jj_root, vcs_root) {
            (Some(root), _) if jj_ok() => CheckoutChoice {
                kind: "jj_workspace",
                root,
            },
            (_, Some(root)) => CheckoutChoice {
                kind: "worktree",
                root,
            },
            _ => return Err(no_repo()),
        },
        "clone" => {
            return Err(err(
                ErrorKind::Unsupported,
                "clone checkouts ship with `task sync` (13 §6)",
            ));
        }
        other => {
            return Err(invalid(format!(
                "unknown isolation {other} (worktree|jj_workspace|none|auto)"
            )));
        }
    })
}

/// Create the checkout (blocking: shells out to the VCS).
pub fn create_checkout(
    kind: &str,
    req: &vk_tasks::CreateRequest,
    cfg: &vk_tasks::WorktreeConfig,
) -> vk_tasks::Result<vk_tasks::Checkout> {
    match kind {
        "jj_workspace" => vk_tasks::Jj::default().create_workspace(req, cfg),
        "none" => {
            let root = req.repo.canonicalize().unwrap_or_else(|_| req.repo.clone());
            let info = vk_tasks::repo_root(&root);
            Ok(vk_tasks::Checkout {
                path: root.clone(),
                branch: info.as_ref().and_then(|i| i.current_branch.clone()),
                base_ref: None,
                slug: req
                    .slug
                    .clone()
                    .unwrap_or_else(|| vk_tasks::slugify(&req.title, cfg.slug_max_len)),
                repo_root: info.map(|i| i.root).unwrap_or(root),
                created_branch: false,
                fetch: vk_tasks::FetchOutcome::Skipped,
                warnings: vec![
                    "shared cwd: no checkout isolation (collision warnings apply)".into(),
                ],
            })
        }
        _ => vk_tasks::create_worktree(req, cfg),
    }
}

/// `task.finish {remove_worktree}` for a jj task: forget the workspace, delete the directory.
pub fn remove_jj_task(server: &Arc<Server>, task: &Task) -> Option<String> {
    let path = task.worktree_path.clone()?;
    let srv = server.clone();
    let (id, root, name) = (task.id.clone(), task.repo_root.clone(), task.slug.clone());
    let job = format!("j{}", &ulid()[20..]);
    let jid = job.clone();
    std::thread::spawn(move || {
        let r = vk_tasks::Jj::default().remove_workspace(
            std::path::Path::new(&root),
            &name,
            std::path::Path::new(&path),
        );
        let state = match &r {
            Ok(()) => "Done".to_string(),
            Err(e) => format!("Failed: {e}"),
        };
        let mut c = srv.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "worktree.removed",
            json!({"task": id, "path": path}),
            json!({"job": jid, "vcs": "jj", "state": state}),
        );
        let _ = srv.commit(&mut c, tx);
    });
    Some(job)
}

/// `task.get` for a jj task: workspace, bookmark and change status (05 §13 "jj: bookmark").
pub fn jj_task_status(task: &Task) -> Value {
    let jj = vk_tasks::Jj::default();
    let st = task
        .worktree_path
        .as_deref()
        .map(|w| jj.status(std::path::Path::new(w)));
    let (status, error) = match st {
        Some(Ok(s)) => (Some(s), None),
        Some(Err(e)) => (None, Some(e.to_string())),
        None => (None, None),
    };
    json!({
        "task": task,
        "branch_status": status.as_ref().map(|s| json!({
            "vcs": "jj",
            "branch": task.branch,
            "bookmark_exists": task.branch.as_ref().is_some_and(|b| s.bookmarks.contains(b)),
            "bookmarks": s.bookmarks, "change_id": s.change_id, "commit_id": s.commit_id,
            "dirty": !s.empty, "conflict": s.conflict, "description": s.description,
        })),
        "jj": {
            "workspace": task.slug,
            "colocated": vk_tasks::jj_colocated(std::path::Path::new(&task.repo_root)),
            "error": error,
        },
    })
}
