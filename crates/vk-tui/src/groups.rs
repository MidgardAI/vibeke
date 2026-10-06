//! Workspace groups in the sidebar (08 §2.1, D#1620): a group level between machine and
//! workspace with aggregate agent counts, collapse/expand (`group.collapse`), moving workspaces
//! between groups (navigate-mode `m`, palette, drag a workspace/agent row onto a group row) and
//! create/rename from the palette. Membership lives on the server's `Group.workspaces`.

use crate::app::{App, Mode, Pending, Popup, Prompt, PromptKind, RpcErr};
use crate::draw::SideRow;
use crate::parity::Reply;
use crate::screen::Grid;
use crossterm::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::collections::HashSet;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::model::*;

/// Nesting deeper than this is drawn flat (and guards against parent cycles).
const MAX_DEPTH: usize = 6;

#[derive(Default)]
pub struct State {
    /// A sidebar drag in progress: (machine, workspace, start row).
    pub drag: Option<(usize, String, u16)>,
    /// A group row pressed (a click toggles, a drag reorders): (machine, group, start row).
    pub group_drag: Option<(usize, String, u16)>,
    /// The row a dragged group is over (drop marker).
    pub group_over: Option<u16>,
}

/// Aggregate agent counts of a set of workspaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub working: usize,
    pub done: usize,
    /// Runs with an open approval/question.
    pub needs: usize,
    pub error: usize,
    pub total: usize,
}

pub fn counts(app: &App, mi: usize, workspaces: &HashSet<String>) -> Counts {
    let m = &app.machines[mi];
    let mut c = Counts::default();
    for r in &m.model.runs {
        let Some(p) = m.model.panes.iter().find(|p| p.id == r.pane) else {
            continue;
        };
        if !workspaces.contains(&p.workspace) {
            continue;
        }
        c.total += 1;
        let open = m
            .model
            .interactions
            .iter()
            .any(|i| i.run == r.id && i.status == InteractionStatus::Open);
        if open {
            c.needs += 1;
            continue;
        }
        match r.execution.value {
            Execution::Working => c.working += 1,
            Execution::Error | Execution::RateLimited => c.error += 1,
            Execution::Idle if r.done_rev > *m.seen.get(&r.pane).unwrap_or(&0) => c.done += 1,
            _ => {}
        }
    }
    c
}

/// A group's workspaces, child groups included.
pub fn members(groups: &[Group], gid: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut stack = vec![(gid.to_string(), 0usize)];
    while let Some((id, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        if let Some(g) = groups.iter().find(|g| g.id == id) {
            out.extend(g.workspaces.iter().cloned());
            for c in groups.iter().filter(|c| c.parent.as_deref() == Some(&id)) {
                stack.push((c.id.clone(), depth + 1));
            }
        }
    }
    out
}

pub fn group_of_ws<'a>(m: &'a SessionModel, ws: &str) -> Option<&'a Group> {
    m.groups
        .iter()
        .find(|g| g.workspaces.iter().any(|w| w == ws))
}

fn badges(app: &App, c: Counts) -> Vec<(String, vk_proto::render::Style)> {
    let t = &app.theme;
    let mut v = Vec::new();
    if c.needs > 0 {
        v.push((format!(" ⚠{}", c.needs), t.bold(t.red)));
    }
    if c.error > 0 {
        v.push((format!(" ✗{}", c.error), t.bold(t.red)));
    }
    if c.working > 0 {
        v.push((format!(" ●{}", c.working), t.bold(t.accent)));
    }
    if c.done > 0 {
        v.push((format!(" ✓{}", c.done), t.bold(t.green)));
    }
    v
}

/// The text of a group row (tests).
pub fn group_row_text(app: &App, mi: usize, gid: &str) -> String {
    let mut rows = Vec::new();
    if let Some(g) = app.machines[mi].model.groups.iter().find(|g| g.id == gid) {
        push_group(app, mi, g, 0, &mut rows);
    }
    rows.first()
        .map(|r| r.segs.iter().map(|(s, _)| s.as_str()).collect())
        .unwrap_or_default()
}

/// One machine's tree: groups (by order, nested) with their workspaces, then ungrouped
/// workspaces in model order.
pub fn push_machine(app: &App, mi: usize, rows: &mut Vec<SideRow>) {
    let m = &app.machines[mi];
    let groups = &m.model.groups;
    if groups.is_empty() {
        for w in &m.model.workspaces {
            crate::draw::workspace_rows(app, mi, w, 0, rows);
        }
        return;
    }
    let mut tops: Vec<&Group> = groups
        .iter()
        .filter(|g| match &g.parent {
            None => true,
            Some(p) => !groups.iter().any(|x| &x.id == p),
        })
        .collect();
    tops.sort_by(|a, b| a.order.total_cmp(&b.order));
    for g in tops {
        push_group(app, mi, g, 0, rows);
    }
    let grouped: HashSet<&String> = groups.iter().flat_map(|g| g.workspaces.iter()).collect();
    for w in m
        .model
        .workspaces
        .iter()
        .filter(|w| !grouped.contains(&w.id))
    {
        crate::draw::workspace_rows(app, mi, w, 0, rows);
    }
}

fn push_group(app: &App, mi: usize, g: &Group, depth: usize, rows: &mut Vec<SideRow>) {
    let m = &app.machines[mi];
    let t = &app.theme;
    let c = counts(app, mi, &members(&m.model.groups, &g.id));
    let pad = "  ".repeat(depth);
    let arrow = if g.collapsed { "▸ " } else { "▾ " };
    let mut segs = vec![
        (format!("{pad}{arrow}"), t.s(t.muted)),
        (g.name.clone(), t.bold(t.fg)),
    ];
    segs.extend(badges(app, c));
    if g.collapsed && c.total == 0 && !g.workspaces.is_empty() {
        segs.push((format!(" ({})", g.workspaces.len()), t.dim()));
    }
    // Being dragged to a new place (08 §2.1).
    if app
        .parity
        .groups
        .group_drag
        .as_ref()
        .is_some_and(|(m, id, _)| *m == mi && *id == g.id)
        && app.parity.groups.group_over.is_some()
    {
        segs.push((" ⇅ moving".into(), t.bold(t.accent)));
    }
    rows.push(SideRow {
        segs,
        group: Some((mi, g.id.clone())),
        ..Default::default()
    });
    if g.collapsed || depth >= MAX_DEPTH {
        return;
    }
    let mut kids: Vec<&Group> = m
        .model
        .groups
        .iter()
        .filter(|x| x.parent.as_deref() == Some(&g.id))
        .collect();
    kids.sort_by(|a, b| a.order.total_cmp(&b.order));
    for k in kids {
        push_group(app, mi, k, depth + 1, rows);
    }
    for wid in &g.workspaces {
        if let Some(w) = m.model.workspaces.iter().find(|w| &w.id == wid) {
            crate::draw::workspace_rows(app, mi, w, depth + 1, rows);
        }
    }
}

/// Toggle (or set) a group's collapsed state; drawn at once, confirmed by the model.
pub fn collapse(app: &mut App, mi: usize, gid: &str, collapsed: Option<bool>) {
    let Some(g) = app.machines[mi]
        .model
        .groups
        .iter_mut()
        .find(|g| g.id == gid)
    else {
        return;
    };
    let v = collapsed.unwrap_or(!g.collapsed);
    g.collapsed = v;
    app.dirty = true;
    app.command_on(
        mi,
        "group.collapse",
        json!({"group": gid, "collapsed": v}),
        Pending::Parity(Reply::Ignore),
    );
}

/// Move `ws` into `group` (`None` = out of any group).
pub fn move_ws(app: &mut App, mi: usize, ws: &str, group: Option<&str>) {
    let (method, params, msg) = match group {
        Some(g) => {
            let name = app.machines[mi]
                .model
                .groups
                .iter()
                .find(|x| x.id == g)
                .map(|x| x.name.clone())
                .unwrap_or_default();
            (
                "group.add",
                json!({"group": g, "workspace": ws}),
                format!("moved to group {name}"),
            )
        }
        None => (
            "group.remove",
            json!({"workspace": ws}),
            "moved out of its group".to_string(),
        ),
    };
    app.command_on(mi, method, params, Pending::Toast(msg));
}

pub fn create(app: &mut App, mi: usize, name: &str, ws: Option<String>) {
    if name.is_empty() {
        return;
    }
    app.command_on(
        mi,
        "group.create",
        json!({"name": name}),
        Pending::Parity(Reply::GroupCreated { ws }),
    );
}

pub fn on_created(app: &mut App, mi: usize, ws: Option<String>, res: Result<Value, RpcErr>) {
    match res {
        Ok(v) => {
            let gid = v.pointer("/group/id").and_then(Value::as_str).unwrap_or("");
            let name = v
                .pointer("/group/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            match ws {
                Some(w) if !gid.is_empty() => move_ws(app, mi, &w, Some(gid)),
                _ => app.toast(format!("group {name} created")),
            }
        }
        Err(e) => app.toast(format!("✗ {}", e.message)),
    }
}

pub fn rename(app: &mut App, mi: usize, gid: &str, name: &str) {
    if name.is_empty() {
        return;
    }
    app.command_on(
        mi,
        "group.rename",
        json!({"group": gid, "name": name}),
        Pending::Parity(Reply::Ignore),
    );
}

fn ws_of_pane(app: &App, mi: usize, pane: &str) -> Option<String> {
    app.machines[mi]
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .map(|p| p.workspace.clone())
}

fn open_pick(app: &mut App, mi: usize, ws: String) {
    let sel = group_of_ws(&app.machines[mi].model, &ws)
        .and_then(|g| {
            pick_items(app, mi)
                .iter()
                .position(|i| i.0.as_deref() == Some(&g.id))
        })
        .unwrap_or(0);
    app.mode = Mode::Popup(Popup::GroupPick { mi, ws, sel });
}

pub fn action(app: &mut App, action: &str) -> bool {
    let cur = app.cur;
    let ws = app.focused_ws().map(|w| w.id);
    let group = ws
        .as_ref()
        .and_then(|w| group_of_ws(&app.machines[cur].model, w))
        .cloned();
    match action {
        "group_new" => {
            app.mode = Mode::Prompt(Prompt {
                kind: PromptKind::GroupNew { mi: cur, ws },
                label: "new group name".into(),
                input: String::new(),
            })
        }
        "group_move" => match ws {
            Some(w) => open_pick(app, cur, w),
            None => app.toast("no focused workspace"),
        },
        "group_rename" => match group {
            Some(g) => {
                app.mode = Mode::Prompt(Prompt {
                    kind: PromptKind::GroupRename {
                        mi: cur,
                        group: g.id.clone(),
                    },
                    label: "group name".into(),
                    input: g.name.clone(),
                })
            }
            None => app.toast("this workspace isn't in a group — :group_move"),
        },
        "group_collapse" => match group {
            Some(g) => collapse(app, cur, &g.id, None),
            None => app.toast("this workspace isn't in a group"),
        },
        _ => return false,
    }
    true
}

/// Navigate-mode keys for group rows (enter/space toggle, l/→ expand, h/← collapse, r rename)
/// plus `m` (move the selected row's workspace into a group) and `G` (new group) on any row.
/// Other keys on a group row are swallowed so pane actions never get a group id.
pub fn navigate_key(app: &mut App, ev: &KeyEvent, sel: usize) -> bool {
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Navigate { sel };
        return true;
    }
    let rows = crate::draw::sidebar_rows(app);
    let item = rows.iter().filter(|r| r.selectable()).nth(sel);
    let group = item.and_then(|r| r.group.clone());
    let target = item.and_then(|r| r.target.clone());
    let stay = |app: &mut App| app.mode = Mode::Navigate { sel };
    match ev.key {
        Key::Char('G') => {
            app.mode = Mode::Prompt(Prompt {
                kind: PromptKind::GroupNew {
                    mi: app.cur,
                    ws: None,
                },
                label: "new group name".into(),
                input: String::new(),
            });
            return true;
        }
        Key::Char('m') => {
            match target.and_then(|(mi, p)| ws_of_pane(app, mi, &p).map(|w| (mi, w))) {
                Some((mi, w)) => open_pick(app, mi, w),
                None => stay(app),
            }
            return true;
        }
        _ => {}
    }
    let Some((mi, gid)) = group else {
        return false;
    };
    // Native plugin sidebar rows (crate::plugin_ui).
    if crate::plugin_ui::is_plugin_row(&gid) {
        if matches!(ev.key, Key::Named(NamedKey::Enter) | Key::Char(' ')) {
            crate::plugin_ui::activate(app, mi, &gid);
        }
        stay(app);
        return true;
    }
    match ev.key {
        Key::Named(NamedKey::Enter) | Key::Char(' ') => collapse(app, mi, &gid, None),
        Key::Char('l') | Key::Named(NamedKey::Right) => collapse(app, mi, &gid, Some(false)),
        Key::Char('h') | Key::Named(NamedKey::Left) => collapse(app, mi, &gid, Some(true)),
        Key::Char('r') => {
            let name = app.machines[mi]
                .model
                .groups
                .iter()
                .find(|g| g.id == gid)
                .map(|g| g.name.clone())
                .unwrap_or_default();
            app.mode = Mode::Prompt(Prompt {
                kind: PromptKind::GroupRename { mi, group: gid },
                label: "group name".into(),
                input: name,
            });
            return true;
        }
        // Movement, digits, esc and new-workspace keep their navigate-mode meaning.
        Key::Named(NamedKey::Up | NamedKey::Down | NamedKey::Escape)
        | Key::Char('j' | 'k' | 'q' | 'n') => return false,
        Key::Char(c) if c.is_ascii_digit() => return false,
        _ => {}
    }
    stay(app);
    true
}

/// Where dropping group `gid` on group row `target` puts it (`group.move {group, parent,
/// index}`): next to `target` among `target`'s siblings, after it when dragged downwards and
/// before it when dragged upwards. `None` when `target` is `gid` itself or inside it.
pub fn drop_position(
    groups: &[Group],
    gid: &str,
    target: &str,
    downwards: bool,
) -> Option<(Option<String>, usize)> {
    if gid == target || members_groups(groups, gid).contains(target) {
        return None;
    }
    let t = groups.iter().find(|g| g.id == target)?;
    let parent = t
        .parent
        .clone()
        .filter(|p| groups.iter().any(|g| &g.id == p));
    let mut sibs: Vec<&Group> = groups
        .iter()
        .filter(|g| {
            g.id != gid
                && g.parent
                    .clone()
                    .filter(|p| groups.iter().any(|x| &x.id == p))
                    == parent
        })
        .collect();
    sibs.sort_by(|a, b| a.order.total_cmp(&b.order));
    let i = sibs.iter().position(|g| g.id == target)?;
    Some((parent, if downwards { i + 1 } else { i }))
}

/// Group ids nested anywhere under `gid`.
fn members_groups(groups: &[Group], gid: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut stack = vec![(gid.to_string(), 0usize)];
    while let Some((id, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        for c in groups.iter().filter(|c| c.parent.as_deref() == Some(&id)) {
            if out.insert(c.id.clone()) {
                stack.push((c.id.clone(), depth + 1));
            }
        }
    }
    out
}

/// Release of a group drag at row `y` (started at `from`): a click toggles the group, a drop
/// on another group row of the same machine reorders (08 §2.1).
fn drop_group(app: &mut App, mi: usize, gid: &str, from: u16, y: u16, row: Option<&SideRow>) {
    if from == y {
        collapse(app, mi, gid, None);
        return;
    }
    let Some((tm, target)) = row.and_then(|r| r.group.clone()) else {
        return;
    };
    if tm != mi {
        app.toast("groups move within their own machine only");
        return;
    }
    match drop_position(&app.machines[mi].model.groups, gid, &target, y > from) {
        Some((parent, index)) => app.command_on(
            mi,
            "group.move",
            json!({"group": gid, "parent": parent, "index": index}),
            Pending::Parity(Reply::Ignore),
        ),
        None if gid != target => app.toast("a group can't move into itself"),
        None => {}
    }
}

/// Sidebar mouse: a click on a group row toggles it; dragging a group row onto another group
/// row reorders the groups (`group.move`); dragging a workspace/agent row onto a group row moves
/// the workspace into that group, onto an ungrouped workspace takes it out.
pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    let (x, y) = (me.column, me.row);
    let in_sidebar = crate::chrome::in_sidebar(app, x);
    if !in_sidebar {
        if matches!(me.kind, MouseEventKind::Up(_)) {
            app.parity.groups.drag = None;
            app.parity.groups.group_drag = None;
        }
        return false;
    }
    let rows = crate::draw::sidebar_rows(app);
    let row = y.checked_sub(1).and_then(|i| rows.get(i as usize));
    if let Some((mi, gid, from)) = app.parity.groups.group_drag.clone() {
        match me.kind {
            MouseEventKind::Drag(CtButton::Left) => {
                app.parity.groups.group_over = Some(y);
                app.dirty = true;
                return true;
            }
            MouseEventKind::Up(CtButton::Left) => {
                app.parity.groups.group_drag = None;
                app.parity.groups.group_over = None;
                drop_group(app, mi, &gid, from, y, row);
                app.dirty = true;
                return true;
            }
            _ => {}
        }
    }
    match me.kind {
        MouseEventKind::Down(CtButton::Left) => {
            if let Some((mi, gid)) = row.and_then(|r| r.group.clone()) {
                if crate::plugin_ui::is_plugin_row(&gid) {
                    crate::plugin_ui::activate(app, mi, &gid);
                    return true;
                }
                // A click (release on the same row) toggles; a drag reorders.
                app.parity.groups.group_drag = Some((mi, gid, y));
                app.parity.groups.group_over = None;
                return true;
            }
            app.parity.groups.drag = row
                .and_then(|r| r.target.clone())
                .and_then(|(mi, p)| ws_of_pane(app, mi, &p).map(|w| (mi, w, y)));
            false
        }
        MouseEventKind::Drag(CtButton::Left) => app.parity.groups.drag.is_some(),
        MouseEventKind::Up(CtButton::Left) => {
            let Some((mi, ws, from)) = app.parity.groups.drag.take() else {
                return false;
            };
            if from == y {
                return false;
            }
            let Some(row) = row else {
                return true;
            };
            if let Some((gm, gid)) = row.group.clone() {
                if gm == mi {
                    move_ws(app, mi, &ws, Some(&gid));
                } else {
                    app.toast("groups hold workspaces of their own machine only");
                }
                return true;
            }
            if let Some((tm, p)) = row.target.clone()
                && tm == mi
                && let Some(tw) = ws_of_pane(app, mi, &p)
                && tw != ws
            {
                let dest = group_of_ws(&app.machines[mi].model, &tw).map(|g| g.id.clone());
                let src = group_of_ws(&app.machines[mi].model, &ws).map(|g| g.id.clone());
                if dest != src {
                    move_ws(app, mi, &ws, dest.as_deref());
                }
            }
            true
        }
        _ => false,
    }
}

/// Picker items: `(None, "(no group)")`, then every group (indented by depth).
pub fn pick_items(app: &App, mi: usize) -> Vec<(Option<String>, String)> {
    let groups = &app.machines[mi].model.groups;
    let mut out = vec![(None, "(no group)".to_string())];
    fn walk(
        groups: &[Group],
        parent: Option<&str>,
        depth: usize,
        out: &mut Vec<(Option<String>, String)>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut kids: Vec<&Group> = groups
            .iter()
            .filter(|g| match (g.parent.as_deref(), parent) {
                (None, None) => true,
                (Some(p), None) => !groups.iter().any(|x| x.id == p),
                (Some(p), Some(q)) => p == q,
                (None, Some(_)) => false,
            })
            .collect();
        kids.sort_by(|a, b| a.order.total_cmp(&b.order));
        for g in kids {
            out.push((
                Some(g.id.clone()),
                format!("{}{}", "  ".repeat(depth), g.name),
            ));
            walk(groups, Some(&g.id), depth + 1, out);
        }
    }
    walk(groups, None, 0, &mut out);
    out
}

pub fn pick_key(app: &mut App, ev: KeyEvent, mi: usize, ws: String, sel: usize) {
    let items = pick_items(app, mi);
    // The last line is "+ new group…".
    let n = items.len() + 1;
    let stay = |app: &mut App, ws: String, sel: usize| {
        app.mode = Mode::Popup(Popup::GroupPick { mi, ws, sel })
    };
    if ev.kind == KeyKind::Release {
        return stay(app, ws, sel);
    }
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {}
        Key::Named(NamedKey::Down) | Key::Char('j') => stay(app, ws, (sel + 1) % n),
        Key::Named(NamedKey::Up) | Key::Char('k') => stay(app, ws, (sel + n - 1) % n),
        Key::Char('n') => {
            app.mode = Mode::Prompt(Prompt {
                kind: PromptKind::GroupNew { mi, ws: Some(ws) },
                label: "new group name".into(),
                input: String::new(),
            })
        }
        Key::Named(NamedKey::Enter) => {
            if sel >= items.len() {
                app.mode = Mode::Prompt(Prompt {
                    kind: PromptKind::GroupNew { mi, ws: Some(ws) },
                    label: "new group name".into(),
                    input: String::new(),
                });
            } else {
                let cur = group_of_ws(&app.machines[mi].model, &ws).map(|g| g.id.clone());
                if items[sel].0 != cur {
                    move_ws(app, mi, &ws, items[sel].0.as_deref());
                }
            }
        }
        _ => stay(app, ws, sel),
    }
}

pub fn draw_pick(app: &App, g: &mut Grid, mi: usize, ws: &str, sel: usize) {
    let t = app.theme;
    let m = &app.machines[mi];
    let name = m
        .model
        .workspaces
        .iter()
        .find(|w| w.id == ws)
        .map(|w| w.display_name().to_string())
        .unwrap_or_default();
    let items = pick_items(app, mi);
    let h = (items.len() as u16 + 5).min(24);
    let mut b = crate::popups::frame(app, g, 56, h, &format!("move {name} to group"));
    let cur = group_of_ws(&m.model, ws).map(|g| g.id.clone());
    for (i, (id, label)) in items.iter().enumerate() {
        let mark = if *id == cur { "● " } else { "  " };
        let st = if i == sel { t.sel(t.fg) } else { t.text() };
        b.line(&format!("{mark}{label}"), st);
    }
    let st = if sel >= items.len() {
        t.sel(t.accent)
    } else {
        t.s(t.accent)
    };
    b.line("  + new group…", st);
    b.line("[enter] move  [n] new group  [esc] cancel", t.dim());
}
