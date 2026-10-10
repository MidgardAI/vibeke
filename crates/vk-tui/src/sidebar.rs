//! Sidebar options (08 §2): `[[ui.sidebar.token]]` rules, the pulsing working glyph
//! (`ui.animate`), width (`width`/`min_width`/`max_width`/`auto_width`, drag the border to
//! resize, saved per client), task workspaces nested under their source repo
//! (`ui.sidebar.nest_tasks`) and the collapsed urgency rail.
//!
//! - **Token rules** match an agent row by `harness`, `state` (`working`, `idle`, `done`,
//!   `needs_approval`, `needs_answer`, `error`, `rate_limited`, `starting`, `exited`, `unknown`)
//!   and/or `regex` (against the row's name and label); every given matcher must hold. The first
//!   matching rule wins: `hide = true` drops the row, `label` replaces the name, `color`
//!   (`#rrggbb`, an ANSI index or a theme token: red, yellow, green, blue, accent, muted, fg)
//!   recolours it.
//! - **Pulse**: a working agent's `●` alternates bold/dim twice a second while `ui.animate` is
//!   on (repaints ride on the working age label's deadline, so no extra wakeup class).
//! - **Width**: `auto_width` grows the sidebar to fit the widest row (never below `width`),
//!   clamped to `min_width..=max_width`. Dragging the border sets a manual width, saved in
//!   `<state>/<session>/sidebar-<client>.json` and restored on the next attach.
//! - **Rail**: with the sidebar collapsed (`prefix+b`, `collapsed = true`), a 2-column rail
//!   (glyph + border) shows one urgency glyph per workspace; clicking a glyph focuses that
//!   workspace, clicking the top cell expands the sidebar.

use crate::app::App;
use crate::draw::SideRow;
use crate::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use crate::screen::{Grid, Rect as SRect};
use crate::time::Instant;
use std::path::PathBuf;
use vk_proto::model::*;
use vk_proto::render::{Color, Style};

/// Columns the collapsed rail takes (glyph + border).
pub const RAIL_W: u16 = 2;

#[derive(Debug, Default, Clone)]
pub struct State {
    /// Width set by dragging the border (wins over `auto_width`).
    pub manual: Option<u16>,
    /// Border drag in progress.
    pub dragging: bool,
    /// Tests and embedders: no rail when collapsed.
    pub hide_rail: bool,
    /// Where the manual width is saved.
    pub path: Option<PathBuf>,
}

/// The state name a token rule's `state` matcher compares with.
pub fn state_name(app: &App, m: &crate::app::Machine, r: &AgentRun) -> &'static str {
    let (g, _, _, _) = crate::draw::run_state(app, m, r);
    match g.as_str() {
        "⚠" => "needs_approval",
        "?" => "needs_answer",
        "●" => "working",
        "✓" => "done",
        "○" => "idle",
        "✗" => "error",
        "⏸" => "rate_limited",
        "◌" => "starting",
        "⊘" => "exited",
        _ => "unknown",
    }
}

/// The first token rule matching a run.
pub fn token_for<'a>(
    app: &'a App,
    m: &crate::app::Machine,
    r: &AgentRun,
    text: &str,
) -> Option<&'a vk_config::SidebarToken> {
    let st = state_name(app, m, r);
    app.config.ui.sidebar.token.iter().find(|t| {
        let mm = &t.matcher;
        if mm.harness.is_none() && mm.state.is_none() && mm.regex.is_none() {
            return false;
        }
        mm.harness.as_ref().is_none_or(|h| *h == r.harness)
            && mm.state.as_ref().is_none_or(|s| s == st)
            && mm
                .regex
                .as_ref()
                .is_none_or(|re| regex::Regex::new(re).is_ok_and(|re| re.is_match(text)))
    })
}

/// Parse a token colour.
pub fn color(app: &App, s: &str) -> Option<Color> {
    let t = &app.theme;
    if let Some(hex) = s.strip_prefix('#')
        && hex.len() == 6
    {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    if let Ok(i) = s.parse::<u8>() {
        return Some(Color::Indexed(i));
    }
    Some(match s {
        "red" => t.red,
        "yellow" => t.yellow,
        "green" => t.green,
        "blue" => t.blue,
        "accent" => t.accent,
        "muted" => t.muted,
        "fg" => t.fg,
        _ => return None,
    })
}

/// A hidden run (`hide = true`).
pub fn hidden(app: &App, mi: usize, r: &AgentRun) -> bool {
    let m = &app.machines[mi];
    let name = r.name.clone().unwrap_or_else(|| r.harness.clone());
    let (_, label, _, _) = crate::draw::run_state(app, m, r);
    token_for(app, m, r, &format!("{name} {label}")).is_some_and(|t| t.hide)
}

/// The displayed name and its style after token rules.
pub fn styled_name(app: &App, mi: usize, r: &AgentRun, label: &str) -> (String, Style) {
    let m = &app.machines[mi];
    // Rules match the name or harness as before; the session title is only for display.
    let name = r.name.clone().unwrap_or_else(|| r.harness.clone());
    let shown = crate::draw::truncate(r.label(), crate::draw::RUN_LABEL_MAX);
    match token_for(app, m, r, &format!("{name} {label}")) {
        Some(tok) => (
            tok.label.clone().unwrap_or(shown),
            tok.color
                .as_deref()
                .and_then(|c| color(app, c))
                .map(|c| app.theme.s(c))
                .unwrap_or_else(|| app.theme.text()),
        ),
        None => (shown, app.theme.text()),
    }
}

/// Pulse phase for a working glyph: true = bright.
pub fn pulse_on(app: &App) -> bool {
    !app.config.ui.animate || (crate::drafts::now_ms() / 500) % 2 == 0
}

/// Style of a working glyph this frame.
pub fn working_style(app: &App, base: Style) -> Style {
    if pulse_on(app) {
        base
    } else {
        app.theme.s(app.theme.muted)
    }
}

pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if !app.config.ui.animate || !app.sidebar {
        return;
    }
    let working = app.machines.iter().any(|m| {
        m.model
            .runs
            .iter()
            .any(|r| r.execution.value == Execution::Working)
    });
    if working {
        let ms = 500 - (crate::drafts::now_ms().rem_euclid(500)) as u64;
        // Same class as the working age label it decorates.
        d.redraw("ages", now + crate::time::Duration::from_millis(ms.max(1)));
    }
}

// ---- nested task workspaces --------------------------------------------------------------------

fn source_ws<'a>(m: &'a crate::app::Machine, w: &Workspace) -> Option<&'a Workspace> {
    let tid = w.task.as_ref()?;
    let task = m.model.tasks.iter().find(|t| &t.id == tid)?;
    let root = task.repo_root.trim_end_matches('/');
    m.model
        .workspaces
        .iter()
        .find(|x| x.id != w.id && x.task.is_none() && x.root_path.trim_end_matches('/') == root)
}

/// A task workspace drawn under its source repo's workspace instead of at the top level.
pub fn nested_elsewhere(app: &App, mi: usize, w: &Workspace) -> bool {
    app.config.ui.sidebar.nest_tasks && source_ws(&app.machines[mi], w).is_some()
}

/// Task workspaces nested under `w`.
pub fn task_children<'a>(app: &'a App, mi: usize, w: &Workspace) -> Vec<&'a Workspace> {
    if !app.config.ui.sidebar.nest_tasks {
        return vec![];
    }
    let m = &app.machines[mi];
    m.model
        .workspaces
        .iter()
        .filter(|x| source_ws(m, x).is_some_and(|s| s.id == w.id))
        .collect()
}

// ---- width -------------------------------------------------------------------------------------

fn bounds(app: &App) -> (u16, u16) {
    let s = &app.config.ui.sidebar;
    let lo = s.min_width.max(8);
    (lo, s.max_width.max(lo))
}

/// The width the sidebar should have now.
pub fn desired_width(app: &App, rows: &[SideRow]) -> u16 {
    let (lo, hi) = bounds(app);
    if let Some(w) = app.ux.sidebar.manual {
        return w.clamp(lo, hi);
    }
    let base = app.config.ui.sidebar.width;
    if !app.config.ui.sidebar.auto_width {
        return base.clamp(lo, hi);
    }
    let widest = rows
        .iter()
        .map(|r| {
            r.segs
                .iter()
                .map(|(s, _)| unicode_width::UnicodeWidthStr::width(s.as_str()) as u16)
                .sum::<u16>()
                + 1
        })
        .max()
        .unwrap_or(0);
    widest.max(base).clamp(lo, hi)
}

/// Recompute `sidebar_w` (once per frame, before drawing).
pub fn fit(app: &mut App) {
    if !app.sidebar {
        return;
    }
    let rows = crate::draw::sidebar_rows(app);
    let w = desired_width(app, &rows);
    if w != app.sidebar_w {
        app.sidebar_w = w;
        app.prev = Grid::new(0, 0);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn width_path(session: &str, client: &str) -> PathBuf {
    crate::pending::default_dir(session).join(format!("sidebar-{client}.json"))
}

pub fn load(app: &mut App, path: PathBuf) {
    if let Ok(t) = std::fs::read_to_string(&path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
        && let Some(w) = v["width"].as_u64()
    {
        app.ux.sidebar.manual = Some(w as u16);
    }
    app.ux.sidebar.path = Some(path);
}

fn save(app: &App) {
    if let (Some(p), Some(w)) = (&app.ux.sidebar.path, app.ux.sidebar.manual) {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(p, serde_json::json!({"width": w}).to_string());
    }
}

// ---- rail ----------------------------------------------------------------------------------------

/// Columns the rail takes now (0 when the sidebar is open or the rail is off).
pub fn rail_w(app: &App) -> u16 {
    if app.sidebar || app.ux.sidebar.hide_rail {
        0
    } else {
        RAIL_W
    }
}

/// First column of the rail.
pub fn rail_x(app: &App) -> u16 {
    if crate::chrome::sidebar_right(app) {
        app.size.0.saturating_sub(RAIL_W)
    } else {
        0
    }
}

/// One rail entry per workspace (all machines): (machine, workspace, glyph, colour).
pub fn rail_entries(app: &App) -> Vec<(usize, String, &'static str, Color)> {
    let t = &app.theme;
    let mut v = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        for w in &m.model.workspaces {
            let urg = m
                .model
                .runs
                .iter()
                .filter(|r| {
                    m.model
                        .panes
                        .iter()
                        .any(|p| p.id == r.pane && p.workspace == w.id)
                })
                .map(|r| crate::draw::urgency(app, m, r))
                .max()
                .unwrap_or(0);
            let (g, c) = match urg {
                8 => ("⚠", t.red),
                7 => ("?", t.yellow),
                6 => ("✗", t.red),
                5 => ("⏸", t.yellow),
                4 => ("✓", t.green),
                3 => ("●", t.accent),
                _ => ("·", t.muted),
            };
            v.push((mi, w.id.clone(), g, c));
        }
    }
    v
}

pub fn draw_rail(app: &App, g: &mut Grid) {
    if rail_w(app) == 0 {
        return;
    }
    let t = app.theme;
    let x = rail_x(app);
    let right = crate::chrome::sidebar_right(app);
    let (gx, bx) = if right { (x + 1, x) } else { (x, x + 1) };
    g.fill(
        SRect {
            x,
            y: 0,
            w: RAIL_W,
            h: app.size.1,
        },
        t.text(),
    );
    g.put_str(gx, 0, "≡", t.bold(t.accent), 1);
    let focused = app.m().focus.workspace.clone();
    for (i, (mi, ws, glyph, c)) in rail_entries(app).iter().enumerate() {
        let y = i as u16 + 1;
        if y >= app.size.1 {
            break;
        }
        let st = if *mi == app.cur && Some(ws) == focused.as_ref() {
            Style {
                bg: t.selection,
                ..t.bold(*c)
            }
        } else {
            t.bold(*c)
        };
        g.put_str(gx, y, glyph, st, 1);
    }
    for y in 0..app.size.1 {
        g.put_str(bx, y, "│", t.border(false), 1);
    }
}

pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    if let Some(sx) = crate::chrome::sidebar_x(app) {
        let w = app.sidebar_w;
        if crate::updates::badge(app).is_some()
            && me.row == app.size.1.saturating_sub(1)
            && me.column >= sx
            && me.column < sx + w
        {
            if matches!(me.kind, MouseEventKind::Down(CtButton::Left)) {
                crate::updates::action(app, "update");
            }
            return true;
        }
    }
    // Rail clicks.
    if rail_w(app) > 0 {
        let x = rail_x(app);
        if me.column >= x && me.column < x + RAIL_W {
            if let MouseEventKind::Down(CtButton::Left) = me.kind {
                if me.row == 0 {
                    app.sidebar = true;
                    app.prev = Grid::new(0, 0);
                } else if let Some((mi, ws, _, _)) =
                    rail_entries(app).get(me.row as usize - 1).cloned()
                {
                    crate::nav::focus_workspace(app, mi, &ws);
                }
            }
            return true;
        }
        return false;
    }
    // Border drag.
    let Some(bx) = crate::chrome::sidebar_border_x(app) else {
        return false;
    };
    match me.kind {
        MouseEventKind::Down(CtButton::Left) if me.column == bx => {
            app.ux.sidebar.dragging = true;
            true
        }
        MouseEventKind::Drag(CtButton::Left) if app.ux.sidebar.dragging => {
            let (lo, hi) = bounds(app);
            let w = if crate::chrome::sidebar_right(app) {
                app.size.0.saturating_sub(me.column + 1)
            } else {
                me.column
            };
            let w = w.clamp(lo, hi);
            if app.ux.sidebar.manual != Some(w) {
                app.ux.sidebar.manual = Some(w);
                app.sidebar_w = w;
                app.prev = Grid::new(0, 0);
                app.dirty = true;
            }
            true
        }
        MouseEventKind::Up(CtButton::Left) if app.ux.sidebar.dragging => {
            app.ux.sidebar.dragging = false;
            save(app);
            app.send_view_hints(true);
            true
        }
        _ => false,
    }
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action == "sidebar_width_reset" {
        app.ux.sidebar.manual = None;
        if let Some(p) = &app.ux.sidebar.path {
            let _ = std::fs::remove_file(p);
        }
        app.toast("sidebar width follows the config again");
        return true;
    }
    false
}

#[cfg(test)]
#[path = "sidebar_tests.rs"]
mod tests;
