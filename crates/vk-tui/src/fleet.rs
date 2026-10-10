//! Fleet grid (08 §6.6): one tile per agent run on every connected machine, a view over the
//! pane area (not a layout change; agents keep running and keep their PTY sizes).
//!
//! Tiles default to a live miniature of the agent's own terminal (`ui.fleet.tile_view =
//! "terminal"`): the last non-empty rows of its screen, cropped to the tile. The text comes from
//! `pane.read {source: visible}` on the run's machine, refreshed about once a second while the
//! grid is open (only then), so panes in other tabs, workspaces and machines show too. `t` flips
//! the selected tile to Vibeke's structured timeline (state with age, last tool, last message,
//! task/branch, turns) and back; `T` flips every tile. `enter` focuses the real pane, `esc`
//! closes. The grid never sends bytes to an agent.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::{Grid, Rect as SRect};
use crate::time::{Duration, Instant};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use vk_config::TileView;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// Screen text refresh while the grid is open.
pub const REFRESH: Duration = Duration::from_secs(1);
const TILE_W: u16 = 40;
const TILE_H: u16 = 10;

pub type RunId = (usize, String);

#[derive(Debug, Clone, Default)]
pub struct View {
    pub sel: usize,
    /// Tiles showing the other view than `ui.fleet.tile_view`.
    pub flipped: HashSet<RunId>,
    /// `T`: every tile flipped.
    pub all_flipped: bool,
    /// Screen text per (machine, pane).
    pub screens: HashMap<(usize, String), Vec<String>>,
    pub fetched_at: Option<Instant>,
    /// Reads in flight (one per pane at most).
    pub inflight: HashSet<(usize, String)>,
    /// First tile row shown (scrolls to keep the selection visible).
    pub top: usize,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Read { pane: String },
}

/// One tile: (machine, run).
pub fn tiles(app: &App) -> Vec<RunId> {
    let mut v = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        for r in &m.model.runs {
            if r.ended_at_ms.is_none() || m.model.panes.iter().any(|p| p.id == r.pane) {
                v.push((mi, r.id.clone()));
            }
        }
    }
    v
}

pub fn open(app: &mut App) {
    app.ux.fleet = Some(View::default());
    app.mode = Mode::Popup(Popup::Fleet);
    fetch(app);
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action == "fleet" || action == "fleet_grid" {
        open(app);
        return true;
    }
    false
}

/// Ask every tile's machine for its screen text (skipping reads still in flight).
pub fn fetch(app: &mut App) {
    let list: Vec<(usize, String)> = tiles(app)
        .into_iter()
        .filter_map(|(mi, run)| {
            let m = &app.machines[mi];
            m.model
                .runs
                .iter()
                .find(|r| r.id == run)
                .map(|r| (mi, r.pane.clone()))
        })
        .collect();
    let Some(v) = app.ux.fleet.as_mut() else {
        return;
    };
    v.fetched_at = Some(Instant::now());
    let todo: Vec<(usize, String)> = list
        .into_iter()
        .filter(|k| v.inflight.insert(k.clone()))
        .collect();
    for (mi, pane) in todo {
        if !app.machines[mi].connected() {
            if let Some(v) = app.ux.fleet.as_mut() {
                v.inflight.remove(&(mi, pane));
            }
            continue;
        }
        app.command_on(
            mi,
            "pane.read",
            json!({"pane": pane, "source": "visible", "lines": 60}),
            Pending::Ux(crate::ux::Reply::Fleet(Reply::Read { pane: pane.clone() })),
        );
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    let Reply::Read { pane } = r;
    let Some(v) = app.ux.fleet.as_mut() else {
        return;
    };
    v.inflight.remove(&(mi, pane.clone()));
    if let Ok(x) = res {
        let lines = x["text"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        v.screens.insert((mi, pane), lines);
        app.dirty = true;
    }
}

pub fn tick(app: &mut App, now: Instant) {
    if !matches!(app.mode, Mode::Popup(Popup::Fleet)) {
        if app.ux.fleet.is_some() && !matches!(app.mode, Mode::Popup(_)) {
            app.ux.fleet = None;
        }
        return;
    }
    if app
        .ux
        .fleet
        .as_ref()
        .and_then(|v| v.fetched_at)
        .is_none_or(|t| now.duration_since(t) >= REFRESH)
    {
        fetch(app);
    }
}

pub fn deadlines(app: &App, _now: Instant, d: &mut crate::deadline::Deadlines) {
    if let (Mode::Popup(Popup::Fleet), Some(v)) = (&app.mode, &app.ux.fleet)
        && let Some(t) = v.fetched_at
    {
        d.at("fleet", t + REFRESH);
    }
}

fn grid_cols(area_w: u16) -> usize {
    (area_w / TILE_W).max(1) as usize
}

pub fn key(app: &mut App, ev: KeyEvent) {
    let keep = |app: &mut App| app.mode = Mode::Popup(Popup::Fleet);
    if ev.kind == KeyKind::Release {
        return keep(app);
    }
    let list = tiles(app);
    let n = list.len();
    let cols = grid_cols(app.pane_area().w);
    let Some(mut v) = app.ux.fleet.take() else {
        return;
    };
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => return,
        Key::Char('l') | Key::Named(NamedKey::Right) => {
            v.sel = (v.sel + 1).min(n.saturating_sub(1))
        }
        Key::Char('h') | Key::Named(NamedKey::Left) => v.sel = v.sel.saturating_sub(1),
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            if v.sel + cols < n {
                v.sel += cols;
            }
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => v.sel = v.sel.saturating_sub(cols),
        Key::Char('t') => {
            if let Some(id) = list.get(v.sel)
                && !v.flipped.remove(id)
            {
                v.flipped.insert(id.clone());
            }
        }
        Key::Char('T') => {
            v.all_flipped = !v.all_flipped;
            v.flipped.clear();
        }
        Key::Named(NamedKey::Enter) => {
            if let Some((mi, run)) = list.get(v.sel)
                && let Some(p) = app.machines[*mi]
                    .model
                    .runs
                    .iter()
                    .find(|r| &r.id == run)
                    .map(|r| r.pane.clone())
            {
                app.focus_pane(*mi, &p);
                return;
            }
        }
        _ => {}
    }
    app.ux.fleet = Some(v);
    keep(app);
}

/// The view a tile shows.
pub fn tile_view(app: &App, v: &View, id: &RunId) -> TileView {
    let flip = v.all_flipped ^ v.flipped.contains(id);
    match (app.config.ui.fleet.tile_view, flip) {
        (TileView::Terminal, false) | (TileView::Timeline, true) => TileView::Terminal,
        _ => TileView::Timeline,
    }
}

/// Body lines of one tile (`h` rows at most).
pub fn tile_body(app: &App, v: &View, id: &RunId, h: usize) -> Vec<String> {
    let m = &app.machines[id.0];
    let Some(r) = m.model.runs.iter().find(|r| r.id == id.1) else {
        return vec![];
    };
    match tile_view(app, v, id) {
        TileView::Terminal => {
            let from_read = v.screens.get(&(id.0, r.pane.clone())).cloned();
            let lines = from_read.unwrap_or_else(|| {
                m.panes
                    .get(&r.pane)
                    .map(|b| {
                        b.lines
                            .iter()
                            .map(|l| l.text().trim_end().to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            });
            let last = lines
                .iter()
                .rposition(|l| !l.is_empty())
                .map(|i| i + 1)
                .unwrap_or(0);
            if last == 0 {
                return vec!["(no output yet)".into()];
            }
            lines[last.saturating_sub(h)..last].to_vec()
        }
        TileView::Timeline => {
            let (glyph, label, _, inferred) = crate::draw::run_state(app, m, r);
            let mut out = vec![format!(
                "{glyph} {label}{}",
                if inferred { " (inferred)" } else { "" }
            )];
            if let Some(tool) = &r.last_tool {
                out.push(format!("tool: {tool}"));
            }
            let ws = m
                .model
                .panes
                .iter()
                .find(|p| p.id == r.pane)
                .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace));
            if let Some(b) = ws.and_then(|w| w.branch.as_deref()) {
                out.push(format!("⎇ {b}"));
            }
            out.push(format!("turns: {}", r.turns_completed));
            if let Some(msg) = &r.last_message {
                out.push("─".into());
                out.extend(msg.lines().map(str::to_string));
            }
            out.truncate(h);
            out
        }
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = &app.ux.fleet else {
        return;
    };
    let t = app.theme;
    let area = app.pane_area();
    let list = tiles(app);
    let header = format!(
        "fleet · {} agent(s) on {} machine(s) · tiles: {} (t flips one, T all)",
        list.len(),
        app.machines.len(),
        app.config.ui.fleet.tile_view.as_str()
    );
    crate::drafts::Area::open(app, g, &header);
    if list.is_empty() {
        g.put_str(
            area.x + 2,
            area.y + 2,
            "no agents running — start one in a pane (claude, codex, pi…)",
            t.dim(),
            area.w.saturating_sub(4),
        );
        return;
    }
    let cols = grid_cols(area.w);
    let tw = area.w / cols as u16;
    let body_h = area.h.saturating_sub(2);
    let rows_fit = (body_h / TILE_H).max(1) as usize;
    let sel_row = v.sel / cols;
    let top = if sel_row >= rows_fit {
        sel_row + 1 - rows_fit
    } else {
        0
    };
    for (i, id) in list.iter().enumerate() {
        let row = i / cols;
        if row < top || row >= top + rows_fit {
            continue;
        }
        let x = area.x + (i % cols) as u16 * tw;
        let y = area.y + 1 + (row - top) as u16 * TILE_H;
        let r = SRect {
            x,
            y,
            w: tw.saturating_sub(1),
            h: TILE_H.min(area.y + area.h - y),
        };
        draw_tile(app, g, v, id, r, i == v.sel);
    }
    g.put_str(
        area.x + 1,
        area.y + area.h.saturating_sub(1),
        "h j k l move · enter focus the pane · t flip tile · T flip all · esc back",
        t.dim(),
        area.w.saturating_sub(2),
    );
}

fn draw_tile(app: &App, g: &mut Grid, v: &View, id: &RunId, r: SRect, selected: bool) {
    let t = app.theme;
    if r.w < 6 || r.h < 3 {
        return;
    }
    let m = &app.machines[id.0];
    let Some(run) = m.model.runs.iter().find(|x| x.id == id.1) else {
        return;
    };
    let b = if selected {
        t.bold(t.accent)
    } else {
        t.border(false)
    };
    let (x1, y1) = (r.x + r.w - 1, r.y + r.h - 1);
    for x in r.x..=x1 {
        g.put_str(x, r.y, "─", b, 1);
        g.put_str(x, y1, "─", b, 1);
    }
    for y in r.y..=y1 {
        g.put_str(r.x, y, "│", b, 1);
        g.put_str(x1, y, "│", b, 1);
    }
    g.put_str(r.x, r.y, "╭", b, 1);
    g.put_str(x1, r.y, "╮", b, 1);
    g.put_str(r.x, y1, "╰", b, 1);
    g.put_str(x1, y1, "╯", b, 1);
    let (glyph, _, color, _) = crate::draw::run_state(app, m, run);
    let ws = m
        .model
        .panes
        .iter()
        .find(|p| p.id == run.pane)
        .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace))
        .map(|w| w.display_name().to_string())
        .unwrap_or_default();
    let name = run.name.clone().unwrap_or_else(|| run.harness.clone());
    let place = if app.machines.len() > 1 {
        format!("{}/{ws}", m.label)
    } else {
        ws
    };
    let title = format!(
        " {} {name} · {place} {glyph} ",
        crate::draw::harness_icon(&run.harness)
    );
    g.put_str(r.x + 1, r.y, &title, t.bold(color), r.w.saturating_sub(2));
    let inner_h = r.h.saturating_sub(2) as usize;
    for (k, line) in tile_body(app, v, id, inner_h).iter().enumerate() {
        g.put_str(
            r.x + 1,
            r.y + 1 + k as u16,
            line,
            t.text(),
            r.w.saturating_sub(2),
        );
    }
}

#[cfg(test)]
#[path = "fleet_tests.rs"]
mod tests;
