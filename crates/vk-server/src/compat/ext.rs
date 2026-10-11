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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use vk_compat::herdr::registry::{self, Registry, RegistryError, Status};
use vk_compat::herdr::{self, launch, status};
use vk_proto::layout::{self, Direction};
use vk_proto::model::{FloatingPane, LayoutNode, Pane, PluginSurface, SplitDir, SurfaceKind, Tab};

/// Methods handled here.
pub const METHODS: &[&str] = &[
    "layout.export",
    "layout.apply",
    "layout.set_split_ratio",
    "pane.layout",
    "pane.process_info",
    "pane.move",
    "pane.swap",
    "pane.report_metadata",
    "workspace.report_metadata",
    "client.window_title.set",
    "client.window_title.clear",
    "agent.view.set",
    "agent.view.clear",
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
    "popup.close",
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
    /// Popup panes ever opened (Vibeke ids): hidden from every compat projection, including
    /// the `pane.closed` that follows their removal from the model (07 §7.7).
    pub popups: HashSet<String>,
    /// Last scroll offset a client reported per pane (`pane.scroll_changed`, `scroll`).
    pub scroll: HashMap<String, u32>,
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
//
// Herdr's layout is a binary tree; Vibeke splits are n-ary. Both projections below nest an
// n-ary split as binary splits (`first` = one child, `second` = the rest), each with the share
// of its first side. A split's path is the list of turns from the root (`false` = first,
// `true` = second), as Herdr's `layout.set_split_ratio` takes it.

/// Herdr's `SplitDirection`: `right` splits side by side, `down` stacks.
fn dir_name(d: SplitDir) -> &'static str {
    match d {
        SplitDir::Horizontal => "right",
        SplitDir::Vertical => "down",
    }
}

/// A ratio as Herdr serializes its `f32` (shortest decimal form, `0.3` rather than
/// `0.30000001192092896`).
fn ratio_json(r: f32) -> Value {
    r.to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or_else(|| json!(0.5))
}

/// The share of the first child among `children` (the first side of the binary split).
fn first_share(children: &[(LayoutNode, f32)]) -> f32 {
    let total: f32 = children.iter().map(|(_, r)| r).sum();
    if total > 0.0 {
        children[0].1 / total
    } else {
        0.5
    }
}

/// A `LayoutDescription` node (`layout.export`, `layout.apply`, `layout.set_split_ratio`):
/// `{type: "pane", pane_id, label?, cwd?}` or `{type: "split", direction, ratio, first,
/// second}`. Vibeke does not keep a pane's launch argv, so `command` is never set.
fn node_json(server: &Server, sn: &Snap, node: &LayoutNode) -> Value {
    match node {
        LayoutNode::Leaf { pane } => {
            let mut v = json!({"type": "pane"});
            if let Some(p) = sn.pane(pane) {
                v["pane_id"] = json!(p.handle);
                if let Some(t) = &p.title {
                    v["label"] = json!(t);
                }
                if let Some(c) = server.pane_cwd(&p.id).or_else(|| p.cwd.clone()) {
                    v["cwd"] = json!(c);
                }
            }
            v
        }
        LayoutNode::Split { dir, children } => binary(server, sn, *dir, children),
    }
}

fn binary(server: &Server, sn: &Snap, dir: SplitDir, children: &[(LayoutNode, f32)]) -> Value {
    match children.len() {
        0 => json!({"type": "pane"}),
        1 => node_json(server, sn, &children[0].0),
        _ => json!({
            "type": "split",
            "direction": dir_name(dir),
            "ratio": ratio_json(first_share(children)),
            "first": node_json(server, sn, &children[0].0),
            "second": binary(server, sn, dir, &children[1..]),
        }),
    }
}

/// The tab's focused pane (Vibeke id): its own focus, else its first pane.
fn tab_focus(t: &Tab) -> Option<String> {
    t.focused_pane
        .clone()
        .or_else(|| t.layout.panes().first().cloned())
}

/// Herdr's `LayoutDescription` of one tab.
fn layout_description(server: &Server, sn: &Snap, t: &Tab) -> Value {
    json!({
        "workspace_id": sn.ws_handle(&t.workspace),
        "tab_id": t.handle,
        "zoomed": t.zoomed_pane.is_some(),
        "focused_pane_id": tab_focus(t).and_then(|p| sn.pane(&p)).map(|p| p.handle.clone()),
        "root": node_json(server, sn, &t.layout),
    })
}

/// A rectangle in cells (Herdr's `PaneLayoutRect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl Area {
    fn json(self) -> Value {
        json!({"x": self.x, "y": self.y, "width": self.width, "height": self.height})
    }
}

/// Herdr's `split_rect`: the first side gets `round(size * ratio)` cells, the second the rest
/// (a pane's rect includes its border; there is no separate divider cell).
fn split_area(a: Area, dir: SplitDir, ratio: f32) -> (Area, Area) {
    match dir {
        SplitDir::Horizontal => {
            let w = ((a.width as f32) * ratio).round() as u16;
            let w = w.min(a.width);
            (
                Area { width: w, ..a },
                Area {
                    x: a.x + w,
                    width: a.width - w,
                    ..a
                },
            )
        }
        SplitDir::Vertical => {
            let h = ((a.height as f32) * ratio).round() as u16;
            let h = h.min(a.height);
            (
                Area { height: h, ..a },
                Area {
                    y: a.y + h,
                    height: a.height - h,
                    ..a
                },
            )
        }
    }
}

/// Pane and split rectangles of a layout within `area`, in Herdr's pre-order.
#[derive(Debug, Default)]
struct Geometry {
    /// `(Vibeke pane id, rect)`.
    panes: Vec<(String, Area)>,
    /// `(path, direction, ratio of the first side, rect of the whole split)`.
    splits: Vec<(Vec<bool>, SplitDir, f32, Area)>,
}

fn geometry(node: &LayoutNode, area: Area) -> Geometry {
    fn walk(node: &LayoutNode, a: Area, path: &mut Vec<bool>, g: &mut Geometry) {
        match node {
            LayoutNode::Leaf { pane } => g.panes.push((pane.clone(), a)),
            LayoutNode::Split { dir, children } => walk_children(*dir, children, a, path, g),
        }
    }
    fn walk_children(
        dir: SplitDir,
        children: &[(LayoutNode, f32)],
        a: Area,
        path: &mut Vec<bool>,
        g: &mut Geometry,
    ) {
        match children.len() {
            0 => {}
            1 => walk(&children[0].0, a, path, g),
            _ => {
                let ratio = first_share(children);
                g.splits.push((path.clone(), dir, ratio, a));
                let (first, second) = split_area(a, dir, ratio);
                path.push(false);
                walk(&children[0].0, first, path, g);
                path.pop();
                path.push(true);
                walk_children(dir, &children[1..], second, path, g);
                path.pop();
            }
        }
    }
    let mut g = Geometry::default();
    walk(node, area, &mut Vec::new(), &mut g);
    g
}

/// Herdr's split id: `split_<pre-order index>_<path as 0/1>` (`split_0_root` for the root).
fn split_id(idx: usize, path: &[bool]) -> String {
    if path.is_empty() {
        return format!("split_{idx}_root");
    }
    let p: String = path.iter().map(|r| if *r { '1' } else { '0' }).collect();
    format!("split_{idx}_{p}")
}

/// Area of one pane when its size is unknown (no client has sized it).
const NOMINAL_PANE: (u16, u16) = (80, 24);

/// The cell area of a tab as the sizing client lays it out: the panes' current sizes plus the
/// one-cell borders Vibeke draws between split children (`vk_proto::layout::rects`). A zoomed
/// tab's area is the zoomed pane's size. Herdr reports its own terminal area; Vibeke has no
/// single client area on the server, so it reconstructs it from the pane sizes (origin 0, 0).
fn tab_area(sn: &Snap, t: &Tab) -> Area {
    fn size(sn: &Snap, pane: &str) -> (u16, u16) {
        sn.pane(pane)
            .map(|p| (p.cols, p.rows))
            .filter(|(c, r)| *c > 0 && *r > 0)
            .unwrap_or(NOMINAL_PANE)
    }
    fn node(sn: &Snap, n: &LayoutNode) -> (u16, u16) {
        match n {
            LayoutNode::Leaf { pane } => size(sn, pane),
            LayoutNode::Split { dir, children } => {
                let sizes: Vec<(u16, u16)> = children.iter().map(|(c, _)| node(sn, c)).collect();
                let gaps = sizes.len().saturating_sub(1) as u16;
                match dir {
                    SplitDir::Horizontal => (
                        sizes
                            .iter()
                            .fold(gaps, |acc, (w, _)| acc.saturating_add(*w)),
                        sizes.iter().map(|(_, h)| *h).max().unwrap_or(0),
                    ),
                    SplitDir::Vertical => (
                        sizes.iter().map(|(w, _)| *w).max().unwrap_or(0),
                        sizes
                            .iter()
                            .fold(gaps, |acc, (_, h)| acc.saturating_add(*h)),
                    ),
                }
            }
        }
    }
    let (width, height) = match &t.zoomed_pane {
        Some(z) => size(sn, z),
        None => node(sn, &t.layout),
    };
    Area {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Herdr's `PaneLayoutSnapshot` of one tab (`pane.layout`, `layout.updated`,
/// `session.snapshot` layouts, the layouts in `pane.move`/`pane.swap` results).
pub fn tab_snapshot(sn: &Snap, t: &Tab) -> Value {
    let area = tab_area(sn, t);
    let g = geometry(&t.layout, area);
    let focus = tab_focus(t);
    let panes: Vec<Value> = g
        .panes
        .iter()
        .filter_map(|(id, r)| {
            sn.pane(id).map(|p| {
                json!({
                    "pane_id": p.handle,
                    "focused": focus.as_deref() == Some(id.as_str()),
                    "rect": r.json(),
                })
            })
        })
        .collect();
    let splits: Vec<Value> = g
        .splits
        .iter()
        .enumerate()
        .map(|(i, (path, dir, ratio, r))| {
            json!({
                "id": split_id(i, path),
                "direction": dir_name(*dir),
                "ratio": ratio_json(*ratio),
                "rect": r.json(),
            })
        })
        .collect();
    json!({
        "workspace_id": sn.ws_handle(&t.workspace),
        "tab_id": t.handle,
        "zoomed": t.zoomed_pane.is_some(),
        "area": area.json(),
        "focused_pane_id": focus.as_deref().and_then(|p| sn.pane(p)).map(|p| p.handle.clone()),
        "panes": panes,
        "splits": splits,
    })
}

/// Parse a split path given as Herdr's `split_<n>_<0/1…>` id (from `pane.layout`), Vibeke's
/// older `s<0/1…>` form, or the bare digits.
fn split_path_digits(id: &str) -> Option<Vec<u8>> {
    let digits = if let Some(rest) = id.strip_prefix("split_") {
        let (_, path) = rest.split_once('_')?;
        if path == "root" { "" } else { path }
    } else {
        id.strip_prefix('s').unwrap_or(id)
    };
    digits
        .bytes()
        .all(|b| b == b'0' || b == b'1')
        .then(|| digits.as_bytes().to_vec())
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
/// destination's focused pane) in `dir`, the moved pane taking `ratio` of the split. An
/// emptied source tab closes; returns whether it did.
fn move_pane(
    server: &Server,
    pane: &str,
    dest: &str,
    anchor: Option<&str>,
    dir: Direction,
    ratio: f32,
) -> Result<bool, WireError> {
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
    if !layout::split(&mut dst.layout, &anchor, pane, dir, ratio) {
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
    Ok(src_closed)
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

/// `pane.move`. Herdr's form is `{pane_id, destination: {type: "tab", tab_id, split:
/// right|down, target_pane_id?, ratio?}, focus?}`; a move into the pane's own tab or a zoomed
/// tab is answered with `changed: false` and a `reason`. Vibeke also accepts its older flat
/// form (`tab_id` or `target_pane_id`, `direction`), which may rearrange a pane within its tab.
/// `new_tab` and `new_workspace` destinations are not supported yet. Result: `pane_move
/// {move_result: PaneMoveResult}`.
async fn pane_move(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let id = pane_target(caller, sn, p)?;
    let x = sn
        .pane(&id)
        .cloned()
        .ok_or_else(|| WireError::new("pane_not_found", "source pane not found"))?;
    if !owns(caller, &x) {
        return Err(denied("the pane is not yours"));
    }
    let src_tab = sn
        .tab(&x.tab)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", "source tab not found"))?;
    let previous = json!({
        "previous_pane_id": x.handle,
        "previous_workspace_id": sn.ws_handle(&x.workspace),
        "previous_tab_id": src_tab.handle,
    });
    let unchanged = |reason: &str, target: &Tab| {
        let mut r = previous.clone();
        let target_layout = tab_snapshot(sn, target);
        r["changed"] = json!(false);
        r["reason"] = json!(reason);
        r["pane"] = sn.pane_json(server, &x);
        r["source_layout"] = tab_snapshot(sn, &src_tab);
        r["focused_pane_id"] = target_layout["focused_pane_id"].clone();
        r["target_layout"] = target_layout;
        typed("pane_move", json!({"move_result": r}))
    };
    let (dest, anchor, dir, ratio) = match p.get("destination") {
        Some(d) => {
            match d.get("type").and_then(Value::as_str) {
                Some("tab") => {}
                Some(kind @ ("new_tab" | "new_workspace")) => {
                    return Err(WireError::new(
                        "unsupported",
                        format!("moving a pane to a {kind} destination is not supported yet"),
                    ));
                }
                _ => {
                    return Err(invalid(
                        "destination.type must be tab, new_tab or new_workspace",
                    ));
                }
            }
            let tab_id = d
                .get("tab_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("destination.tab_id is required"))?;
            let dest = sn.tab(tab_id).cloned().ok_or_else(|| {
                WireError::new("tab_not_found", format!("tab {tab_id} not found"))
            })?;
            if dest.id == src_tab.id {
                return Ok(unchanged("same_tab", &src_tab));
            }
            if src_tab.zoomed_pane.is_some() {
                return Ok(unchanged("zoomed_tab", &src_tab));
            }
            if dest.zoomed_pane.is_some() {
                return Ok(unchanged("zoomed_tab", &dest));
            }
            let dir = match d.get("split").and_then(Value::as_str) {
                Some("right") => Direction::Right,
                Some("down") => Direction::Down,
                _ => return Err(invalid("destination.split must be right or down")),
            };
            let anchor = match d.get("target_pane_id").and_then(Value::as_str) {
                Some(t) => {
                    let a = sn.pane(t).filter(|a| a.tab == dest.id).ok_or_else(|| {
                        WireError::new("pane_not_found", format!("pane {t} not found in {tab_id}"))
                    })?;
                    Some(a.id.clone())
                }
                None => None,
            };
            // Herdr's ratio is the first side's share; the moved pane is the second side.
            let ratio = d
                .get("ratio")
                .and_then(Value::as_f64)
                .map(|r| 1.0 - r as f32)
                .unwrap_or(0.5);
            (dest, anchor, dir, ratio)
        }
        None => {
            let anchor = match sp(p, "target_pane_id") {
                Some(t) => Some(
                    sn.pane(t)
                        .cloned()
                        .ok_or_else(|| WireError::new("pane_not_found", t.to_string()))?,
                ),
                None => None,
            };
            let dest = match (&anchor, sp(p, "tab_id")) {
                (Some(a), _) => sn
                    .tab(&a.tab)
                    .cloned()
                    .ok_or_else(|| WireError::new("tab_not_found", a.tab.clone()))?,
                (None, Some(_)) => tab_target(sn, p)?,
                (None, None) => {
                    return Err(invalid("destination is required"));
                }
            };
            let dir = Direction::parse(sp(p, "direction").unwrap_or("right"))
                .ok_or_else(|| invalid("direction must be right|down|left|up"))?;
            (dest, anchor.map(|a| a.id), dir, 0.5)
        }
    };
    let src_closed = move_pane(server, &x.id, &dest.id, anchor.as_deref(), dir, ratio)?;
    if p.get("focus").and_then(Value::as_bool) == Some(true) && caller.ctx.pane_scope.is_none() {
        native(server, caller, "pane.focus", json!({"pane": x.id})).await?;
    }
    let sn = snap(server);
    let pane = sn
        .pane(&x.id)
        .ok_or_else(|| WireError::new("pane_move_failed", "moved pane is unavailable"))?;
    let target = sn
        .tab(&dest.id)
        .ok_or_else(|| WireError::new("pane_layout_unavailable", "pane layout unavailable"))?;
    let target_layout = tab_snapshot(&sn, target);
    let mut r = previous;
    r["changed"] = json!(true);
    r["pane"] = sn.pane_json(server, pane);
    r["source_layout"] = match sn.tab(&src_tab.id) {
        Some(t) if !src_closed && t.id != target.id => tab_snapshot(&sn, t),
        _ => Value::Null,
    };
    if src_closed {
        r["closed_tab_id"] = json!(src_tab.handle);
    }
    r["focused_pane_id"] = target_layout["focused_pane_id"].clone();
    r["target_layout"] = target_layout;
    Ok(typed("pane_move", json!({"move_result": r})))
}

/// `pane.swap`. Herdr's forms: `{source_pane_id, target_pane_id}` and `{pane_id?, direction}`
/// (the neighbor in that direction). A swap that cannot happen (`not_found`, `same_pane`,
/// `cross_tab`, `no_neighbor`) is answered with `changed: false` and a `reason`. Vibeke's older
/// form `{pane_id, other_pane_id | target_pane_id}` also swaps across tabs of a workspace.
/// Result: `pane_swap {swap: PaneSwapResult}`.
fn pane_swap(
    server: &Arc<Server>,
    caller: &Caller,
    sn: &Snap,
    p: &Value,
) -> Result<Value, WireError> {
    let herdr_pair = sp(p, "source_pane_id").is_some();
    // (source pane, its id as reported, target pane, target id as requested, reason)
    #[allow(clippy::type_complexity)]
    let (source, source_out, target, raw_target, reason): (
        Pane,
        String,
        Option<Pane>,
        Option<String>,
        Option<&str>,
    ) = if let Some(d) = sp(p, "direction") {
        let dir =
            Direction::parse(d).ok_or_else(|| invalid("direction must be left|right|up|down"))?;
        let id = pane_target(caller, sn, p)?;
        let src = sn
            .pane(&id)
            .cloned()
            .ok_or_else(|| WireError::new("pane_not_found", "source pane not found"))?;
        let t = sn
            .tab(&src.tab)
            .ok_or_else(|| WireError::new("pane_not_found", "source pane not found"))?;
        let g = geometry(&t.layout, tab_area(sn, t));
        let rects: Vec<(String, layout::Rect)> = g
            .panes
            .iter()
            .map(|(id, a)| {
                (
                    id.clone(),
                    layout::Rect {
                        x: a.x,
                        y: a.y,
                        w: a.width,
                        h: a.height,
                    },
                )
            })
            .collect();
        match layout::neighbor(&rects, &src.id, dir).and_then(|n| sn.pane(&n).cloned()) {
            Some(n) => (src.clone(), src.handle, Some(n), None, None),
            None => (src.clone(), src.handle, None, None, Some("no_neighbor")),
        }
    } else if herdr_pair {
        let s_raw = sp(p, "source_pane_id").unwrap_or_default();
        let t_raw = sp(p, "target_pane_id")
            .ok_or_else(|| WireError::new("invalid_pane_swap", "missing target_pane_id"))?;
        let s = sn.pane(s_raw).cloned();
        let t = sn.pane(t_raw).cloned();
        let reason = match (&s, &t) {
            (None, _) | (_, None) => Some("not_found"),
            (Some(a), Some(b)) if a.id == b.id => Some("same_pane"),
            (Some(a), Some(b)) if a.tab != b.tab => Some("cross_tab"),
            _ => None,
        };
        let source_out = s
            .as_ref()
            .map(|x| x.handle.clone())
            .unwrap_or_else(|| s_raw.to_string());
        let src = match s.or_else(|| t.clone()) {
            Some(x) => x,
            None => {
                return Err(WireError::new(
                    "pane_layout_unavailable",
                    "pane layout unavailable",
                ));
            }
        };
        (src, source_out, t, Some(t_raw.to_string()), reason)
    } else {
        let id = pane_target(caller, sn, p)?;
        let other = sp(p, "other_pane_id")
            .or(sp(p, "target_pane_id"))
            .ok_or_else(|| invalid("target_pane_id or direction is required"))?;
        let a = sn
            .pane(&id)
            .cloned()
            .ok_or_else(|| WireError::new("pane_not_found", id.clone()))?;
        let b = sn
            .pane(other)
            .cloned()
            .ok_or_else(|| WireError::new("pane_not_found", other.to_string()))?;
        let reason = (a.id == b.id).then_some("same_pane");
        (a.clone(), a.handle, Some(b), None, reason)
    };
    let mut changed = false;
    if reason.is_none()
        && let Some(t) = &target
    {
        if !owns(caller, &source) || !owns(caller, t) {
            return Err(denied("both panes must be yours"));
        }
        swap_panes(server, &source.id, &t.id)?;
        changed = true;
    }
    let sn = snap(server);
    let layout_tab = sn
        .pane(&source.id)
        .and_then(|x| sn.tab(&x.tab))
        .ok_or_else(|| WireError::new("pane_layout_unavailable", "pane layout unavailable"))?;
    let layout = tab_snapshot(&sn, layout_tab);
    let mut r = json!({
        "changed": changed,
        "source_pane_id": source_out,
        "focused_pane_id": layout["focused_pane_id"],
        "layout": layout,
    });
    if let Some(reason) = reason {
        r["reason"] = json!(reason);
    }
    match (target.map(|t| t.handle), raw_target) {
        (Some(h), _) | (None, Some(h)) => r["target_pane_id"] = json!(h),
        (None, None) => {}
    }
    Ok(typed("pane_swap", json!({"swap": r})))
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

/// Apply a registry change and answer `kind` (`plugin_linked`, `plugin_enabled`,
/// `plugin_disabled`) with the plugin's `InstalledPluginInfo`.
fn registry_change(
    caller: &Caller,
    kind: &str,
    f: impl FnOnce(&mut Registry) -> Result<String, RegistryError>,
) -> Result<Value, WireError> {
    if caller.ctx.pane_scope.is_some() {
        return Err(denied(
            "plugin registrations cannot be changed from a pane; ask the user to run it",
        ));
    }
    let id = Registry::update(&plugin_dirs(), f).map_err(reg_err)?;
    Ok(typed(kind, json!({"plugin": plugin_json(&id)})))
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
    let settings = super::limits::settings(plugin);
    if let Some(why) = settings.error {
        return Err(denied(why));
    }
    if settings.isolate == herdr::settings::Isolate::Sandbox {
        return Err(WireError::new(
            "unsupported",
            format!(
                "{plugin} runs in restricted (sandboxed) mode, which supports actions, hooks and startup but not plugin panes; use isolate = \"host\" to open its panes"
            ),
        ));
    }
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
        "split" | "tab" | "zoomed" | "overlay" | "popup" => {}
        other => return Err(invalid(format!("unknown placement `{other}`"))),
    }
    let popup = placement == "popup";
    if popup && let Some(open) = open_popup(server) {
        return Err(WireError::new(
            "popup_busy",
            format!(
                "a popup is already open ({}); close it first (`popup.close`)",
                open.1.plugin
            ),
        ));
    }
    let transient = matches!(placement.as_str(), "zoomed" | "overlay" | "popup");
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
            grant_id: entry
                .trust
                .as_ref()
                .map(|g| g.grant_id.clone())
                .unwrap_or_default(),
            life: brokers::Life::Pending,
            created_at_ms: vk_store::now_ms(),
            stdout: None,
            stderr: None,
            // Plugin panes are host-only; their processes prove themselves by pane ancestry
            // (no token in a pane's command line).
            isolation: Some("host".into()),
            token: String::new(),
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
        if popup { POPUP_WRAP } else { PANE_WRAP }.into(),
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
    // The test-only trace file (spec 18 §9), which the pane environment would drop too.
    argv.extend(
        super::trace::child_env(None)
            .into_iter()
            .map(|(k, v)| format!("{k}={v}")),
    );
    argv.extend(launch::resolve_argv(&entry.root, &decl.command));
    let prev_focus = sn.focus.pane.clone();
    let result = match placement.as_str() {
        "popup" | "overlay" => {
            let tab = target
                .as_deref()
                .and_then(|t| sn.pane(t))
                .map(|x| x.tab.clone())
                .or_else(|| sn.focus.tab.clone())
                .ok_or_else(|| invalid("pane_id is required (no focused pane)"));
            let dim = |k: &str, d: &Option<Value>| -> String {
                match p.get(k).or(d.as_ref()) {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => n.to_string(),
                    _ => String::new(),
                }
            };
            let surface = PluginSurface {
                kind: if popup {
                    SurfaceKind::Popup
                } else {
                    SurfaceKind::Overlay
                },
                width: dim("width", &decl.width),
                height: dim("height", &decl.height),
                plugin: entry.id.clone(),
                entrypoint: decl.id.clone(),
            };
            match tab {
                Ok(tab) => open_surface(
                    server,
                    &tab,
                    &cwd,
                    argv,
                    &decl.title,
                    &surface,
                    focus.then_some(caller.ctx.client_id.as_str()),
                ),
                Err(e) => Err(e),
            }
        }
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
            let t = target
                .clone()
                .ok_or_else(|| invalid("pane_id is required (no focused pane)"))?;
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
    if popup {
        state(server)
            .meta
            .lock()
            .unwrap()
            .popups
            .insert(new_id.clone());
    }
    let sn = snap(server);
    // A popup has no pane identity of its own: its callbacks act on the pane underneath.
    let handle = if popup {
        target
            .as_deref()
            .or(prev_focus.as_deref())
            .and_then(|t| sn.pane(t))
            .map(|x| x.handle.clone())
    } else {
        sn.pane(&new_id).map(|x| x.handle.clone())
    };
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
    // Popups are not panes (07 §7.7): like Herdr, opening one answers `ok`, without a pane.
    // The plugin and entrypoint are Vibeke extras.
    if popup {
        return Ok(typed(
            "ok",
            json!({"plugin_id": entry.id, "entrypoint": decl.id, "placement": placement}),
        ));
    }
    let pane = sn
        .pane(&new_id)
        .map(|x| sn.pane_json(server, x))
        .ok_or_else(|| WireError::new("plugin_pane_open_failed", "plugin pane disappeared"))?;
    Ok(typed(
        "plugin_pane_opened",
        json!({"plugin_pane": plugin_pane_info(&entry.id, &decl.id, &placement, pane)}),
    ))
}

/// Herdr's `PluginPaneInfo` (`plugin_pane_opened`, `plugin_pane_focused`); `placement` is a
/// Vibeke extra.
fn plugin_pane_info(plugin: &str, entrypoint: &str, placement: &str, pane: Value) -> Value {
    json!({
        "plugin_id": plugin,
        "entrypoint": entrypoint,
        "pane": pane,
        "placement": placement,
    })
}

/// Like [`PANE_WRAP`] for a popup: no `HERDR_PANE_ID` (07 §7.7), the tab/workspace of the
/// context it opened over.
const POPUP_WRAP: &str = "PATH=\"$1:$PATH\"; unset HERDR_PANE_ID; \
HERDR_TAB_ID=\"$VIBEKE_TAB_ID\"; HERDR_WORKSPACE_ID=\"$VIBEKE_WORKSPACE_ID\"; \
export PATH HERDR_TAB_ID HERDR_WORKSPACE_ID; shift; exec \"$@\"";

/// The open popup of this session (at most one: popups are session-modal), if its pane is
/// still alive.
pub fn open_popup(server: &Server) -> Option<(String, PluginPane)> {
    let meta = state(server).meta.lock().unwrap().clone();
    meta.plugin_panes
        .into_iter()
        .filter(|(_, pp)| pp.placement == "popup")
        .find(|(id, _)| server.with_core(|c| c.pane(id).is_some_and(|p| !p.exited)))
}

/// Start a plugin popup/overlay as a floating pane of `tab`, tagged through `created_by`
/// ([`PluginSurface`]) so clients draw it as a popup or full-area overlay and the compat
/// projection hides popups. No `tab.layout_changed`: surfaces don't change the layout.
fn open_surface(
    server: &Arc<Server>,
    tab: &str,
    cwd: &str,
    argv: Vec<String>,
    title: &str,
    surface: &PluginSurface,
    focus_client: Option<&str>,
) -> Result<String, WireError> {
    let mut c = server.core.lock().unwrap();
    let mut t = c
        .tab(tab)
        .cloned()
        .ok_or_else(|| WireError::new("tab_not_found", tab.to_string()))?;
    let ws = c
        .ws(&t.workspace)
        .cloned()
        .ok_or_else(|| WireError::new("workspace_not_found", t.workspace.clone()))?;
    let mut tx = Tx::new();
    let handle = t.handle.clone();
    let pane = server
        .new_pane(
            &mut c,
            &mut tx,
            &ws,
            &t.id,
            &handle,
            cwd,
            Some(argv),
            Some(title.to_string()),
            &surface.tag(),
        )
        .map_err(internal)?;
    let pct = |spec: &str| {
        spec.strip_suffix('%')
            .and_then(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(80.0)
            .clamp(10.0, 100.0)
    };
    let (w, h) = match surface.kind {
        SurfaceKind::Overlay => (100.0, 100.0),
        SurfaceKind::Popup => (pct(&surface.width), pct(&surface.height)),
    };
    let z = t.floating.iter().map(|f| f.z).max().unwrap_or(0) + 1;
    t.floating.push(FloatingPane {
        pane: pane.id.clone(),
        x: (100.0 - w) / 2.0,
        y: (100.0 - h) / 2.0,
        w,
        h,
        z,
    });
    t.floats_hidden = false;
    tx.tab(t);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    if let Some(client) = focus_client {
        server.focus_pane(client, &pane.id);
    }
    Ok(pane.id)
}

/// Close a plugin popup/overlay/zoomed pane and give focus back to what had it before
/// (07 §7.7). Focus moves first, so the pane's removal doesn't pick another pane.
pub fn close_surface(server: &Arc<Server>, pane: &str) {
    let pp = state(server).meta.lock().unwrap().plugin_panes.remove(pane);
    let prev = pp.and_then(|pp| pp.prev_focus);
    if let (Some(prev), Some(client)) = (prev, crate::notify::recent_client(server))
        && prev != pane
        && server.with_core(|c| c.pane(&prev).is_some())
        && server.client_focus(&client).pane.as_deref() == Some(pane)
    {
        server.focus_pane(&client, &prev);
    }
    if server.with_core(|c| c.pane(pane).is_some()) {
        server.close_pane(pane);
    }
}

/// The most recently opened live pane of `(plugin, entrypoint)`.
fn find_plugin_pane(server: &Server, sn: &Snap, p: &Value) -> Result<String, WireError> {
    if let Some(id) = sp(p, "pane_id") {
        // Herdr answers `plugin_pane_not_found` for an unknown pane and for a pane that is not
        // a plugin pane alike.
        let not_found = || WireError::new("plugin_pane_not_found", "plugin pane not found");
        let x = sn.pane(id).ok_or_else(not_found)?;
        return state(server)
            .meta
            .lock()
            .unwrap()
            .plugin_panes
            .contains_key(&x.id)
            .then(|| x.id.clone())
            .ok_or_else(not_found);
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
    let placement = state(server)
        .meta
        .lock()
        .unwrap()
        .plugin_panes
        .get(pane)
        .map(|pp| pp.placement.clone());
    match placement.as_deref() {
        Some("overlay" | "zoomed" | "popup") => close_surface(server, pane),
        Some(_) => {
            state(server).meta.lock().unwrap().plugin_panes.remove(pane);
        }
        None => {}
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
                // Vibeke extension: every tab of a workspace.
                let w = ws_target(sn, p)?;
                let layouts: Vec<Value> = sn
                    .tabs
                    .iter()
                    .filter(|t| t.workspace == w.id)
                    .map(|t| layout_description(server, sn, t))
                    .collect();
                return Ok(typed(
                    "layout_export",
                    json!({"workspace_id": w.handle, "layout": layouts.first(), "layouts": layouts}),
                ));
            }
            if sp(p, "tab_id").is_some() && sp(p, "pane_id").is_some() {
                return Err(WireError::new(
                    "layout_not_found",
                    "layout target not found (use tab_id or pane_id, not both)",
                ));
            }
            let t = layout_tab(caller, sn, p)?;
            Ok(typed(
                "layout_export",
                json!({"layout": layout_description(server, sn, &t)}),
            ))
        }
        "layout.apply" => {
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("layouts cannot be applied from a pane"));
            }
            // Baseline requests carry the tree as `root` next to `tab_id`; a whole description
            // under `layout` (as `layout.export` returns it) is accepted too.
            let (root, snapshot_tab) = match (p.get("root"), p.get("layout")) {
                (Some(r), _) => (r, None),
                (None, Some(l)) => (l.get("root").unwrap_or(l), l.get("tab_id")),
                (None, None) => return Err(invalid("root is required")),
            };
            let p2 = match (sp(p, "tab_id"), snapshot_tab) {
                (None, Some(t)) => json!({"tab_id": t}),
                _ => p.clone(),
            };
            let t = layout_tab(caller, sn, &p2)?;
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
                "layout_apply",
                json!({"layout": layout_description(server, &sn, &t)}),
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
            if !ratio.is_finite() {
                return Err(WireError::new("invalid_ratio", "ratio must be finite"));
            }
            if !(0.0..=1.0).contains(&ratio) {
                return Err(invalid("ratio must be between 0 and 1"));
            }
            let t = layout_tab(caller, sn, p)?;
            // Baseline: `path` is the list of turns from the root, `false` = first child,
            // `true` = second; `[]` is the root split. Vibeke also accepts a split `id` from
            // `pane.layout` (`split_id`) or a pane whose enclosing split is meant.
            let digits: Vec<u8> = match (p.get("path"), sp(p, "split_id")) {
                (Some(Value::Array(turns)), _) => {
                    let mut s = Vec::new();
                    for t in turns {
                        match t.as_bool() {
                            Some(false) => s.push(b'0'),
                            Some(true) => s.push(b'1'),
                            None => return Err(invalid("path must be a list of booleans")),
                        }
                    }
                    s
                }
                (Some(_), _) => return Err(invalid("path must be a list of booleans")),
                (None, Some(s)) => {
                    split_path_digits(s).ok_or_else(|| invalid(format!("not a split id: {s}")))?
                }
                (None, None) => {
                    let pane = pane_target(caller, sn, p)?;
                    let s = split_of_leaf(&t.layout, &pane, "s")
                        .ok_or_else(|| invalid("the pane is not part of a split"))?;
                    split_path_digits(&s).unwrap_or_default()
                }
            };
            let mut layout = t.layout.clone();
            if !set_ratio(&mut layout, &digits, ratio) {
                return Err(WireError::new("split_not_found", "split path not found"));
            }
            let t = commit_layout(server, &t, |x| x.layout = layout)?;
            let sn = snap(server);
            Ok(typed(
                "layout_split_ratio_set",
                json!({"layout": layout_description(server, &sn, &t)}),
            ))
        }
        // ---- panes -----------------------------------------------------------------------
        "pane.layout" => {
            let id = pane_target(caller, sn, p)?;
            let t = sn
                .pane(&id)
                .and_then(|x| sn.tab(&x.tab))
                .ok_or_else(|| WireError::new("pane_not_found", "pane not found"))?;
            Ok(typed("pane_layout", json!({"layout": tab_snapshot(sn, t)})))
        }
        "pane.process_info" => {
            let id = pane_target(caller, sn, p)?;
            let x = sn
                .pane(&id)
                .ok_or_else(|| WireError::new("pane_not_found", "pane not found"))?;
            // Herdr's shape: optional fields are left out, never `null`; `tty` is never set.
            let mut info = json!({"pane_id": x.handle});
            let shell_pid = x.child_pid.filter(|pid| *pid > 0 && !x.exited);
            if let Some(pid) = shell_pid {
                info["shell_pid"] = json!(pid);
            }
            if let Some((pgid, procs)) = shell_pid.and_then(fgjob::job) {
                info["foreground_process_group_id"] = json!(pgid);
                if !procs.is_empty() {
                    info["foreground_processes"] = Value::Array(procs);
                }
            }
            Ok(typed("pane_process_info", json!({"process_info": info})))
        }
        "pane.move" => pane_move(server, caller, sn, p).await,
        "pane.swap" => pane_swap(server, caller, sn, p),
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
            let prev = std::mem::replace(
                &mut state(server).meta.lock().unwrap().window_title,
                title.clone(),
            );
            {
                let mut c = server.core.lock().unwrap();
                let mut tx = Tx::new();
                tx.event(
                    "client.window_title_changed",
                    json!({}),
                    json!({"title": title}),
                );
                server.commit(&mut c, tx).map_err(internal)?;
            }
            // Herdr's `client_window_title`: with no attached client nothing shows the title.
            // Vibeke keeps it and shows it when a client attaches.
            let (changed, reason) = if crate::notify::recent_client(server).is_some() {
                (
                    prev != title,
                    if title.is_some() { "set" } else { "cleared" },
                )
            } else {
                (false, "no_foreground_client")
            };
            Ok(typed(
                "client_window_title",
                json!({"changed": changed, "reason": reason}),
            ))
        }
        // ---- agents ----------------------------------------------------------------------
        "agent.view.set" => super::views::set(server, caller, sn, p),
        "agent.view.clear" => super::views::clear(server, caller, sn, p),
        "agent.start" => {
            // Herdr names the agent `kind`; Vibeke also accepts `agent` and `harness`.
            let agent = sp(p, "kind")
                .or(sp(p, "agent"))
                .or(sp(p, "harness"))
                .ok_or_else(|| invalid("kind is required"))?;
            let harness = status::vibeke_harness(agent);
            // Herdr reports the argv it launched; Vibeke starts the harness with `args`.
            let mut argv = vec![json!(harness)];
            if let Some(Value::Array(a)) = p.get("args") {
                argv.extend(a.iter().cloned());
            }
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
            let mut info = agent_info(server, &run);
            info["argv"] = Value::Array(argv);
            Ok(typed("agent_started", info))
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
            Ok(typed("agent_prompted", agent_info(server, &r.id)))
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
            // The workspace's active tab and that tab's root (first) pane, as Herdr reports.
            let tab = sn
                .tabs
                .iter()
                .filter(|t| sn.ws(ws).is_some_and(|w| w.id == t.workspace))
                .find(|t| sn.focus.tab.as_deref() == Some(t.id.as_str()))
                .or_else(|| {
                    sn.tabs
                        .iter()
                        .find(|t| sn.ws(ws).is_some_and(|w| w.id == t.workspace))
                });
            let root = tab.and_then(|t| t.layout.panes().first().cloned());
            Ok(typed(
                "worktree_opened",
                json!({
                    "worktree": r["worktree"],
                    "workspace": sn.ws(ws).map(|w| sn.ws_json(w)),
                    "tab": tab.map(|t| sn.tab_json(t)),
                    "root_pane": root.as_deref().and_then(|x| sn.pane(x)).map(|x| sn.pane_json(server, x)),
                    "already_open": r["created"].as_bool().map(|c| !c),
                    "created": r["created"],
                }),
            ))
        }
        // ---- plugin registry ---------------------------------------------------------------
        "plugin.link" => {
            let path = req_s(p, "path")?.to_string();
            let r = registry_change(caller, "plugin_linked", |reg| {
                reg.link(std::path::Path::new(&path)).map(|(e, _)| e.id)
            })?;
            super::views::registry_changed(server);
            Ok(r)
        }
        "plugin.unlink" => {
            let id = sp(p, "plugin_id")
                .or(sp(p, "plugin"))
                .ok_or_else(|| invalid("plugin_id is required"))?
                .to_string();
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("plugin registrations cannot be changed from a pane"));
            }
            // Herdr answers an unknown plugin with `removed: false`, not an error.
            let removed = match Registry::update(&plugin_dirs(), |reg| reg.unlink(&id)) {
                Ok(_) => true,
                Err(RegistryError::NotFound(_)) => false,
                Err(e) => return Err(reg_err(e)),
            };
            if removed {
                super::views::registry_changed(server);
            }
            Ok(typed(
                "plugin_unlinked",
                json!({"plugin_id": id, "removed": removed}),
            ))
        }
        "plugin.enable" | "plugin.disable" => {
            let id = sp(p, "plugin_id")
                .or(sp(p, "plugin"))
                .ok_or_else(|| invalid("plugin_id is required"))?
                .to_string();
            let on = method == "plugin.enable";
            let kind = if on {
                "plugin_enabled"
            } else {
                "plugin_disabled"
            };
            let r = registry_change(caller, kind, |reg| reg.set_enabled(&id, on).map(|e| e.id))?;
            super::views::registry_changed(server);
            Ok(r)
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
            let not_found = || WireError::new("plugin_pane_not_found", "plugin pane not found");
            let pp = state(server)
                .meta
                .lock()
                .unwrap()
                .plugin_panes
                .get(&id)
                .cloned()
                .ok_or_else(not_found)?;
            let pane = sn
                .pane(&id)
                .map(|x| sn.pane_json(server, x))
                .ok_or_else(not_found)?;
            Ok(typed(
                "plugin_pane_focused",
                json!({"plugin_pane": plugin_pane_info(&pp.plugin, &pp.entrypoint, &pp.placement, pane)}),
            ))
        }
        "plugin.pane.close" => {
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("plugin panes cannot be closed from a pane"));
            }
            let id = find_plugin_pane(server, sn, p)?;
            let handle = sn.pane(&id).map(|x| x.handle.clone()).unwrap_or_default();
            native(server, caller, "pane.close", json!({"pane": id})).await?;
            Ok(typed("plugin_pane_closed", json!({"pane_id": handle})))
        }
        "popup.close" => {
            if caller.ctx.pane_scope.is_some() {
                return Err(denied("popups cannot be closed from a pane"));
            }
            // A plugin closes its own popup; the user (or CLI) closes whichever is open.
            let mine = caller.plugin.as_ref().map(|(id, _)| id.clone());
            let open =
                open_popup(server).filter(|(_, pp)| mine.as_deref().is_none_or(|m| pp.plugin == m));
            let Some((id, _)) = open else {
                return Err(WireError::new("popup_not_open", "no popup is open"));
            };
            close_surface(server, &id);
            Ok(ok())
        }
        other => Err(WireError::new(
            "method_not_found",
            format!("unknown method: {other}"),
        )),
    }
}

/// The foreground job of a pane's terminal for `pane.process_info`, as Herdr collects it: the
/// terminal's foreground process group (read from the shell's `tpgid`) and the live members of
/// that group, each `{pid, name, argv0?, argv?, cmdline?, cwd?}`. Linux walks the process
/// trees below the group leader and the shell (`/proc`), keeps members of the group sorted by
/// pid, uses the kernel `comm` as `name` and no `argv0`; macOS lists the group with libproc
/// and adds `argv0` (the basename of `argv[0]` without a login shell's `-`).
mod fgjob {
    use serde_json::{Value, json};

    /// Upper bound on the processes a tree walk visits.
    #[cfg(target_os = "linux")]
    const MAX_VISITED: usize = 1024;

    fn process_json(
        pid: u32,
        name: String,
        argv0: Option<String>,
        argv: Option<Vec<String>>,
    ) -> Value {
        let mut v = json!({"pid": pid, "name": name});
        if let Some(a0) = argv0 {
            v["argv0"] = json!(a0);
        }
        if let Some(argv) = argv {
            v["cmdline"] = json!(argv.join(" "));
            v["argv"] = json!(argv);
        }
        if let Some(cwd) = vk_hold::procinfo::cwd(pid) {
            v["cwd"] = json!(cwd);
        }
        v
    }

    fn argv(pid: u32) -> Option<Vec<String>> {
        Some(vk_hold::procinfo::argv(pid)).filter(|a| !a.is_empty())
    }

    /// `(foreground process group id, processes)`; `None` when the terminal has no foreground
    /// group or none of its members can be read.
    #[cfg(target_os = "linux")]
    pub fn job(shell_pid: u32) -> Option<(u32, Vec<Value>)> {
        use std::collections::{HashSet, VecDeque};
        let pgid = tpgid(shell_pid)?;
        let mut seen = HashSet::new();
        let mut queue: VecDeque<u32> = [pgid, shell_pid].into_iter().collect();
        let mut members = Vec::new();
        while let Some(pid) = queue.pop_front() {
            if pid == 0 || !seen.insert(pid) || seen.len() > MAX_VISITED {
                continue;
            }
            if let Some((pgrp, comm, state)) = stat(pid)
                && pgrp == pgid
            {
                members.push((pid, comm, state));
            }
            queue.extend(vk_hold::procinfo::children(pid));
        }
        members.sort_unstable_by_key(|(pid, _, _)| *pid);
        let procs: Vec<Value> = members
            .into_iter()
            .map(|(pid, comm, state)| {
                // Reading another process's memory blocks on some states (Herdr does the same).
                let argv = (!matches!(state, 'D' | 'Z' | 'X' | 'x'))
                    .then(|| argv(pid))
                    .flatten();
                process_json(pid, comm, None, argv)
            })
            .collect();
        (!procs.is_empty()).then_some((pgid, procs))
    }

    /// `tpgid` of `/proc/<pid>/stat`: the foreground group of the process's terminal.
    #[cfg(target_os = "linux")]
    fn tpgid(pid: u32) -> Option<u32> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = s.get(s.rfind(')')? + 2..)?;
        let f: Vec<&str> = rest.split_whitespace().collect();
        let t: i32 = f.get(5)?.parse().ok()?;
        (t > 0).then_some(t as u32)
    }

    /// `(pgrp, comm, state)` of `/proc/<pid>/stat`.
    #[cfg(target_os = "linux")]
    fn stat(pid: u32) -> Option<(u32, String, char)> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let close = s.rfind(')')?;
        let comm = s.get(s.find('(')? + 1..close)?.to_string();
        let rest = s.get(close + 2..)?;
        let f: Vec<&str> = rest.split_whitespace().collect();
        let state = f.first()?.chars().next()?;
        let pgrp: i32 = f.get(2)?.parse().ok()?;
        (pgrp > 0).then_some((pgrp as u32, comm, state))
    }

    #[cfg(target_os = "macos")]
    pub fn job(shell_pid: u32) -> Option<(u32, Vec<Value>)> {
        let pgid = bsdinfo(shell_pid).map(|i| i.e_tpgid).filter(|t| *t > 0)?;
        let procs: Vec<Value> = group_pids(pgid)
            .into_iter()
            .filter_map(|pid| {
                let info = bsdinfo(pid)?;
                if info.pbi_pgid != pgid {
                    return None;
                }
                let end = info
                    .pbi_comm
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(info.pbi_comm.len());
                if end == 0 {
                    return None;
                }
                let bytes: Vec<u8> = info.pbi_comm[..end].iter().map(|&b| b as u8).collect();
                let name = String::from_utf8(bytes).ok()?;
                let argv = argv(pid);
                let argv0 = argv.as_ref().and_then(|a| argv0_name(&a[0]));
                Some(process_json(pid, name, argv0, argv))
            })
            .collect();
        (!procs.is_empty()).then_some((pgid, procs))
    }

    /// Basename of `argv[0]` without a login shell's leading `-`.
    #[cfg(target_os = "macos")]
    fn argv0_name(a0: &str) -> Option<String> {
        let base = std::path::Path::new(a0).file_name()?.to_str()?;
        let name = base.strip_prefix('-').unwrap_or(base);
        (!name.is_empty()).then(|| name.to_string())
    }

    #[cfg(target_os = "macos")]
    fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
        // SAFETY: an all-zero proc_bsdinfo is valid; proc_pidinfo fills at most `size` bytes.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a properly sized out-buffer.
        let r = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (r == size).then_some(info)
    }

    /// Members of process group `pgid` (`proc_listpids(PROC_PGRP_ONLY)`).
    #[cfg(target_os = "macos")]
    fn group_pids(pgid: u32) -> Vec<u32> {
        const PROC_PGRP_ONLY: u32 = 2;
        let mut cap = 16usize;
        for _ in 0..8 {
            let mut pids = vec![0 as libc::pid_t; cap];
            let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
            // SAFETY: the buffer holds `bytes` bytes.
            let n = unsafe {
                libc::proc_listpids(PROC_PGRP_ONLY, pgid, pids.as_mut_ptr().cast(), bytes)
            };
            if n <= 0 {
                return Vec::new();
            }
            if n < bytes {
                let count = n as usize / std::mem::size_of::<libc::pid_t>();
                return pids
                    .into_iter()
                    .take(count)
                    .filter(|p| *p > 0)
                    .map(|p| p as u32)
                    .collect();
            }
            cap *= 2;
        }
        Vec::new()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn job(_shell_pid: u32) -> Option<(u32, Vec<Value>)> {
        None
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

    fn area(x: u16, y: u16, width: u16, height: u16) -> Area {
        Area {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn pane_layout_geometry_follows_herdrs_binary_split_rects() {
        // [a | (b / c) | d]: the n-ary row nests as a vs ((b/c) vs d).
        let n = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            children: vec![
                (leaf("a"), 0.5),
                (
                    LayoutNode::Split {
                        dir: SplitDir::Vertical,
                        children: vec![(leaf("b"), 0.3), (leaf("c"), 0.7)],
                    },
                    0.25,
                ),
                (leaf("d"), 0.25),
            ],
        };
        let g = geometry(&n, area(0, 0, 101, 40));
        // Herdr's `split_rect`: round(width * ratio) cells to the first side, no divider.
        assert_eq!(
            g.panes,
            vec![
                ("a".to_string(), area(0, 0, 51, 40)),
                ("b".to_string(), area(51, 0, 25, 12)),
                ("c".to_string(), area(51, 12, 25, 28)),
                ("d".to_string(), area(76, 0, 25, 40)),
            ]
        );
        let ids: Vec<String> = g
            .splits
            .iter()
            .enumerate()
            .map(|(i, (p, _, _, _))| split_id(i, p))
            .collect();
        assert_eq!(ids, vec!["split_0_root", "split_1_1", "split_2_10"]);
        let (_, dir, ratio, rect) = &g.splits[1];
        assert_eq!(dir_name(*dir), "right");
        assert!((ratio - 0.5).abs() < 1e-6, "(b/c) vs d share the rest");
        assert_eq!(*rect, area(51, 0, 50, 40));
        assert_eq!(dir_name(g.splits[2].1), "down");
        // A split id from `pane.layout` addresses the same split as Herdr's path.
        assert_eq!(split_path_digits("split_0_root"), Some(vec![]));
        assert_eq!(split_path_digits("split_2_10"), Some(b"10".to_vec()));
        assert_eq!(split_path_digits("s01"), Some(b"01".to_vec()));
        assert_eq!(split_path_digits("split_3"), None);
        assert_eq!(split_path_digits("x"), None);
    }

    #[test]
    fn ratios_serialize_like_herdrs_f32() {
        assert_eq!(ratio_json(0.3).to_string(), "0.3");
        assert_eq!(ratio_json(0.5).to_string(), "0.5");
        assert_eq!(ratio_json(1.0 / 3.0).to_string(), "0.33333334");
        assert_eq!(first_share(&[(leaf("a"), 0.0), (leaf("b"), 0.0)]), 0.5);
    }

    #[test]
    fn foreground_job_of_this_process_lists_its_group() {
        // This test process has no controlling terminal under a test runner, so the job may
        // be absent; when it is present it has Herdr's shape.
        if let Some((pgid, procs)) = fgjob::job(std::process::id()) {
            assert!(pgid > 0);
            for p in &procs {
                assert!(p["pid"].as_u64().unwrap() > 0, "{p}");
                assert!(p["name"].is_string(), "{p}");
                if let Some(argv) = p.get("argv") {
                    let joined: Vec<&str> = argv
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|a| a.as_str().unwrap())
                        .collect();
                    assert_eq!(p["cmdline"], joined.join(" "), "{p}");
                }
            }
        }
        assert!(fgjob::job(0).is_none());
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
