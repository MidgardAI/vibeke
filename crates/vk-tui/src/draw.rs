//! Frame composition (03 §6.2): chrome (sidebar, tab bar, borders, popups) + pane cells.

use crate::app::{App, Mode, PaneBuf};
use crate::screen::{Grid, Rect as SRect};
use vk_proto::layout::Rect;
use vk_proto::model::*;
use vk_proto::render::{CursorShape, Style, attr};

/// One sidebar line: text segments with styles, and the pane it targets when clicked/entered.
pub struct SideRow {
    pub segs: Vec<(String, Style)>,
    pub target: Option<(usize, String)>,
    pub focused: bool,
}

pub fn harness_icon(h: &str) -> &'static str {
    match h {
        "claude" => "✳",
        "codex" => "◎",
        "pi" | "omp" => "π",
        _ => "•",
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
    if let Some(i) = open.iter().find(|i| i.kind == InteractionKind::Question) {
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

fn urgency(app: &App, m: &crate::app::Machine, r: &AgentRun) -> u8 {
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

fn agent_row(app: &App, mi: usize, r: &AgentRun, indent: &str) -> SideRow {
    let m = &app.machines[mi];
    let t = &app.theme;
    let (g, label, color, inferred) = run_state(app, m, r);
    let name = r.name.clone().unwrap_or_else(|| r.harness.clone());
    let mut segs = vec![
        (format!("{indent}{} ", harness_icon(&r.harness)), t.s(color)),
        (format!("{name} "), t.text()),
    ];
    if r.yolo {
        segs.push(("YOLO·HOST ".into(), app.theme.bold(t.red)));
    }
    let glyph = if inferred
        && app.config.ui.sidebar.show_state_source != vk_config::ShowStateSource::Never
    {
        format!("{g}~")
    } else {
        g
    };
    segs.push((format!("{glyph} "), t.bold(color)));
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
    let focused = mi == app.cur && app.m().focus.pane.as_deref() == Some(&r.pane);
    SideRow {
        segs,
        target: Some((mi, r.pane.clone())),
        focused,
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
            });
            for (_, mi, r) in need {
                rows.push(agent_row(app, mi, r, " "));
            }
            rows.push(SideRow {
                segs: vec![],
                target: None,
                focused: false,
            });
        }
    }
    let multi = app.machines.len() > 1;
    for (mi, m) in app.machines.iter().enumerate() {
        if multi || !m.connected() {
            let dot = if m.connected() {
                ("● ", t.green)
            } else {
                ("○ ", t.muted)
            };
            let status = if m.connected() {
                String::new()
            } else {
                format!(" {}", m.status)
            };
            rows.push(SideRow {
                segs: vec![
                    (dot.0.into(), t.s(dot.1)),
                    (m.label.clone(), t.bold(t.fg)),
                    (status, t.dim()),
                ],
                target: None,
                focused: false,
            });
        }
        for w in &m.model.workspaces {
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
                    format!("{}{marker}", if focused_ws { "▸ " } else { "  " }),
                    t.s(t.accent),
                ),
                (w.display_name().to_string(), name_style),
            ];
            if let Some(b) = &w.branch {
                segs.push((format!(" ⎇ {b}"), t.dim()));
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
            });
            for r in &runs {
                rows.push(agent_row(app, mi, r, "    "));
            }
            if app.config.ui.sidebar.show_shell_panes {
                for p in m
                    .model
                    .panes
                    .iter()
                    .filter(|p| p.workspace == w.id && !runs.iter().any(|r| r.pane == p.id))
                {
                    let focused = mi == app.cur && m.focus.pane.as_deref() == Some(&p.id);
                    rows.push(SideRow {
                        segs: vec![(format!("    {}", p.display_title()), t.dim())],
                        target: Some((mi, p.id.clone())),
                        focused,
                    });
                }
            }
        }
    }
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
        });
        for (mi, p) in pinned {
            rows.push(SideRow {
                segs: vec![(format!("  {}", p.display_title()), t.text())],
                target: Some((mi, p.id.clone())),
                focused: false,
            });
        }
    }
    rows
}

pub fn sidebar_targets(app: &App) -> Vec<(usize, String)> {
    sidebar_rows(app)
        .into_iter()
        .filter_map(|r| r.target)
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

/// Tab bar entries with their x ranges.
fn tab_entries(app: &App) -> Vec<(Tab, String, u16, u16)> {
    let m = app.m();
    let Some(ws) = &m.focus.workspace else {
        return vec![];
    };
    let mut x = if app.sidebar { app.sidebar_w + 1 } else { 0 } + 1;
    let mut out = Vec::new();
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
                Some(r) => r.name.clone().unwrap_or_else(|| r.harness.clone()),
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
        let label = format!(" {}{} {}{zoom} ", glyph, t.number, title);
        let w = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
        out.push((t.clone(), label, x, x + w));
        x += w + 1;
    }
    out
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
    let (cols, rows) = app.size;
    // Sidebar.
    if app.sidebar {
        let w = app.sidebar_w;
        g.fill(
            SRect {
                x: 0,
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
        g.put_str(0, 0, &title, t.bold(t.accent), w);
        let nav_sel = if let Mode::Navigate { sel } = app.mode {
            Some(sel)
        } else {
            None
        };
        let mut target_i = 0;
        for (i, r) in sidebar_rows(app).iter().enumerate() {
            let y = i as u16 + 1;
            if y >= rows {
                break;
            }
            let selected = r.target.is_some() && nav_sel == Some(target_i);
            if r.target.is_some() {
                target_i += 1;
            }
            if selected || r.focused {
                g.fill(SRect { x: 0, y, w, h: 1 }, t.sel(t.fg));
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
                x += g.put_str(x, y, s, st, w.saturating_sub(x));
            }
        }
        for y in 0..rows {
            g.put_str(w, y, "│", t.border(false), 1);
        }
    }
    // Tab bar.
    let tx = if app.sidebar { app.sidebar_w + 1 } else { 0 };
    g.fill(
        SRect {
            x: tx,
            y: 0,
            w: cols.saturating_sub(tx),
            h: 1,
        },
        t.text(),
    );
    let focused_tab = app.m().focus.tab.clone();
    for (tab, label, x, _) in tab_entries(app) {
        let st = if Some(&tab.id) == focused_tab.as_ref() {
            t.rev()
        } else {
            t.dim()
        };
        g.put_str(x, 0, &label, st, cols.saturating_sub(x));
    }
    // Right side of the tab bar: mode, toasts, connection.
    let mut right: Vec<(String, Style)> = Vec::new();
    match &app.mode {
        Mode::Prefix(_) => right.push((" PREFIX ".into(), t.rev())),
        Mode::Copy(_) => right.push((" COPY ".into(), t.rev())),
        Mode::Navigate { .. } => right.push((" NAV ".into(), t.rev())),
        Mode::Resize => right.push((" RESIZE ".into(), t.rev())),
        _ => {}
    }
    if let Some(up) = crate::upload::status(app) {
        right.insert(0, (format!(" {up} "), t.s(t.yellow)));
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
        right.insert(0, (format!(" ⚠ {} ", truncate(d, 30)), t.bold(t.red)));
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
    let rw: u16 = right
        .iter()
        .map(|(s, _)| unicode_width::UnicodeWidthStr::width(s.as_str()) as u16)
        .sum();
    let mut x = cols.saturating_sub(rw);
    for (s, st) in &right {
        x += g.put_str(x, 0, s, *st, cols.saturating_sub(x));
    }
    // Panes.
    let area = app.pane_area();
    let rects = app.pane_rects();
    let focused = app.m().focus.pane.clone();
    let mut cursor = None;
    if rects.is_empty() {
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
    let copy_pane = if let Mode::Copy(cm) = &app.mode {
        Some(cm.pane.clone())
    } else {
        None
    };
    for (pid, r) in &rects {
        if copy_pane.as_deref() == Some(pid.as_str())
            && let Mode::Copy(cm) = &app.mode
        {
            cm.draw(g, *r, &t);
            continue;
        }
        let Some(buf) = app.m().panes.get(pid) else {
            g.put_str(r.x + 1, r.y, "…", t.dim(), r.w);
            continue;
        };
        draw_pane(g, buf, *r);
        if focused.as_deref() == Some(pid.as_str())
            && buf.cursor.visible
            && matches!(app.mode, Mode::Normal | Mode::Prefix(_))
        {
            let (cx, cy) = (buf.cursor.col, buf.cursor.row);
            if cx < r.w && cy < r.h {
                cursor = Some((r.x + cx, r.y + cy, buf.cursor.shape));
            }
        }
        // Recovery / exit badges.
        if let Some(p) = app.m().model.panes.iter().find(|p| &p.id == pid)
            && p.recovered.as_deref() == Some("ring_only")
        {
            let badge = " recovered (ring only) ";
            let w = badge.len() as u16;
            g.put_str(r.x + r.w.saturating_sub(w), r.y, badge, t.dim(), w);
        }
    }
    if !matches!(
        app.mode,
        Mode::Normal | Mode::Prefix(_) | Mode::Navigate { .. } | Mode::Resize
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
            g.put_str(x, y, ch, app.theme.border(near_focus), 1);
        }
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
    t.push('…');
    t
}
