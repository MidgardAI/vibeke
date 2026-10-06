//! Herdr socket methods added in M5 slice 2 (07 §8.3): layouts, pane process info/move/swap,
//! metadata reporting, window title, `agent.start/prompt/wait/rename`, `worktree.create/open`,
//! `plugin.link/unlink/enable/disable` and `plugin.pane.open/focus/close`.
//!
//! Result types and parameter names follow the spec's mapping table where it names them; the
//! rest are *unverified* against the baseline schema (inventory: partial) and pinned by the
//! differential suite once it runs.

use super::{
    Caller, Snap, WireError, brokers, launcher, native, ok, pane_target, plugin_dirs, req_s,
    run_target, snap, sp, state, tab_target, typed, ws_target,
};
use crate::Server;
use crate::core::{Tx, subject_pane};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use vk_compat::herdr::registry::{self, Registry, RegistryError, Status};
use vk_compat::herdr::{self, launch, status};
use vk_proto::layout::{self, Direction};
use vk_proto::model::{LayoutNode, Pane, SplitDir, Tab};

/// Methods handled here.
pub const METHODS: &[&str] = &[
    "layout.export",
    "layout.apply",
    "layout.set_split_ratio",
    "pane.process_info",
    "pane.move",
    "pane.swap",
    "pane.report_metadata",
    "workspace.report_metadata",
    "client.window_title.set",
    "client.window_title.clear",
    "agent.start",
    "agent.prompt",
    "agent.wait",
    "agent.rename",
    "worktree.create",
    "worktree.open",
    "plugin.link",
    "plugin.unlink",
    "plugin.enable",
    "plugin.disable",
    "plugin.pane.open",
    "plugin.pane.focus",
    "plugin.pane.close",
];

/// A pane opened by `plugin.pane.open`.
#[derive(Debug, Clone)]
pub struct PluginPane {
    pub plugin: String,
    pub entrypoint: String,
    pub placement: String,
    /// Focus to restore when a transient (overlay/zoomed) pane goes away.
    pub prev_focus: Option<String>,
}

/// Per-server compat state that is not in Vibeke's model.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub panes: HashMap<String, Map<String, Value>>,
    pub workspaces: HashMap<String, Map<String, Value>>,
    pub window_title: Option<String>,
    pub plugin_panes: HashMap<String, PluginPane>,
}

fn invalid(msg: impl Into<String>) -> WireError {
    WireError::new("invalid_params", msg)
}

fn denied(msg: impl Into<String>) -> WireError {
    WireError::new("permission_denied", msg)
}

fn owns(caller: &Caller, p: &Pane) -> bool {
    match &caller.ctx.pane_scope {
        None => true,
        Some(s) => &p.id == s || p.created_by == format!("agent:{s}"),
    }
}

// ---- layout snapshots ---------------------------------------------------------------------------

fn dir_name(d: SplitDir) -> &'static str {
    match d {
        SplitDir::Horizontal => "horizontal",
        SplitDir::Vertical => "vertical",
    }
}

fn node_json(sn: &Snap, node: &LayoutNode, path: &str) -> Value {
    match node {
        LayoutNode::Leaf { pane } => json!({
            "type": "pane",
            "pane_id": sn.pane(pane).map(|p| p.handle.clone()),
        }),
        LayoutNode::Split { dir, children } => binary(sn, *dir, children, path),
    }
}

/// Vibeke splits are n-ary; the snapshot nests them as binary splits (`first` = one child,
/// `second` = the rest), each with the share of its first side and a stable `split_id`.
fn binary(sn: &Snap, dir: SplitDir, children: &[(LayoutNode, f32)], path: &str) -> Value {
    if children.len() == 1 {
        return node_json(sn, &children[0].0, path);
    }
    let total: f32 = children.iter().map(|(_, r)| r).sum();
    let ratio = if total > 0.0 {
        children[0].1 / total
    } else {
        0.5
    };
    json!({
        "type": "split",
        "split_id": path,
        "direction": dir_name(dir),
        "ratio": (ratio * 1000.0).round() / 1000.0,
        "first": node_json(sn, &children[0].0, &format!("{path}0")),
        "second": binary(sn, dir, &children[1..], &format!("{path}1")),
    })
}

/// The `PaneLayoutSnapshot` projection of one tab (shape unverified against the baseline).
pub fn tab_snapshot(sn: &Snap, t: &Tab) -> Value {
    let h = |id: &Option<String>| {
        id.as_deref()
            .and_then(|p| sn.pane(p))
            .map(|p| p.handle.clone())
    };
    json!({
        "tab_id": t.handle,
        "workspace_id": sn.ws_handle(&t.workspace),
        "focused_pane_id": h(&t.focused_pane),
        "zoomed_pane_id": h(&t.zoomed_pane),
        "root": node_json(sn, &t.layout, "s"),
    })
}

/// Set the share of the first side of binary split `path` (`s`, `s0`, `s1`, …).
fn set_ratio(node: &mut LayoutNode, path: &[u8], r: f32) -> bool {
    let LayoutNode::Split { children, .. } = node else {
        return false;
    };
    let mut j = 0usize;
    let mut rest = path;
    loop {
        let Some((&d, more)) = rest.split_first() else {
            if children.len() - j < 2 {
                return false;
            }
            let total: f32 = children[j..].iter().map(|(_, x)| x).sum();
            let old_rest = total - children[j].1;
            let first = r.clamp(0.05, 0.95) * total;
            let n_rest = (children.len() - j - 1) as f32;
            children[j].1 = first;
            for c in children[j + 1..].iter_mut() {
                c.1 = if old_rest > 0.0 {
                    c.1 / old_rest * (total - first)
                } else {
                    (total - first) / n_rest
                };
            }
            return true;
        };
        rest = more;
        match d {
            b'0' => return set_ratio(&mut children[j].0, rest, r),
            b'1' if children.len() - (j + 1) >= 2 => j += 1,
            b'1' => return set_ratio(&mut children[j + 1].0, rest, r),
            _ => return false,
        }
    }
}

/// The binary split (snapshot `split_id`) one of whose sides is exactly the leaf `pane`.
fn split_of_leaf(node: &LayoutNode, pane: &str, path: &str) -> Option<String> {
    match node {
        LayoutNode::Leaf { .. } => None,
        LayoutNode::Split { children, .. } => scan_split(children, pane, path),
    }
}

fn scan_split(children: &[(LayoutNode, f32)], pane: &str, path: &str) -> Option<String> {
    let is = |n: &LayoutNode| matches!(n, LayoutNode::Leaf { pane: x } if x == pane);
    if children.len() == 1 {
        return split_of_leaf(&children[0].0, pane, path);
    }
    if is(&children[0].0) || (children.len() == 2 && is(&children[1].0)) {
        return Some(path.to_string());
    }
    if let Some(x) = split_of_leaf(&children[0].0, pane, &format!("{path}0")) {
        return Some(x);
    }
    if children.len() == 2 {
        split_of_leaf(&children[1].0, pane, &format!("{path}1"))
    } else {
        scan_split(&children[1..], pane, &format!("{path}1"))
    }
}

/// Parse a snapshot node back into a layout over `allowed` panes (Vibeke ids by handle).
fn parse_node(
    v: &Value,
    resolve: &dyn Fn(&str) -> Option<String>,
    seen: &mut Vec<String>,
) -> Result<LayoutNode, WireError> {
    match v.get("type").and_then(Value::as_str) {
        Some("pane") => {
            let h = v
                .get("pane_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("pane node without pane_id"))?;
            let id = resolve(h)
                .ok_or_else(|| WireError::new("pane_not_found", format!("pane not in tab: {h}")))?;
            if seen.contains(&id) {
                return Err(invalid(format!("pane {h} appears twice")));
            }
            seen.push(id.clone());
            Ok(LayoutNode::Leaf { pane: id })
        }
        Some("split") => {
            let dir = match v.get("direction").and_then(Value::as_str) {
                Some("horizontal" | "right" | "left") => SplitDir::Horizontal,
                Some("vertical" | "down" | "up") => SplitDir::Vertical,
                _ => return Err(invalid("split direction must be horizontal|vertical")),
            };
            let r = v.get("ratio").and_then(Value::as_f64).unwrap_or(0.5) as f32;
            let r = r.clamp(0.05, 0.95);
            let a = parse_node(
                v.get("first")
                    .ok_or_else(|| invalid("split without first"))?,
                resolve,
                seen,
            )?;
            let b = parse_node(
                v.get("second")
                    .ok_or_else(|| invalid("split without second"))?,
                resolve,
                seen,
            )?;
            Ok(LayoutNode::Split {
                dir,
                children: vec![(a, r), (b, 1.0 - r)],
            })
        }
        _ => Err(invalid("layout node type must be pane|split")),
    }
}

/// The tab a layout request targets: `tab_id`, else the pane's tab, else the focused tab.
fn layout_tab(caller: &Caller, sn: &Snap, p: &Value) -> Result<Tab, WireError> {
    if sp(p, "tab_id").is_some() {
        return tab_target(sn, p);
    }
    let pane = pane_target(caller, sn, p)?;
    sn.pane(&pane)
        .and_then(|x| sn.tab(&x.tab))
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", "no tab"))
}

fn commit_layout(server: &Server, tab: &Tab, f: impl FnOnce(&mut Tab)) -> Result<Tab, WireError> {
    let mut c = server.core.lock().unwrap();
    let mut t = c
        .tab(&tab.id)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", tab.handle.clone()))?;
    f(&mut t);
    let mut tx = Tx::new();
    tx.event("tab.layout_changed", json!({"tab": t.id}), json!({}));
    tx.tab(t.clone());
    server
        .commit(&mut c, tx)
        .map_err(|e| WireError::new("internal_error", e.to_string()))?;
    Ok(t)
}

// ---- pane move / swap -------------------------------------------------------------------------

fn replace_leaf(node: &mut LayoutNode, from: &str, to: &str) {
    match node {
        LayoutNode::Leaf { pane } if pane == from => *pane = to.to_string(),
        LayoutNode::Leaf { .. } => {}
        LayoutNode::Split { children, .. } => {
            for (c, _) in children {
                replace_leaf(c, from, to);
            }
        }
    }
}

fn internal(e: impl std::fmt::Display) -> WireError {
    WireError::new("internal_error", e.to_string())
}

/// Move `pane` into `dest` tab of the same workspace, split next to `anchor` (default: the
/// destination's focused pane) in `dir`. An emptied source tab closes.
fn move_pane(
    server: &Server,
    pane: &str,
    dest: &str,
    anchor: Option<&str>,
    dir: Direction,
) -> Result<(), WireError> {
    let mut c = server.core.lock().unwrap();
    let mut p = c
        .pane(pane)
        .cloned()
        .ok_or_else(|| WireError::new("pane_not_found", pane.to_string()))?;
    let mut src = c
        .tab(&p.tab)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", p.tab.clone()))?;
    let dst0 = c
        .tab(dest)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", dest.to_string()))?;
    if src.workspace != dst0.workspace {
        return Err(WireError::new(
            "unsupported",
            "moving a pane to another workspace is not supported yet (Herdr ids carry the workspace)",
        ));
    }
    if src.floating.iter().any(|f| f.pane == p.id) {
        return Err(invalid("floating panes cannot be moved"));
    }
    if anchor == Some(pane) {
        return Err(invalid("a pane cannot be moved next to itself"));
    }
    let same = src.id == dst0.id;
    let mut tx = Tx::new();
    let mut src_closed = false;
    let mut dst = match layout::remove(&src.layout, pane) {
        Some(l) => {
            src.layout = l;
            if src.focused_pane.as_deref() == Some(pane) {
                src.focused_pane = src.layout.panes().first().cloned();
            }
            if src.zoomed_pane.as_deref() == Some(pane) {
                src.zoomed_pane = None;
            }
            if same { src.clone() } else { dst0 }
        }
        None if same => return Err(invalid("the pane is alone in its tab")),
        None => {
            src_closed = true;
            dst0
        }
    };
    let anchor = anchor
        .map(str::to_string)
        .or_else(|| dst.focused_pane.clone().filter(|f| f != pane))
        .or_else(|| dst.layout.panes().into_iter().find(|x| x != pane))
        .ok_or_else(|| invalid("destination tab has no pane to split"))?;
    if !layout::split(&mut dst.layout, &anchor, pane, dir, 0.5) {
        return Err(WireError::new(
            "pane_not_found",
            "target pane is not in the destination tab",
        ));
    }
    dst.zoomed_pane = None;
    p.tab = dst.id.clone();
    if src_closed {
        tx.event(
            "tab.closed",
            json!({"tab": src.id, "workspace": src.workspace}),
            json!({}),
        );
        tx.close_tab(&src);
    } else if !same {
        tx.event("tab.layout_changed", json!({"tab": src.id}), json!({}));
        tx.tab(src.clone());
    }
    tx.event("tab.layout_changed", json!({"tab": dst.id}), json!({}));
    tx.event(
        "pane.moved",
        subject_pane(&p),
        json!({"from_tab_id": src.handle, "to_tab_id": dst.handle}),
    );
    tx.tab(dst);
    tx.pane(p);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

fn swap_panes(server: &Server, a: &str, b: &str) -> Result<(), WireError> {
    if a == b {
        return Err(invalid("cannot swap a pane with itself"));
    }
    let mut c = server.core.lock().unwrap();
    let get = |c: &crate::core::Core, id: &str| {
        c.pane(id)
            .cloned()
            .ok_or_else(|| WireError::new("pane_not_found", id.to_string()))
    };
    let (mut pa, mut pb) = (get(&c, a)?, get(&c, b)?);
    if pa.workspace != pb.workspace {
        return Err(WireError::new(
            "unsupported",
            "swapping panes across workspaces is not supported yet",
        ));
    }
    let mut ta = c
        .tab(&pa.tab)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", pa.tab.clone()))?;
    let mut tx = Tx::new();
    let tmp = "\u{0}swap";
    if pa.tab == pb.tab {
        replace_leaf(&mut ta.layout, a, tmp);
        replace_leaf(&mut ta.layout, b, a);
        replace_leaf(&mut ta.layout, tmp, b);
        tx.event("tab.layout_changed", json!({"tab": ta.id}), json!({}));
        tx.tab(ta.clone());
        for p in [&pa, &pb] {
            tx.event(
                "pane.moved",
                subject_pane(p),
                json!({"from_tab_id": ta.handle, "to_tab_id": ta.handle}),
            );
        }
    } else {
        let mut tb = c
            .tab(&pb.tab)
            .cloned()
            .ok_or_else(|| WireError::new("tab_not_found", pb.tab.clone()))?;
        replace_leaf(&mut ta.layout, a, b);
        replace_leaf(&mut tb.layout, b, a);
        for (t, from, to) in [(&mut ta, a, b), (&mut tb, b, a)] {
            if t.focused_pane.as_deref() == Some(from) {
                t.focused_pane = Some(to.to_string());
            }
            if t.zoomed_pane.as_deref() == Some(from) {
                t.zoomed_pane = None;
            }
        }
        pa.tab = tb.id.clone();
        pb.tab = ta.id.clone();
        for t in [&ta, &tb] {
            tx.event("tab.layout_changed", json!({"tab": t.id}), json!({}));
        }
        tx.event(
            "pane.moved",
            subject_pane(&pa),
            json!({"from_tab_id": ta.handle, "to_tab_id": tb.handle}),
        );
        tx.event(
            "pane.moved",
            subject_pane(&pb),
            json!({"from_tab_id": tb.handle, "to_tab_id": ta.handle}),
        );
        tx.tab(ta);
        tx.tab(tb);
        tx.pane(pa);
        tx.pane(pb);
    }
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

// ---- metadata ---------------------------------------------------------------------------------

/// `{metadata: {...}}`, `{key, value}` or the remaining params; `null` removes a key and
/// `clear: true` starts over.
fn merge_meta(dst: &mut Map<String, Value>, p: &Value, skip: &[&str]) {
    if p.get("clear").and_then(Value::as_bool) == Some(true) {
        dst.clear();
    }
    let mut set = |k: &str, v: &Value| {
        if v.is_null() {
            dst.remove(k);
        } else {
            dst.insert(k.to_string(), v.clone());
        }
    };
    if let Some(Value::Object(m)) = p.get("metadata") {
        for (k, v) in m {
            set(k, v);
        }
    } else if let Some(k) = sp(p, "key") {
        set(k, p.get("value").unwrap_or(&Value::Null));
    } else if let Some(o) = p.as_object() {
        for (k, v) in o {
            if !skip.contains(&k.as_str()) && k != "clear" {
                set(k, v);
            }
        }
    }
}

// ---- plugins ----------------------------------------------------------------------------------

fn reg_err(e: RegistryError) -> WireError {
    match e {
        RegistryError::NotFound(id) => {
            WireError::new("plugin_not_found", format!("plugin not found: {id}"))
        }
        RegistryError::Conflict(m) => WireError::new("conflict", m),
        RegistryError::Manifest(m) => WireError::new("invalid_manifest", m.to_string()),
        e => internal(e),
    }
}

fn plugin_json(id: &str) -> Value {
    super::plugin_list()
        .into_iter()
        .find(|p| p["plugin_id"] == id)
        .unwrap_or(Value::Null)
}

fn registry_change(
    caller: &Caller,
    f: impl FnOnce(&mut Registry) -> Result<String, RegistryError>,
) -> Result<Value, WireError> {
    if caller.ctx.pane_scope.is_some() {
        return Err(denied(
            "plugin registrations cannot be changed from a pane; ask the user to run it",
        ));
    }
    let dirs = plugin_dirs();
    let mut reg = Registry::load(&dirs).map_err(reg_err)?;
    let id = f(&mut reg).map_err(reg_err)?;
    reg.save(&dirs).map_err(reg_err)?;
    Ok(typed("plugin_info", json!({"plugin": plugin_json(&id)})))
}

/// The wrapper that runs a plugin pane's command inside a Vibeke pane: the private launcher
/// first on `PATH`, `HERDR_PANE_ID`/`HERDR_TAB_ID`/`HERDR_WORKSPACE_ID` from the pane's own
/// identity, then the invocation's `HERDR_*` values through `env`.
const PANE_WRAP: &str = "PATH=\"$1:$PATH\"; HERDR_PANE_ID=\"$VIBEKE_PANE_ID\"; \
HERDR_TAB_ID=\"$VIBEKE_TAB_ID\"; HERDR_WORKSPACE_ID=\"$VIBEKE_WORKSPACE_ID\"; \
export PATH HERDR_PANE_ID HERDR_TAB_ID HERDR_WORKSPACE_ID; shift; exec \"$@\"";

async fn plugin_pane_open(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    if caller.ctx.pane_scope.is_some() {
        return Err(denied(
            "legacy plugin panes cannot be opened from a pane; ask the user to run it",
        ));
    }
    let plugin = sp(p, "plugin_id")
        .or(sp(p, "plugin"))
        .ok_or_else(|| invalid("plugin_id is required"))?;
    let ep = sp(p, "pane")
        .or(sp(p, "entrypoint"))
        .or(sp(p, "entrypoint_id"))
        .or(sp(p, "pane_id_entrypoint"))
        .ok_or_else(|| invalid("pane (entrypoint) is required"))?;
    let reg = Registry::load(&plugin_dirs()).map_err(reg_err)?;
    let entry = reg.get(plugin).map_err(reg_err)?.clone();
    let (st, m) = registry::entry_status(&entry);
    if st != Status::Active {
        return Err(denied(format!("plugin {plugin} is {}", st.as_str())));
    }
    let m = m.expect("active plugins have a manifest");
    let pf = herdr::current_platform();
    let decl = m
        .panes
        .iter()
        .find(|d| d.id == ep && m.entry_on(d.platforms.as_ref(), pf))
        .cloned()
        .ok_or_else(|| {
            WireError::new(
                "plugin_pane_not_found",
                format!("{plugin} has no pane `{ep}` on {pf}"),
            )
        })?;
    let placement = sp(p, "placement").unwrap_or(&decl.placement).to_string();
    match placement.as_str() {
        "split" | "tab" | "zoomed" | "overlay" => {}
        "popup" => {
            return Err(WireError::new(
                "unsupported",
                "popup placement needs Vibeke's TUI popup layer, which is not built yet",
            ));
        }
        other => return Err(invalid(format!("unknown placement `{other}`"))),
    }
    let transient = matches!(placement.as_str(), "zoomed" | "overlay");
    let focus = p.get("focus").and_then(Value::as_bool).unwrap_or(transient);
    let target = pane_target(caller, sn, p).ok();
    let cwd = sp(p, "cwd")
        .map(str::to_string)
        .unwrap_or_else(|| entry.root.to_string_lossy().into_owned());
    // Broker bound to the grant for the pane's lifetime.
    let digest = entry
        .trust
        .as_ref()
        .map(|g| g.manifest_sha256.clone())
        .unwrap_or_default();
    let broker = brokers::new_path(server).map_err(internal)?;
    brokers::bind(
        server,
        brokers::Binding {
            path: broker.clone(),
            plugin_id: entry.id.clone(),
            digest,
            default_pane: None,
            entrypoint: Some(decl.id.clone()),
            source: format!("pane:{placement}"),
            log_id: None,
            life: brokers::Life::Pending,
            created_at_ms: vk_store::now_ms(),
            stdout: None,
            stderr: None,
        },
    )
    .map_err(internal)?;
    let dirs = plugin_dirs();
    let inv = launch::Invocation {
        plugin_id: entry.id.clone(),
        root: entry.root.clone(),
        config_dir: dirs.config_dir(&entry.id),
        state_dir: dirs.state_dir(&entry.id),
        socket_path: broker.clone(),
        bin_path: launcher(server),
        context: json!({
            "source": "pane",
            "plugin_id": entry.id,
            "entrypoint_id": decl.id,
            "placement": placement,
            "plugin_version": m.version,
        }),
        entrypoint_id: Some(decl.id.clone()),
        ..Default::default()
    };
    let _ = std::fs::create_dir_all(&inv.config_dir);
    let _ = std::fs::create_dir_all(&inv.state_dir);
    let launcher_dir = inv
        .bin_path
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut argv = vec![
        "/bin/sh".to_string(),
        "-c".into(),
        PANE_WRAP.into(),
        "herdr-plugin-pane".into(),
        launcher_dir,
        "/usr/bin/env".into(),
    ];
    argv.extend(
        launch::runtime_env(&inv, std::iter::empty())
            .into_iter()
            .filter(|(k, _)| k != "PATH")
            .map(|(k, v)| format!("{k}={v}")),
    );
    // Pane environments drop VIBEKE_*; the launcher needs the same directory overrides as the
    // server to recognize its broker and the per-user registry.
    for k in ["VIBEKE_RUNTIME_DIR", "VIBEKE_STATE_DIR", "VIBEKE_CONFIG"] {
        if let Ok(v) = std::env::var(k) {
            argv.push(format!("{k}={v}"));
        }
    }
    argv.extend(launch::resolve_argv(&entry.root, &decl.command));
    let prev_focus = sn.focus.pane.clone();
    let result = match placement.as_str() {
        "tab" => {
            let ws = match sp(p, "workspace_id") {
                Some(_) => ws_target(sn, p)?.id,
                None => target
                    .as_deref()
                    .and_then(|t| sn.pane(t))
                    .map(|x| x.workspace.clone())
                    .or_else(|| sn.focus.workspace.clone())
                    .ok_or_else(|| WireError::new("workspace_not_found", "no workspace"))?,
            };
            native(
                server,
                caller,
                "tab.create",
                json!({"workspace": ws, "cwd": cwd, "command": argv, "title": decl.title, "focus": focus}),
            )
            .await
            .map(|r| r["root_pane"]["id"].as_str().unwrap_or_default().to_string())
        }
        _ => {
            let t = target.ok_or_else(|| invalid("pane_id is required (no focused pane)"))?;
            let r = native(
                server,
                caller,
                "pane.split",
                json!({"pane": t, "direction": sp(p, "direction").unwrap_or("right"), "cwd": cwd, "command": argv, "title": decl.title, "focus": focus}),
            )
            .await
            .map(|r| r["pane"]["id"].as_str().unwrap_or_default().to_string());
            if let Ok(id) = &r
                && transient
            {
                native(
                    server,
                    caller,
                    "pane.zoom",
                    json!({"pane": id, "zoomed": true}),
                )
                .await?;
            }
            r
        }
    };
    let new_id = match result {
        Ok(id) if !id.is_empty() => id,
        Ok(_) => {
            brokers::close(server, &broker);
            return Err(internal("the pane was not created"));
        }
        Err(e) => {
            brokers::close(server, &broker);
            return Err(e);
        }
    };
    let sn = snap(server);
    let handle = sn.pane(&new_id).map(|x| x.handle.clone());
    brokers::set_life(
        server,
        &broker,
        brokers::Life::Pane {
            pane: new_id.clone(),
        },
    );
    brokers::set_default_pane(server, &broker, handle.clone());
    state(server).meta.lock().unwrap().plugin_panes.insert(
        new_id.clone(),
        PluginPane {
            plugin: entry.id.clone(),
            entrypoint: decl.id.clone(),
            placement: placement.clone(),
            prev_focus,
        },
    );
    super::audit(
        server,
        "plugin.pane_opened",
        json!({"plugin": entry.id, "pane": new_id}),
        json!({"kind": "plugin", "id": entry.id}),
        json!({"entrypoint_id": decl.id, "placement": placement}),
    );
    Ok(typed(
        "plugin_pane_opened",
        json!({
            "plugin_id": entry.id,
            "entrypoint_id": decl.id,
            "placement": placement,
            "pane": sn.pane(&new_id).map(|x| sn.pane_json(server, x)),
        }),
    ))
}

/// The most recently opened live pane of `(plugin, entrypoint)`.
fn find_plugin_pane(server: &Server, sn: &Snap, p: &Value) -> Result<String, WireError> {
    if let Some(id) = sp(p, "pane_id") {
        let x = sn
            .pane(id)
            .ok_or_else(|| WireError::new("pane_not_found", id.to_string()))?;
        return state(server)
            .meta
            .lock()
            .unwrap()
            .plugin_panes
            .contains_key(&x.id)
            .then(|| x.id.clone())
            .ok_or_else(|| invalid(format!("{id} is not a plugin pane")));
    }
    let plugin = sp(p, "plugin_id")
        .or(sp(p, "plugin"))
        .ok_or_else(|| invalid("plugin_id is required"))?;
    let ep = sp(p, "pane").or(sp(p, "entrypoint"));
    let st = state(server);
    let meta = st.meta.lock().unwrap();
    let mut found: Vec<(&String, String)> = meta
        .plugin_panes
        .iter()
        .filter(|(_, pp)| pp.plugin == plugin && ep.is_none_or(|e| pp.entrypoint == e))
        .filter_map(|(id, _)| sn.pane(id).map(|x| (id, x.handle.clone())))
        .collect();
    found.sort_by(|a, b| a.0.cmp(b.0));
    found
        .pop()
        .map(|(id, _)| id.clone())
        .ok_or_else(|| WireError::new("plugin_pane_not_found", format!("no open pane of {plugin}")))
}

/// A plugin pane's broker closed because the pane is gone or its process exited: forget it;
/// a transient (overlay/zoomed) pane is closed and the prior focus restored (07 §7.7).
pub fn plugin_pane_gone(server: &Arc<Server>, pane: &str) {
    let pp = state(server).meta.lock().unwrap().plugin_panes.remove(pane);
    let Some(pp) = pp else { return };
    if matches!(pp.placement.as_str(), "overlay" | "zoomed") {
        if server.with_core(|c| c.pane(pane).is_some()) {
            server.close_pane(pane);
        }
        if let (Some(prev), Some(client)) = (pp.prev_focus, crate::notify::recent_client(server))
            && server.with_core(|c| c.pane(&prev).is_some())
        {
            server.focus_pane(&client, &prev);
        }
    }
}

// ---- dispatch ---------------------------------------------------------------------------------

fn agent_info(server: &Server, run_id: &str) -> Value {
    let sn = snap(server);
    let r = sn.pane_run.values().find(|r| r.id == run_id);
    json!({
        "agent": r.map(|r| sn.agent_json(server, r)),
        "agent_status": r.map(|r| sn.run_status(r)),
    })
}

pub async fn call(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    method: &str,
    p: &Value,
) -> Result<Value, WireError> {
    match method {
        // ---- layouts ---------------------------------------------------------------------
        "layout.export" => {
            if sp(p, "workspace_id").is_some() && sp(p, "tab_id").is_none() {
                let w = ws_target(sn, p)?;
                let layouts: Vec<Value> = sn
                    .tabs
                    .iter()
                    .filter(|t| t.workspace == w.id)
                    .map(|t| tab_snapshot(sn, t))
                    .collect();
                return Ok(typed(
                    "layout_snapshot",
                    json!({"workspace_id": w.handle, "layouts": layouts}),
                ));
            }
            let t = layout_tab(caller, sn, p)?;
            Ok(typed(
                "layout_snapshot",
                json!({"layout": tab_snapshot(sn, &t)}),
            ))
        }
        "layout.apply" => {
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("layouts cannot be applied from a pane"));
            }
            let layout = p
                .get("layout")
                .ok_or_else(|| invalid("layout is required"))?;
            let p2 = match (sp(p, "tab_id"), layout.get("tab_id")) {
                (None, Some(t)) => json!({"tab_id": t}),
                _ => p.clone(),
            };
            let t = layout_tab(caller, sn, &p2)?;
            let root = layout.get("root").unwrap_or(layout);
            let tab_panes = t.layout.panes();
            let resolve = |h: &str| {
                sn.pane(h)
                    .filter(|x| tab_panes.contains(&x.id))
                    .map(|x| x.id.clone())
            };
            let mut seen = Vec::new();
            let node = parse_node(root, &resolve, &mut seen)?;
            if seen.len() != tab_panes.len() {
                return Err(invalid(
                    "the layout must place every pane of the tab exactly once",
                ));
            }
            let t = commit_layout(server, &t, |x| {
                x.layout = node;
                x.zoomed_pane = None;
            })?;
            let sn = snap(server);
            Ok(typed(
                "layout_snapshot",
                json!({"layout": tab_snapshot(&sn, &t)}),
            ))
        }
        "layout.set_split_ratio" => {
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("split ratios cannot be changed from a pane"));
            }
            let ratio = p
                .get("ratio")
                .and_then(Value::as_f64)
                .ok_or_else(|| invalid("ratio is required"))? as f32;
            if !(0.0..=1.0).contains(&ratio) {
                return Err(invalid("ratio must be between 0 and 1"));
            }
            let t = layout_tab(caller, sn, p)?;
            let path = match sp(p, "split_id") {
                Some(s) => s.to_string(),
                None => {
                    let pane = pane_target(caller, sn, p)?;
                    split_of_leaf(&t.layout, &pane, "s")
                        .ok_or_else(|| invalid("the pane is not part of a split"))?
                }
            };
            let digits = path
                .strip_prefix('s')
                .ok_or_else(|| invalid("split_id must start with `s`"))?
                .as_bytes()
                .to_vec();
            let mut layout = t.layout.clone();
            if !set_ratio(&mut layout, &digits, ratio) {
                return Err(WireError::new(
                    "split_not_found",
                    format!("no split {path} in {}", t.handle),
                ));
            }
            let t = commit_layout(server, &t, |x| x.layout = layout)?;
            let sn = snap(server);
            Ok(typed(
                "layout_snapshot",
                json!({"layout": tab_snapshot(&sn, &t)}),
            ))
        }
        // ---- panes -----------------------------------------------------------------------
        "pane.process_info" => {
            let id = pane_target(caller, sn, p)?;
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
            let fg = &x.fg_cmdline;
            Ok(typed(
                "pane_process_info",
                json!({
                    "pane_id": x.handle,
                    "pid": x.child_pid,
                    "exited": x.exited,
                    "exit_code": x.exit_code,
                    "foreground": {
                        "command": fg.first().map(|a| a.rsplit('/').next().unwrap_or(a).trim_start_matches('-').to_string()),
                        "argv": fg,
                        "cwd": server.pane_cwd(&x.id),
                    },
                }),
            ))
        }
        "pane.move" => {
            let id = pane_target(caller, sn, p)?;
            let x = sn
                .pane(&id)
                .cloned()
                .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
            if !owns(caller, &x) {
                return Err(denied("the pane is not yours"));
            }
            let anchor = match sp(p, "target_pane_id") {
                Some(t) => Some(
                    sn.pane(t)
                        .cloned()
                        .ok_or_else(|| WireError::new("pane_not_found", t.to_string()))?,
                ),
                None => None,
            };
            let dest = match (&anchor, sp(p, "tab_id")) {
                (Some(a), _) => a.tab.clone(),
                (None, Some(_)) => tab_target(sn, p)?.id,
                (None, None) => {
                    return Err(invalid("tab_id or target_pane_id is required"));
                }
            };
            let dir = Direction::parse(sp(p, "direction").unwrap_or("right"))
                .ok_or_else(|| invalid("direction must be right|down|left|up"))?;
            move_pane(
                server,
                &x.id,
                &dest,
                anchor.as_ref().map(|a| a.id.as_str()),
                dir,
            )?;
            if p.get("focus").and_then(Value::as_bool) == Some(true)
                && caller.ctx.pane_scope.is_none()
            {
                native(server, caller, "pane.focus", json!({"pane": x.id})).await?;
            }
            let sn = snap(server);
            Ok(typed(
                "pane_info",
                json!({"pane": sn.pane(&x.id).map(|y| sn.pane_json(server, y))}),
            ))
        }
        "pane.swap" => {
            let id = pane_target(caller, sn, p)?;
            let other = sp(p, "other_pane_id")
                .or(sp(p, "target_pane_id"))
                .ok_or_else(|| invalid("other_pane_id is required"))?;
            let a = sn
                .pane(&id)
                .cloned()
                .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
            let b = sn
                .pane(other)
                .cloned()
                .ok_or_else(|| WireError::new("pane_not_found", other.to_string()))?;
            if !owns(caller, &a) || !owns(caller, &b) {
                return Err(denied("both panes must be yours"));
            }
            swap_panes(server, &a.id, &b.id)?;
            let sn = snap(server);
            let panes: Vec<Value> = [&a.id, &b.id]
                .iter()
                .filter_map(|i| sn.pane(i))
                .map(|y| sn.pane_json(server, y))
                .collect();
            Ok(typed("pane_list", json!({"panes": panes})))
        }
        "pane.report_metadata" => {
            let id = pane_target(caller, sn, p)?;
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
            if !owns(caller, x) {
                return Err(denied("the pane is not yours"));
            }
            let st = state(server);
            let mut meta = st.meta.lock().unwrap();
            merge_meta(meta.panes.entry(x.id.clone()).or_default(), p, &["pane_id"]);
            Ok(ok())
        }
        "workspace.report_metadata" => {
            let w = match sp(p, "workspace_id") {
                Some(_) => ws_target(sn, p)?,
                None => {
                    let pane = pane_target(caller, sn, p)?;
                    sn.pane(&pane)
                        .and_then(|x| sn.ws(&x.workspace))
                        .cloned()
                        .ok_or_else(|| WireError::new("workspace_not_found", "no workspace"))?
                }
            };
            if let Some(scope) = &caller.ctx.pane_scope
                && sn.pane(scope).is_none_or(|x| x.workspace != w.id)
            {
                return Err(denied("the workspace is not yours"));
            }
            let st = state(server);
            let mut meta = st.meta.lock().unwrap();
            merge_meta(
                meta.workspaces.entry(w.id.clone()).or_default(),
                p,
                &["workspace_id", "pane_id"],
            );
            Ok(ok())
        }
        "client.window_title.set" | "client.window_title.clear" => {
            let title = if method.ends_with(".set") {
                Some(req_s(p, "title")?.to_string())
            } else {
                None
            };
            state(server).meta.lock().unwrap().window_title = title.clone();
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.event(
                "client.window_title_changed",
                json!({}),
                json!({"title": title}),
            );
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(ok())
        }
        // ---- agents ----------------------------------------------------------------------
        "agent.start" => {
            let agent = sp(p, "agent")
                .or(sp(p, "harness"))
                .ok_or_else(|| invalid("agent is required"))?;
            let harness = status::vibeke_harness(agent);
            let mut np = json!({"harness": harness});
            for (k, nk) in [("name", "name"), ("prompt", "prompt"), ("cwd", "cwd")] {
                if let Some(v) = sp(p, k) {
                    np[nk] = json!(v);
                }
            }
            if let Some(a) = p.get("args") {
                np["args"] = a.clone();
            }
            let r = if sp(p, "pane_id").is_some() {
                np["pane"] = json!(pane_target(caller, sn, p)?);
                native(server, caller, "agent.start", np).await?
            } else {
                np["split_of"] = json!(pane_target(caller, sn, p)?);
                np["direction"] = json!(sp(p, "direction").unwrap_or("right"));
                np["focus"] = json!(p.get("focus").and_then(Value::as_bool).unwrap_or(false));
                native(server, caller, "agent.spawn", np).await?
            };
            let run = r["run"]["id"].as_str().unwrap_or_default().to_string();
            Ok(typed("agent_started", agent_info(server, &run)))
        }
        "agent.prompt" => {
            let r = run_target(sn, p)?;
            let mut np = json!({"target": r.id, "text": req_s(p, "text")?});
            for k in ["wait", "timeout_ms"] {
                if let Some(v) = p.get(k) {
                    np[k] = v.clone();
                }
            }
            native(server, caller, "agent.prompt", np).await?;
            Ok(typed("agent_info", agent_info(server, &r.id)))
        }
        "agent.wait" => {
            let r = run_target(sn, p)?;
            let wanted: Vec<String> = match p.get("status").or(p.get("until")) {
                Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => ["idle", "done", "blocked", "exited"]
                    .map(String::from)
                    .to_vec(),
            };
            let mut until = Vec::new();
            for w in &wanted {
                let c = status::wait_conditions(w);
                if c.is_empty() {
                    return Err(invalid(format!("unknown agent status `{w}`")));
                }
                until.extend(c.iter().map(|s| s.to_string()));
            }
            let mut np = json!({"target": r.id, "until": until});
            if let Some(t) = p.get("timeout_ms") {
                np["timeout_ms"] = t.clone();
            }
            native(server, caller, "agent.wait", np).await?;
            Ok(typed("agent_info", agent_info(server, &r.id)))
        }
        "agent.rename" => {
            let r = run_target(sn, p)?;
            native(
                server,
                caller,
                "agent.rename",
                json!({"target": r.id, "name": sp(p, "name").or(sp(p, "label")).unwrap_or("")}),
            )
            .await?;
            Ok(typed("agent_info", agent_info(server, &r.id)))
        }
        // ---- worktrees -------------------------------------------------------------------
        "worktree.create" => {
            let mut np = json!({
                "cwd": req_s(p, "cwd")?,
                "branch": req_s(p, "branch")?,
                "open": p.get("open").and_then(Value::as_bool).unwrap_or(true),
                "focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false),
            });
            for k in ["base", "label"] {
                if let Some(v) = sp(p, k) {
                    np[if k == "label" { "name" } else { k }] = json!(v);
                }
            }
            let r = native(server, caller, "worktree.create", np).await?;
            let sn = snap(server);
            let id = |k: &str| r[k]["id"].as_str().unwrap_or_default().to_string();
            Ok(typed(
                "worktree_created",
                json!({
                    "worktree": r["worktree"],
                    "workspace": sn.ws(&id("workspace")).map(|w| sn.ws_json(w)),
                    "tab": sn.tab(&id("tab")).map(|t| sn.tab_json(t)),
                    "root_pane": sn.pane(&id("root_pane")).map(|x| sn.pane_json(server, x)),
                }),
            ))
        }
        "worktree.open" => {
            let r = native(
                server,
                caller,
                "worktree.open",
                json!({"path": req_s(p, "path")?, "focus": p.get("focus").and_then(Value::as_bool).unwrap_or(false)}),
            )
            .await?;
            let sn = snap(server);
            let ws = r["workspace"]["id"].as_str().unwrap_or_default();
            Ok(typed(
                "worktree_opened",
                json!({
                    "worktree": r["worktree"],
                    "workspace": sn.ws(ws).map(|w| sn.ws_json(w)),
                    "created": r["created"],
                }),
            ))
        }
        // ---- plugin registry ---------------------------------------------------------------
        "plugin.link" => {
            let path = req_s(p, "path")?.to_string();
            registry_change(caller, |reg| {
                reg.link(std::path::Path::new(&path)).map(|(e, _)| e.id)
            })
        }
        "plugin.unlink" => {
            let id = sp(p, "plugin_id")
                .or(sp(p, "plugin"))
                .ok_or_else(|| invalid("plugin_id is required"))?
                .to_string();
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("plugin registrations cannot be changed from a pane"));
            }
            let dirs = plugin_dirs();
            let mut reg = Registry::load(&dirs).map_err(reg_err)?;
            let e = reg.unlink(&id).map_err(reg_err)?;
            reg.save(&dirs).map_err(reg_err)?;
            Ok(typed(
                "plugin_info",
                json!({"plugin": {"plugin_id": e.id, "root": e.root, "status": "unlinked"}}),
            ))
        }
        "plugin.enable" | "plugin.disable" => {
            let id = sp(p, "plugin_id")
                .or(sp(p, "plugin"))
                .ok_or_else(|| invalid("plugin_id is required"))?
                .to_string();
            let on = method == "plugin.enable";
            registry_change(caller, |reg| reg.set_enabled(&id, on).map(|e| e.id))
        }
        // ---- plugin panes ----------------------------------------------------------------
        "plugin.pane.open" => plugin_pane_open(server, caller, sn, p).await,
        "plugin.pane.focus" => {
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("agents can't move the user's focus"));
            }
            let id = find_plugin_pane(server, sn, p)?;
            native(server, caller, "pane.focus", json!({"pane": id})).await?;
            let sn = snap(server);
            Ok(typed(
                "pane_info",
                json!({"pane": sn.pane(&id).map(|x| sn.pane_json(server, x))}),
            ))
        }
        "plugin.pane.close" => {
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("plugin panes cannot be closed from a pane"));
            }
            let id = find_plugin_pane(server, sn, p)?;
            native(server, caller, "pane.close", json!({"pane": id})).await?;
            Ok(ok())
        }
        other => Err(WireError::new(
            "method_not_found",
            format!("unknown method: {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(p: &str) -> LayoutNode {
        LayoutNode::Leaf { pane: p.into() }
    }

    fn ratios(n: &LayoutNode) -> Vec<f32> {
        match n {
            LayoutNode::Split { children, .. } => children.iter().map(|(_, r)| *r).collect(),
            _ => vec![],
        }
    }

    #[test]
    fn split_ratio_paths_follow_the_binary_projection() {
        // [a | b | c] with equal shares: s = a vs (b|c), s1 = b vs c.
        let mut n = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            children: vec![
                (leaf("a"), 1.0 / 3.0),
                (leaf("b"), 1.0 / 3.0),
                (leaf("c"), 1.0 / 3.0),
            ],
        };
        assert!(set_ratio(&mut n, b"", 0.5));
        let r = ratios(&n);
        assert!(
            (r[0] - 0.5).abs() < 1e-4 && (r[1] - 0.25).abs() < 1e-4,
            "{r:?}"
        );
        assert!(set_ratio(&mut n, b"1", 0.8));
        let r = ratios(&n);
        assert!(
            (r[1] - 0.4).abs() < 1e-4 && (r[2] - 0.1).abs() < 1e-4,
            "{r:?}"
        );
        assert!(!set_ratio(&mut n, b"11", 0.5), "c is a leaf");
        assert!(!set_ratio(&mut n, b"0", 0.5), "a is a leaf");
        assert_eq!(split_of_leaf(&n, "a", "s").as_deref(), Some("s"));
        assert_eq!(split_of_leaf(&n, "b", "s").as_deref(), Some("s1"));
        assert_eq!(split_of_leaf(&n, "c", "s").as_deref(), Some("s1"));
        // Nested: [a | (b / c)].
        let mut m = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            children: vec![
                (leaf("a"), 0.5),
                (
                    LayoutNode::Split {
                        dir: SplitDir::Vertical,
                        children: vec![(leaf("b"), 0.5), (leaf("c"), 0.5)],
                    },
                    0.5,
                ),
            ],
        };
        assert_eq!(split_of_leaf(&m, "b", "s").as_deref(), Some("s1"));
        assert!(set_ratio(&mut m, b"1", 0.7));
        let LayoutNode::Split { children, .. } = &m else {
            unreachable!()
        };
        assert!((ratios(&children[1].0)[0] - 0.7).abs() < 1e-4);
        assert!(!set_ratio(&mut leaf("x"), b"", 0.5));
    }

    #[test]
    fn metadata_merge() {
        let mut m = Map::new();
        merge_meta(&mut m, &json!({"metadata": {"ci": "green", "n": 1}}), &[]);
        merge_meta(&mut m, &json!({"key": "n", "value": null}), &[]);
        assert_eq!(Value::Object(m.clone()), json!({"ci": "green"}));
        merge_meta(
            &mut m,
            &json!({"pane_id": "w1:p1", "branch": "x"}),
            &["pane_id"],
        );
        assert_eq!(
            Value::Object(m.clone()),
            json!({"ci": "green", "branch": "x"})
        );
        merge_meta(&mut m, &json!({"clear": true, "metadata": {}}), &[]);
        assert!(m.is_empty());
    }
}
