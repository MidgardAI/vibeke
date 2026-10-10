//! Frame composition (03 §6.2): chrome (sidebar, tab bar, borders, popups) + pane cells.

use crate::app::{App, Mode, PaneBuf, PrefixState};
use crate::screen::{Grid, Rect as SRect};
use vk_proto::layout::Rect;
use vk_proto::model::*;
use vk_proto::render::{CursorShape, Style, attr};

/// One sidebar line: text segments with styles, and the pane it targets when clicked/entered.
#[derive(Default)]
pub struct SideRow {
    pub segs: Vec<(String, Style)>,
    pub target: Option<(usize, String)>,
    pub focused: bool,
    /// A workspace group row (machine, group id): selectable in navigate mode, collapses on
    /// enter/click (08 §2.1, M4).
    pub group: Option<(usize, String)>,
    /// A preview row (machine, preview id) of the Previews section; with `chips`, a line of
    /// its expanded action chips instead (06 B2).
    pub preview: Option<(usize, String)>,
    /// Chips on this line and their x ranges, relative to the sidebar's left edge.
    pub chips: Vec<(crate::preview_manager::Chip, u16, u16)>,
}

impl SideRow {
    /// Navigate mode can select it (a pane target, a group or a preview row).
    pub fn selectable(&self) -> bool {
        self.target.is_some()
            || self.group.is_some()
            || (self.preview.is_some() && self.chips.is_empty())
    }

    /// What navigate mode selects on this row (pane, group or preview id).
    fn selection(&self) -> Option<(usize, String)> {
        self.target
            .clone()
            .or(self.group.clone())
            .or(self.preview.clone().filter(|_| self.chips.is_empty()))
    }
}

pub fn harness_icon(h: &str) -> &'static str {
    match h {
        "claude" => "✳",
        "codex" => "◎",
        "pi" | "omp" => "π",
        _ => "•",
    }
}

/// Repaint when an age on screen changes: a working agent's `working · 12s` (sidebar, peek,
/// tiles) and, while one is open, the inbox / desk / gallery / pending-operations ages. Idle,
/// done and waiting agents show static labels and arm nothing (spec 10 §1.3.1).
pub(crate) fn deadlines(app: &App, now: std::time::Instant, d: &mut crate::deadline::Deadlines) {
    use crate::app::Popup;
    let wall = vk_now();
    let mut next: Option<u64> = None;
    for m in &app.machines {
        for r in &m.model.runs {
            if matches!(r.execution.value, Execution::Working) {
                let ms = crate::deadline::age_change_in(wall - r.execution.since_ms);
                next = Some(next.map_or(ms, |n| n.min(ms)));
            }
        }
    }
    if matches!(
        app.mode,
        Mode::Popup(Popup::Inbox | Popup::Desk | Popup::Gallery | Popup::PendingOps { .. })
    ) {
        let ms = crate::deadline::age_change_in(wall.rem_euclid(1_000));
        next = Some(next.map_or(ms, |n| n.min(ms)));
    }
    if let Some(ms) = next {
        d.redraw("ages", now + std::time::Duration::from_millis(ms));
    }
}

/// (glyph, label, colour, inferred)
pub fn run_state(
    app: &App,
    m: &crate::app::Machine,
    r: &AgentRun,
) -> (String, String, vk_proto::render::Color, bool) {
    let t = &app.theme;
    let inferred = !matches!(
        r.execution.source,
        StateSource::Structured | StateSource::SelfReport
    ) || r.execution.confidence < 0.8;
    let open: Vec<&Interaction> = m
        .model
        .interactions
        .iter()
        .filter(|i| i.run == r.id && i.status == InteractionStatus::Open)
        .collect();
    if let Some(i) = open
        .iter()
        .find(|i| i.kind == InteractionKind::Approval || i.kind == InteractionKind::PlanReview)
    {
        let what = i
            .action
            .as_ref()
            .map(|a| a.summary.clone())
            .unwrap_or_else(|| i.title.clone());
        return (
            "⚠".into(),
            format!("approve: {what}"),
            t.red,
            i.source == StateSource::Screen,
        );
    }
    if let Some(i) = open
        .iter()
        .find(|i| matches!(i.kind, InteractionKind::Question | InteractionKind::Picker))
    {
        let q = i
            .questions
            .first()
            .map(|q| q.prompt.clone())
            .unwrap_or_else(|| i.title.clone());
        return (
            "?".into(),
            format!("ask: {q}"),
            t.yellow,
            i.source == StateSource::Screen,
        );
    }
    let age = |since: i64| {
        let s = ((vk_now() - since) / 1000).max(0);
        if s < 60 {
            format!("{s}s")
        } else if s < 3600 {
            format!("{}m", s / 60)
        } else {
            format!("{}h", s / 3600)
        }
    };
    match r.execution.value {
        Execution::Working => (
            "●".into(),
            format!("working · {}", age(r.execution.since_ms)),
            t.accent,
            inferred,
        ),
        Execution::Idle => {
            let seen = *m.seen.get(&r.pane).unwrap_or(&0);
            if r.done_rev > seen {
                ("✓".into(), "done".into(), t.green, inferred)
            } else {
                ("○".into(), "idle".into(), t.muted, inferred)
            }
        }
        Execution::Starting => ("◌".into(), "starting".into(), t.muted, inferred),
        Execution::Error => (
            "✗".into(),
            format!("error: {}", r.execution.detail.clone().unwrap_or_default()),
            t.red,
            inferred,
        ),
        Execution::RateLimited => ("⏸".into(), "rate limited".into(), t.yellow, inferred),
        Execution::Exited => ("⊘".into(), "exited".into(), t.muted, inferred),
        Execution::Unknown => ("·".into(), "—".into(), t.muted, inferred),
    }
}

fn vk_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn urgency(app: &App, m: &crate::app::Machine, r: &AgentRun) -> u8 {
    let (g, _, _, _) = run_state(app, m, r);
    match g.as_str() {
        "⚠" => 8,
        "?" => 7,
        "✗" => 6,
        "⏸" => 5,
        "✓" => 4,
        "●" => 3,
        "○" => 2,
        _ => 1,
    }
}

/// Sidebar isolation glyph (13 §3): `sb`/`ct`/`vm` (configurable), plus the network profile
/// when it is not the default `dev`. `None` for host panes.
pub fn isolation_glyph(app: &App, iso: &Isolation) -> Option<String> {
    let g = &app.config.ui.sidebar.isolation_glyphs;
    let base = match iso.level {
        IsolationLevel::Host => g.host.clone(),
        IsolationLevel::Sandbox => g.sandbox.clone(),
        IsolationLevel::Container => g.container.clone(),
        IsolationLevel::Vm => g.vm.clone(),
    };
    if base.is_empty() {
        return None;
    }
    Some(match iso.network.as_str() {
        "" | "dev" => base,
        n => format!("{base}·{n}"),
    })
}

fn agent_row(app: &App, mi: usize, r: &AgentRun, indent: &str) -> SideRow {
    let m = &app.machines[mi];
    let t = &app.theme;
    let (g, label, color, inferred) = run_state(app, m, r);
    // `[[ui.sidebar.token]]` rules rename/recolour the name (08 §2.4).
    let (name, name_style) = crate::sidebar::styled_name(app, mi, r, &label);
    let mut segs = vec![
        (format!("{indent}{} ", harness_icon(&r.harness)), t.s(color)),
        (format!("{name} "), name_style),
    ];
    // A run is as contained as its pane (13 §2.2): enforced inside sandbox/container/vm.
    let iso = m
        .model
        .panes
        .iter()
        .find(|p| p.id == r.pane)
        .map(|p| p.isolation.clone())
        .unwrap_or_default();
    if let Some(gl) = isolation_glyph(app, &iso) {
        segs.push((format!("{gl} "), t.s(t.accent)));
    }
    if r.yolo {
        if iso.is_contained() {
            segs.push(("YOLO ".into(), app.theme.bold(t.yellow)));
        } else {
            segs.push(("YOLO·HOST ".into(), app.theme.bold(t.red)));
        }
    }
    let glyph = if inferred
        && app.config.ui.sidebar.show_state_source != vk_config::ShowStateSource::Never
    {
        format!("{g}~")
    } else {
        g
    };
    // A working agent's glyph pulses with `ui.animate` (08 §2.2).
    let gst = if r.execution.value == Execution::Working && glyph.starts_with('●') {
        crate::sidebar::working_style(app, t.bold(color))
    } else {
        t.bold(color)
    };
    segs.push((format!("{glyph} "), gst));
    // A tracked task's review label is its own small marker; it never replaces "done".
    if let Some(tid) = crate::tasks::task_for_run(app, mi, r) {
        let label = m
            .model
            .tasks
            .iter()
            .find(|x| x.id == tid)
            .and_then(|x| x.review_label.as_deref());
        let (mark, tone) = crate::tasks::sidebar_marker(label);
        let c = match tone {
            1 => t.accent,
            2 => t.yellow,
            3 => t.green,
            _ => t.muted,
        };
        segs.push((format!("{mark} "), t.s(c)));
    }
    segs.push((label, t.dim()));
    // Another agent is editing the same files in this checkout (05 §10; `~` = same directory).
    if let Some((mark, rank)) = crate::collision::agent_marker(app, mi, &r.pane) {
        let c = match rank {
            3 => t.red,
            2 => t.yellow,
            _ => t.muted,
        };
        segs.push((format!(" {mark}"), t.bold(c)));
    }
    // A plugin's status line for this run (`agent.view.set`).
    if let Some(v) = crate::plugins::agent_view(app, mi, &r.id) {
        let c = match v.tone.as_str() {
            "ok" => t.green,
            "warn" => t.yellow,
            "error" => t.red,
            _ => t.accent,
        };
        segs.push((format!(" ▸ {}", v.text), t.s(c)));
    }
    crate::plugin_ui::decorate(app, mi, &r.pane, &mut segs);
    // Screenshots captured since the gallery was last opened for this agent (06 B7/B8).
    if let Some(n) = crate::gallery::badge(app, mi, &r.pane) {
        segs.push((format!(" 📷{n}"), t.s(t.accent)));
    }
    let focused = mi == app.cur && app.m().focus.pane.as_deref() == Some(&r.pane);
    SideRow {
        segs,
        target: Some((mi, r.pane.clone())),
        focused,
        ..Default::default()
    }
}

/// Tests: the text of one agent row.
#[cfg(test)]
pub fn agent_row_text(app: &App, mi: usize, run: &str) -> String {
    let r = app.machines[mi]
        .model
        .runs
        .iter()
        .find(|r| r.id == run)
        .expect("run");
    agent_row(app, mi, r, "")
        .segs
        .into_iter()
        .map(|(s, _)| s)
        .collect()
}

pub fn sidebar_rows(app: &App) -> Vec<SideRow> {
    let t = &app.theme;
    let mut rows = Vec::new();
    // Needs-you section (08 §2.1): runs with open interactions or errors, oldest first.
    if app.config.ui.sidebar.attention_section {
        let mut need: Vec<(i64, usize, &AgentRun)> = Vec::new();
        for (mi, m) in app.machines.iter().enumerate() {
            for r in &m.model.runs {
                let opened = m
                    .model
                    .interactions
                    .iter()
                    .filter(|i| i.run == r.id && i.status == InteractionStatus::Open)
                    .map(|i| i.opened_at_ms)
                    .min();
                if let Some(o) = opened {
                    need.push((o, mi, r));
                } else if matches!(r.execution.value, Execution::Error | Execution::RateLimited) {
                    need.push((r.execution.since_ms, mi, r));
                }
            }
        }
        if !need.is_empty() {
            need.sort_by_key(|x| x.0);
            rows.push(SideRow {
                segs: vec![("─ needs you ─".into(), t.bold(t.red))],
                target: None,
                focused: false,
                ..Default::default()
            });
            for (_, mi, r) in need {
                if !crate::sidebar::hidden(app, mi, r) {
                    rows.push(agent_row(app, mi, r, " "));
                }
            }
            rows.push(SideRow {
                segs: vec![],
                target: None,
                focused: false,
                ..Default::default()
            });
        }
    }
    let multi = app.machines.len() > 1;
    for (mi, m) in app.machines.iter().enumerate() {
        if multi || !m.connected() {
            // `● devbox 23ms`, `◐ devbox degraded 512ms`, `○ devbox offline · last seen 4m ago`.
            let (status, degraded) = crate::remote_view::status_suffix(app, mi);
            let dot = if degraded {
                ("◐ ", t.yellow)
            } else if m.connected() {
                ("● ", t.green)
            } else {
                ("○ ", t.muted)
            };
            rows.push(SideRow {
                segs: vec![
                    (dot.0.into(), t.s(dot.1)),
                    (m.label.clone(), t.bold(t.fg)),
                    (status, t.dim()),
                ],
                target: None,
                focused: false,
                ..Default::default()
            });
        }
        crate::groups::push_machine(app, mi, &mut rows);
    }
    // Native plugin sidebar sections (07 §7.4).
    crate::plugin_ui::push_sidebar(app, &mut rows);
    let pinned: Vec<(usize, &Pane)> = app
        .machines
        .iter()
        .enumerate()
        .flat_map(|(mi, m)| {
            m.model
                .panes
                .iter()
                .filter(|p| p.pinned)
                .map(move |p| (mi, p))
        })
        .collect();
    if !pinned.is_empty() {
        rows.push(SideRow {
            segs: vec![("─ pinned ─".into(), t.dim())],
            target: None,
            focused: false,
            ..Default::default()
        });
        for (mi, p) in pinned {
            rows.push(SideRow {
                segs: vec![(format!("  {}", p.display_title()), t.text())],
                target: Some((mi, p.id.clone())),
                focused: false,
                ..Default::default()
            });
        }
    }
    // Navigate-mode `/` filter (08 §6.1): matching selectable rows only.
    if crate::navkeys::filter(app).is_some() {
        crate::navkeys::apply_filter(app, &mut rows);
        return rows;
    }
    // Previews (06 B2): last section, so `browser::preview_hit` can map rows back.
    let previews = crate::browser::preview_entries(app);
    if !previews.is_empty() {
        rows.push(SideRow {
            segs: vec![("─ previews ─".into(), t.dim())],
            target: None,
            focused: false,
            ..Default::default()
        });
        let width = app.sidebar_w.saturating_sub(1);
        for (mi, p) in &previews {
            let key = (*mi, p.id.clone());
            rows.push(SideRow {
                segs: crate::browser::preview_segs(app, *mi, p),
                target: None,
                focused: false,
                preview: Some(key.clone()),
                ..Default::default()
            });
            // The selected row's action chips, right under it.
            if app.preview_mgr.expanded.as_ref() == Some(&key) {
                for (segs, chips) in crate::preview_manager::chip_rows(app, *mi, p, width) {
                    rows.push(SideRow {
                        segs,
                        preview: Some(key.clone()),
                        chips,
                        ..Default::default()
                    });
                }
            }
        }
    }
    rows
}

/// One workspace row plus its agent (and optional shell pane) rows, indented by group depth.
/// Task workspaces nest under their source repo's workspace (`ui.sidebar.nest_tasks`).
pub(crate) fn workspace_rows(
    app: &App,
    mi: usize,
    w: &Workspace,
    depth: usize,
    rows: &mut Vec<SideRow>,
) {
    if crate::sidebar::nested_elsewhere(app, mi, w) {
        return;
    }
    workspace_rows_at(app, mi, w, depth, rows);
}

fn workspace_rows_at(app: &App, mi: usize, w: &Workspace, depth: usize, rows: &mut Vec<SideRow>) {
    let t = &app.theme;
    let m = &app.machines[mi];
    let pad = "  ".repeat(depth);
    let runs: Vec<&AgentRun> = m
        .model
        .runs
        .iter()
        .filter(|r| {
            m.model
                .panes
                .iter()
                .any(|p| p.id == r.pane && p.workspace == w.id)
        })
        .collect();
    let focused_ws = mi == app.cur && m.focus.workspace.as_deref() == Some(&w.id);
    let unread = m
        .model
        .panes
        .iter()
        .any(|p| p.workspace == w.id && (p.unread || p.marked_unread));
    let badge = runs.iter().map(|r| urgency(app, m, r)).max().unwrap_or(0);
    let (bg, bc) = match badge {
        8 => ("⚠", t.red),
        7 => ("?", t.yellow),
        6 => ("✗", t.red),
        4 => ("✓", t.green),
        3 => ("●", t.accent),
        _ => ("", t.muted),
    };
    let target = m
        .model
        .tabs
        .iter()
        .find(|tb| tb.workspace == w.id)
        .and_then(|tb| {
            tb.focused_pane
                .clone()
                .or_else(|| tb.layout.panes().first().cloned())
        });
    let marker = if w.task.is_some() { "◆ " } else { "" };
    let name_style = if unread { t.bold(t.fg) } else { t.text() };
    let mut segs = vec![
        (
            format!("{pad}{}{marker}", if focused_ws { "▸ " } else { "  " }),
            t.s(t.accent),
        ),
        (w.display_name().to_string(), name_style),
    ];
    if let Some(b) = w.branch.as_deref() {
        segs.push((format!(" ⎇ {b}"), t.dim()));
    }
    // Task workspaces: missing checkout, cached PR status (05 §4).
    if let Some(tid) = w.task.as_deref() {
        segs.extend(crate::taskbadge::segs(app, mi, tid));
    }
    let ws_panes: Vec<&str> = m
        .model
        .panes
        .iter()
        .filter(|p| p.workspace == w.id)
        .map(|p| p.id.as_str())
        .collect();
    if let Some(p) = crate::osc::progress_in(app, mi, &ws_panes) {
        let (bar, st) = crate::osc::progress_bar(app, p);
        segs.push((format!(" {bar}"), st));
    }
    if !bg.is_empty() {
        let working = runs
            .iter()
            .filter(|r| r.execution.value == Execution::Working)
            .count();
        let n = if bg == "●" && working > 1 {
            working.to_string()
        } else {
            String::new()
        };
        segs.push((format!(" {bg}{n}"), t.bold(bc)));
    }
    rows.push(SideRow {
        segs,
        target: target.map(|p| (mi, p)),
        focused: false,
        ..Default::default()
    });
    for r in &runs {
        if !crate::sidebar::hidden(app, mi, r) {
            rows.push(agent_row(app, mi, r, &format!("{pad}    ")));
        }
    }
    // "2 agents editing src/auth.ts" (05 §10); a dim `~` hint for the same-directory level.
    for (line, rank) in crate::collision::sidebar_lines(app, mi, &ws_panes) {
        let (mark, c) = match rank {
            3 => ("⚠", t.red),
            2 => ("⚠", t.yellow),
            _ => ("~", t.muted),
        };
        rows.push(SideRow {
            segs: vec![(format!("{pad}    {mark} {line}"), t.s(c))],
            target: crate::collision::sidebar_target(app, mi, &ws_panes).map(|p| (mi, p)),
            focused: false,
            ..Default::default()
        });
    }
    for c in crate::sidebar::task_children(app, mi, w) {
        workspace_rows_at(app, mi, c, depth + 1, rows);
    }
    if app.config.ui.sidebar.show_shell_panes {
        for p in m
            .model
            .panes
            .iter()
            .filter(|p| p.workspace == w.id && !runs.iter().any(|r| r.pane == p.id))
        {
            let focused = mi == app.cur && m.focus.pane.as_deref() == Some(&p.id);
            let mut segs = vec![(format!("{pad}    {}", p.display_title()), t.dim())];
            if let Some(gl) = isolation_glyph(app, &p.isolation) {
                segs.push((format!(" {gl}"), t.s(t.accent)));
            }
            if let Some(pr) = crate::osc::progress_in(app, mi, &[p.id.as_str()]) {
                let (bar, st) = crate::osc::progress_bar(app, pr);
                segs.push((format!(" {bar}"), st));
            }
            rows.push(SideRow {
                segs,
                target: Some((mi, p.id.clone())),
                focused,
                ..Default::default()
            });
        }
    }
}

/// Navigate-mode selectable rows, in order: pane targets, and group rows as `(machine, group
/// id)` (`crate::groups::navigate_key` handles keys on those before anything else sees them).
pub fn sidebar_targets(app: &App) -> Vec<(usize, String)> {
    sidebar_rows(app)
        .into_iter()
        .filter_map(|r| r.selection())
        .collect()
}

pub fn sidebar_index_of_focus(app: &App) -> usize {
    let f = app.m().focus.pane.clone();
    sidebar_targets(app)
        .iter()
        .position(|(mi, p)| *mi == app.cur && Some(p) == f.as_ref())
        .unwrap_or(0)
}

pub fn sidebar_hit(app: &App, y: u16) -> Option<(usize, String)> {
    let rows = sidebar_rows(app);
    rows.into_iter()
        .nth(y.checked_sub(1)? as usize)
        .and_then(|r| r.target)
}

/// Tab bar entries with their x ranges (the visible ones when the bar overflows).
fn tab_entries(app: &App) -> Vec<(Tab, String, u16, u16)> {
    tab_layout(app).entries
}

/// The tab bar laid out for the main area: visible entries and overflow arrows (08 §3).
pub fn tab_layout(app: &App) -> crate::tabbar::Laid {
    let m = app.m();
    let Some(ws) = &m.focus.workspace else {
        return Default::default();
    };
    let (mx, mw) = crate::chrome::main_x(app);
    let mut raw = Vec::new();
    for t in m.model.tabs.iter().filter(|t| &t.workspace == ws) {
        let title = t.title.clone().unwrap_or_else(|| {
            let fp = t
                .focused_pane
                .clone()
                .or_else(|| t.layout.panes().first().cloned());
            let run = fp
                .as_ref()
                .and_then(|p| m.model.runs.iter().find(|r| &r.pane == p));
            match run {
                Some(r) => truncate(r.label(), RUN_LABEL_MAX),
                None => fp
                    .and_then(|p| m.model.panes.iter().find(|x| x.id == p))
                    .map(|p| p.display_title().to_string())
                    .unwrap_or_else(|| "shell".into()),
            }
        });
        let glyph = {
            let panes = t.layout.panes();
            let urg = m
                .model
                .runs
                .iter()
                .filter(|r| panes.contains(&r.pane))
                .map(|r| urgency(app, m, r))
                .max()
                .unwrap_or(0);
            match urg {
                8 => "⚑",
                7 => "?",
                4 => "✓",
                _ => "",
            }
        };
        let zoom = if t.zoomed_pane.is_some() { " Z" } else { "" };
        // OSC 9;4 progress of the tab's panes (03 §8).
        let ids = t.layout.panes();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let prog = crate::osc::progress_in(app, app.cur, &ids)
            .map(|p| format!(" {}", crate::osc::progress_bar(app, p).0))
            .unwrap_or_default();
        let sync = crate::sync_input::tab_badge(app, &t.id);
        let label = if app.config.ui.tabs.show_numbers {
            format!(" {}{} {}{zoom}{prog}{sync} ", glyph, t.number, title)
        } else {
            let g = if glyph.is_empty() {
                String::new()
            } else {
                format!("{glyph} ")
            };
            format!(" {g}{title}{zoom}{prog}{sync} ")
        };
        let w = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
        raw.push((t.clone(), label, w));
    }
    crate::tabbar::layout(app, raw, mx + 1, mx + mw)
}

/// First column after the last tab entry.
pub fn tabs_end(app: &App) -> u16 {
    tab_entries(app)
        .last()
        .map(|(_, _, _, b)| *b)
        .unwrap_or(crate::chrome::main_x(app).0 + 1)
}

pub fn tabbar_hit(app: &App, x: u16) -> Option<String> {
    tab_entries(app)
        .into_iter()
        .find(|(_, _, a, b)| x >= *a && x < *b)
        .map(|(t, _, _, _)| t.id)
}

/// Compose the whole frame; returns the host cursor position if a pane cursor should show.
pub fn compose(app: &App, g: &mut Grid) -> Option<(u16, u16, CursorShape)> {
    let t = app.theme;
    let rows = app.size.1;
    // Sidebar, left or right (08 §2.4).
    if let (Some(sx), Some(bx)) = (
        crate::chrome::sidebar_x(app),
        crate::chrome::sidebar_border_x(app),
    ) {
        let w = app.sidebar_w;
        g.fill(
            SRect {
                x: sx,
                y: 0,
                w,
                h: rows,
            },
            t.text(),
        );
        let m = app.m();
        let title = format!(
            " vibeke · {}",
            if app.machines.len() > 1 {
                "all machines".to_string()
            } else {
                m.label.clone()
            }
        );
        g.put_str(sx, 0, &title, t.bold(t.accent), w);
        let nav_sel = if let Mode::Navigate { sel } = app.mode {
            Some(sel)
        } else {
            None
        };
        let mut target_i = 0;
        for (i, r) in sidebar_rows(app).iter().enumerate() {
            let y = i as u16 + 1;
            if y >= rows.saturating_sub(u16::from(crate::updates::badge(app).is_some())) {
                break;
            }
            let selected = r.selectable() && nav_sel == Some(target_i);
            if r.selectable() {
                target_i += 1;
            }
            if selected || r.focused {
                g.fill(SRect { x: sx, y, w, h: 1 }, t.sel(t.fg));
            }
            let mut x = 0;
            for (s, st) in &r.segs {
                let st = if selected || r.focused {
                    Style {
                        bg: t.selection,
                        ..*st
                    }
                } else {
                    *st
                };
                x += g.put_str(sx + x, y, s, st, w.saturating_sub(x));
            }
        }
        crate::preview_ui::draw_thumbs(app, g, sx, w);
        if let Some(badge) = crate::updates::badge(app) {
            g.fill(
                SRect {
                    x: sx,
                    y: rows.saturating_sub(1),
                    w,
                    h: 1,
                },
                t.text(),
            );
            g.put_str(sx, rows.saturating_sub(1), &badge, t.bold(t.accent), w);
        }
        for y in 0..rows {
            g.put_str(bx, y, "│", t.border(false), 1);
        }
    }
    // The collapsed sidebar's urgency rail (08 §2.4).
    crate::sidebar::draw_rail(app, g);
    // Tab bar, top or bottom (08 §3); hidden draws no row.
    let (tx, tw) = crate::chrome::main_x(app);
    let right = right_cluster(app);
    if let Some(ty) = crate::chrome::tab_row(app) {
        g.fill(
            SRect {
                x: tx,
                y: ty,
                w: tw,
                h: 1,
            },
            t.text(),
        );
        let focused_tab = app.m().focus.tab.clone();
        let laid = tab_layout(app);
        for (tab, label, x, _) in &laid.entries {
            let st = if Some(&tab.id) == focused_tab.as_ref() {
                t.rev()
            } else {
                t.dim()
            };
            g.put_str(*x, ty, label, st, (tx + tw).saturating_sub(*x));
        }
        // Overflow arrows and the drag drop marker (08 §3).
        if let Some(x) = laid.left {
            g.put_str(x, ty, "‹", t.bold(t.accent), 1);
        }
        if let Some(x) = laid.right {
            g.put_str(x, ty, "›", t.bold(t.accent), 1);
        }
        if let Some(x) = crate::tabbar::drop_marker(app, &laid) {
            g.put_str(x, ty, "▏", t.bold(t.accent), 1);
        }
        // Preview chips for the focused tab's panes (06 B2).
        crate::browser::draw_chips(app, g, tabs_end(app), tx + tw);
        put_right(g, &right, ty, tx + tw);
    }
    // Status bar (08 §4), when enabled.
    crate::statusbar::draw(app, g);
    // Panes: the tiling, then floating panes on top in z order (08 §5).
    let area = app.pane_area();
    let rects = app.tiled_rects();
    let floats = crate::floats::visible(app);
    let focused = app.m().focus.pane.clone();
    let mut cursor = None;
    if rects.is_empty() && floats.is_empty() {
        let msg = if app.m().connected() {
            "no panes — prefix+c for a new tab, prefix+shift+n for a workspace"
        } else {
            "connecting…"
        };
        g.put_str(
            area.x + 2,
            area.y + 1,
            msg,
            t.dim(),
            area.w.saturating_sub(2),
        );
    }
    draw_borders(app, g, area, &rects, focused.as_deref());
    for (pid, r) in &rects {
        draw_pane_at(app, g, pid, *r, focused.as_deref(), &mut cursor);
        // ⚠ in the top-left corner of a pane whose agent shares files with another (05 §10).
        if let Some(b) = crate::collision::pane_badge(app, app.cur, pid) {
            let c = if crate::collision::pane_rank(app, app.cur, pid) >= 3 {
                t.red
            } else {
                t.yellow
            };
            g.put_str(r.x, r.y, &format!(" {b} "), t.bold(c), 3);
        }
        // 📷 counter in the corner of unfocused panes only (never over the focused agent).
        if focused.as_deref() != Some(pid.as_str())
            && let Some(n) = crate::gallery::badge(app, app.cur, pid)
        {
            let badge = format!(" 📷{n} ");
            let w = unicode_width::UnicodeWidthStr::width(badge.as_str()) as u16;
            g.put_str(r.x + r.w.saturating_sub(w), r.y, &badge, t.s(t.accent), w);
        }
    }
    for f in &floats {
        // A tiled pane's cursor hidden under a float doesn't show through it.
        if cursor.is_some_and(|(x, y, _)| f.outer.contains(x, y)) {
            cursor = None;
        }
        crate::floats::draw_frame(app, g, f, focused.as_deref() == Some(f.pane.as_str()));
        draw_pane_at(app, g, &f.pane, f.inner, focused.as_deref(), &mut cursor);
    }
    // An open popup dims what is under it (08 §5).
    crate::popup_pane::dim(app, g);
    // Plugin overlays and popups on top of everything in the pane area (M5).
    for s in crate::plugins::surfaces(app) {
        if cursor.is_some_and(|(x, y, _)| s.outer.contains(x, y)) {
            cursor = None;
        }
        let on = focused.as_deref() == Some(s.pane.as_str());
        crate::plugins::draw_chrome(app, g, &s, on);
        draw_pane_at(app, g, &s.pane, s.inner, focused.as_deref(), &mut cursor);
    }
    // Hidden tab bar: mode, toasts and notices still show, over the pane area's top-right
    // corner, only while there is something to say.
    if crate::chrome::tab_row(app).is_none() && !right.is_empty() {
        put_right(g, &right, area.y, area.x + area.w);
        if cursor.is_some_and(|(_, y, _)| y == area.y) {
            cursor = None;
        }
    }
    // The prefix menu and resize level cover the pane like a popup: no pane cursor.
    if !matches!(
        app.mode,
        Mode::Normal | Mode::Prefix(PrefixState { menu: false, .. }) | Mode::Navigate { .. }
    ) {
        cursor = None;
    }
    if let Some(c) = crate::popups::draw(app, g) {
        cursor = Some(c);
    }
    if app.gateway.modal() {
        crate::gateway::draw_overlay(app, g);
        cursor = None;
    }
    cursor
}

/// `PREFIX`, or `PREFIX g` inside a key sequence.
pub(crate) fn prefix_badge(p: &crate::app::PrefixState) -> String {
    if p.seq.is_empty() {
        "PREFIX".into()
    } else {
        format!("PREFIX {}", p.seq_text())
    }
}

/// The right side of the tab bar: mode, toasts, connection, notices.
fn right_cluster(app: &App) -> Vec<(String, Style)> {
    let t = app.theme;
    let mut right: Vec<(String, Style)> = Vec::new();
    match &app.mode {
        Mode::Prefix(p) => right.push((format!(" {} ", prefix_badge(p)), t.rev())),
        Mode::Copy(_) => right.push((" COPY ".into(), t.rev())),
        Mode::Navigate { .. } => match &app.ux.nav.filter {
            Some(f) => right.push((
                format!(" NAV /{f}{} ", if app.ux.nav.typing { "▏" } else { "" }),
                t.rev(),
            )),
            None => right.push((" NAV ".into(), t.rev())),
        },
        Mode::Resize => right.push((" RESIZE ".into(), t.rev())),
        _ => {}
    }
    // Synchronized input: bright badge (08 §5).
    if let Some(b) = crate::sync_input::badge(app) {
        right.insert(0, (b, t.rev()));
    }
    if let Some(up) = crate::upload::status(app) {
        right.insert(0, (format!(" {up} "), t.s(t.yellow)));
    }
    if let Some(h) = crate::handoff::status(app) {
        right.insert(0, (format!(" {h} "), t.s(t.accent)));
    }
    // Incoming handoffs waiting (16 §15.2): chrome only, like the elevation notice.
    if let Some(b) = crate::handoff::badge(app) {
        right.insert(0, (b, t.bold(t.accent)));
    }
    if let Some(p) = app.focused_pane()
        && let Some(badge) = crate::osc::exit_badge(app, app.cur, &p, std::time::Instant::now())
    {
        right.insert(0, (badge, t.bold(t.red)));
    }
    if let Some(r) = app.clip.pending.first() {
        right.insert(
            0,
            (
                format!(
                    " ⎘ {} clipboard request — prefix+y ",
                    truncate(&app.machines[r.machine].label, 16)
                ),
                t.bold(t.yellow),
            ),
        );
    }
    // Elevation requests (09 §3.2): a notice in the chrome only, never over a pane.
    if let Some(n) = crate::elevate::notice(app) {
        right.insert(0, (n, t.bold(t.red)));
    }
    let unknown = app.pending_ops.unknown_count();
    if unknown > 0 {
        right.insert(
            0,
            (
                format!(" ⚠ {unknown} outcome(s) unknown — :pending_operations "),
                t.bold(t.yellow),
            ),
        );
    }
    if let Some(toast) = app.toasts.last() {
        right.insert(
            0,
            (format!(" {} ", truncate(&toast.text, 60)), t.s(t.yellow)),
        );
    }
    if let Some(d) = &app.m().model.degraded {
        right.insert(0, (format!(" ⚠ {} ", degraded_label(d)), t.bold(t.red)));
    }
    if !app.m().connected() {
        right.insert(
            0,
            (
                format!(" {} {} ", app.m().label, app.m().status),
                t.bold(t.red),
            ),
        );
    }
    if let Some(dev) = crate::gateway::devices_label(app) {
        right.push((format!(" {dev} "), t.s(t.accent)));
    }
    // A plugin-set window title shows here when it can't go to the outer terminal (M5).
    if !app.config.ui.title_sync
        && let Some(title) = crate::plugins::window_title(app)
    {
        right.insert(0, (format!(" {} ", truncate(title, 40)), t.s(t.accent)));
    }
    right
}

/// The right-cluster entry drawn at (`x`, `y`), as [`compose`] places it: on the tab row, or
/// over the pane area's top-right corner when the tab bar is hidden.
pub fn right_cluster_at(app: &App, x: u16, y: u16) -> Option<String> {
    let right = right_cluster(app);
    let end = match crate::chrome::tab_row(app) {
        Some(ty) if ty == y => {
            let (tx, tw) = crate::chrome::main_x(app);
            tx + tw
        }
        Some(_) => return None,
        None => {
            let area = app.pane_area();
            if right.is_empty() || area.y != y {
                return None;
            }
            area.x + area.w
        }
    };
    let rw: u16 = right
        .iter()
        .map(|(s, _)| unicode_width::UnicodeWidthStr::width(s.as_str()) as u16)
        .sum();
    let mut at = end.saturating_sub(rw);
    for (s, _) in right {
        let w = unicode_width::UnicodeWidthStr::width(s.as_str()) as u16;
        if (at..at + w).contains(&x) {
            return Some(s);
        }
        at += w;
    }
    None
}

/// Draw `right` right-aligned on row `y`, ending before column `end`.
fn put_right(g: &mut Grid, right: &[(String, Style)], y: u16, end: u16) {
    let rw: u16 = right
        .iter()
        .map(|(s, _)| unicode_width::UnicodeWidthStr::width(s.as_str()) as u16)
        .sum();
    let mut x = end.saturating_sub(rw);
    for (s, st) in right {
        x += g.put_str(x, y, s, *st, end.saturating_sub(x));
    }
}

/// One pane's content in `r`: copy mode, a browser pane, or terminal cells (+ badges); sets the
/// host cursor for the focused pane.
fn draw_pane_at(
    app: &App,
    g: &mut Grid,
    pid: &str,
    r: Rect,
    focused: Option<&str>,
    cursor: &mut Option<(u16, u16, CursorShape)>,
) {
    let t = app.theme;
    if let Mode::Copy(cm) = &app.mode
        && cm.pane == pid
    {
        cm.draw(g, r, &t);
        return;
    }
    if crate::browser::browser_of(app, app.cur, pid).is_some() {
        crate::browser::draw_pane(app, g, pid, r);
        return;
    }
    let Some(buf) = app.m().panes.get(pid) else {
        g.put_str(r.x + 1, r.y, "…", t.dim(), r.w);
        return;
    };
    draw_pane(g, buf, r);
    crate::pane_images::draw(app, g, app.cur, pid, r);
    crate::osc::draw_hover(app, g, app.cur, pid, r);
    // A failed command's exit code for 5 s (03 §8): in the corner of unfocused panes; the
    // focused pane's shows in the tab bar instead (the focused pane is the agent's, 08 §0).
    if focused != Some(pid)
        && let Some(badge) = crate::osc::exit_badge(app, app.cur, pid, std::time::Instant::now())
    {
        let w = unicode_width::UnicodeWidthStr::width(badge.as_str()) as u16;
        g.put_str(r.x + r.w.saturating_sub(w), r.y, &badge, t.bold(t.red), w);
    }
    if focused == Some(pid)
        && buf.cursor.visible
        && matches!(
            app.mode,
            Mode::Normal | Mode::Prefix(PrefixState { menu: false, .. })
        )
    {
        let (cx, cy) = (buf.cursor.col, buf.cursor.row);
        if cx < r.w && cy < r.h {
            *cursor = Some((r.x + cx, r.y + cy, buf.cursor.shape));
        }
    }
    // Recovery / exit badges.
    if let Some(p) = app.m().model.panes.iter().find(|p| p.id == pid)
        && let Some(rec) = p.recovered.as_deref()
        && let Some(badge) =
            crate::osc::recovery_badge(app, app.cur, pid, rec, std::time::Instant::now())
    {
        let w = badge.chars().count() as u16;
        g.put_str(r.x + r.w.saturating_sub(w), r.y, badge, t.dim(), w);
    }
}

fn draw_pane(g: &mut Grid, buf: &PaneBuf, r: Rect) {
    for (y, row) in buf.lines.iter().enumerate() {
        let y = y as u16;
        if y >= r.h {
            break;
        }
        g.put_row(r.x, r.y + y, row, r.w);
    }
    // Another client controls the PTY size (01 §1.4): show a hint when we letterbox.
    if buf.cols < r.w.saturating_sub(1) || buf.rows < r.h.saturating_sub(1) {
        let hint = format!(" ⇲ {}×{} (other client) ", buf.cols, buf.rows);
        let st = Style {
            attrs: attr::DIM,
            ..Style::default()
        };
        let w = hint.chars().count() as u16;
        if r.h > buf.rows {
            g.put_str(r.x + r.w.saturating_sub(w), r.y + r.h - 1, &hint, st, w);
        }
    }
}

fn draw_borders(
    app: &App,
    g: &mut Grid,
    area: Rect,
    rects: &[(String, Rect)],
    focused: Option<&str>,
) {
    if rects.len() < 2 {
        return;
    }
    let covered = |x: u16, y: u16| rects.iter().any(|(_, r)| r.contains(x, y));
    let fr = focused
        .and_then(|f| rects.iter().find(|(p, _)| p == f))
        .map(|(_, r)| *r);
    for y in area.y..area.y + area.h {
        for x in area.x..area.x + area.w {
            if covered(x, y) {
                continue;
            }
            let border = |dx: i32, dy: i32| {
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                if nx < area.x as i32
                    || ny < area.y as i32
                    || nx >= (area.x + area.w) as i32
                    || ny >= (area.y + area.h) as i32
                {
                    return false;
                }
                !covered(nx as u16, ny as u16)
            };
            let (u, d, l, r) = (border(0, -1), border(0, 1), border(-1, 0), border(1, 0));
            let ch = match (u || d, l || r) {
                (true, true) => match (u, d, l, r) {
                    (true, true, true, true) => "┼",
                    (false, true, true, true) => "┬",
                    (true, false, true, true) => "┴",
                    (true, true, false, true) => "├",
                    (true, true, true, false) => "┤",
                    _ => "┼",
                },
                (true, false) => "│",
                (false, true) => "─",
                _ => "│",
            };
            let near_focus = fr
                .is_some_and(|f| x + 1 >= f.x && x <= f.x + f.w && y + 1 >= f.y && y <= f.y + f.h);
            let st = crate::plugin_ui::border_style(app, rects, x, y)
                .unwrap_or_else(|| app.theme.border(near_focus));
            g.put_str(x, y, ch, st, 1);
        }
    }
}

/// The status-bar text for a degraded server (02 §4a). A storage failure says what it means for
/// the user: panes and typing keep working, but snapshots and the archive are paused, so a
/// crash now would recover from the ring only.
pub fn degraded_label(d: &str) -> String {
    if d.starts_with("storage unavailable") {
        "storage degraded · ring only".to_string()
    } else {
        truncate(d, 30)
    }
}

/// Widest a run's label gets in the sidebar and tab bar (session titles run to 80 characters).
pub const RUN_LABEL_MAX: usize = 28;

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod degraded_label_tests {
    use super::degraded_label;

    #[test]
    fn storage_failures_say_ring_only_and_other_reasons_are_truncated() {
        assert_eq!(
            degraded_label("storage unavailable: database or disk is full"),
            "storage degraded · ring only"
        );
        assert_eq!(degraded_label("store degraded: x"), "store degraded: x");
        assert!(degraded_label(&"y".repeat(100)).chars().count() <= 30);
    }
}
