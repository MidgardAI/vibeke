//! Cross-machine agent list (06 A5, 08 §6.5): one popup listing every live agent run on every
//! connected machine, most urgent first, with jump-to.
//!
//! Bound to `agent_list` (default **`prefix+alt+a`**: `prefix+a` is `next_attention` and
//! `prefix+shift+a` `next_attention_focus`, so the list of all agents takes the same letter with
//! `alt`, as `pin_pane`/`sync_input_pane` do). Ordering: open approvals and questions first
//! (oldest open interaction first), then errors and rate limits, done-unseen, working, idle,
//! the rest; ties by how long the run has been in that state (longest first), then machine and
//! name. Each row: `machine · icon name · workspace · state glyph label · age`. Typing filters
//! (fuzzy, over machine, name, harness, workspace, state and branch); `↑/↓`, `tab`, `ctrl+n/p`
//! move; `enter` focuses the agent's pane on its machine; `alt+enter` opens its card when it has
//! an open interaction (gated by `ui.interaction_overlay`, as everywhere); `esc` closes. Nothing
//! is sent to an agent.

use crate::app::{App, Mode, Popup};
use crate::nav::{ListKey, fuzzy, highlight, list_frame, list_key, list_row};
use crate::screen::{Grid, Rect as SRect};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::KeyEvent;
use vk_proto::model::*;
use vk_proto::render::{Style, attr};

/// One row.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub mi: usize,
    pub run: String,
    pub pane: String,
    /// Shown and matched.
    pub label: String,
    /// Matched, not shown.
    pub extra: String,
    pub glyph: String,
    pub state: String,
    /// `crate::draw::urgency` (8 approval … 0).
    pub urgency: u8,
    /// Since when the run has needed attention / been in its state (ms).
    pub since_ms: i64,
    /// Open interaction, if any.
    pub interaction: Option<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Every live agent run on every machine, by attention.
pub fn entries(app: &App) -> Vec<Entry> {
    let multi = app.machines.len() > 1;
    let mut v = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        for r in m.model.runs.iter().filter(|r| r.ended_at_ms.is_none()) {
            if crate::sidebar::hidden(app, mi, r) {
                continue;
            }
            let pane = m.model.panes.iter().find(|p| p.id == r.pane);
            let ws = pane
                .and_then(|p| m.model.workspaces.iter().find(|w| w.id == p.workspace))
                .map(|w| (w.display_name().to_string(), w.branch.clone()));
            let open = m
                .model
                .interactions
                .iter()
                .filter(|i| i.run == r.id && i.status == InteractionStatus::Open)
                .min_by_key(|i| i.opened_at_ms);
            let (glyph, state, _, _) = crate::draw::run_state(app, m, r);
            let name = r.label().to_string();
            let on = if multi {
                format!("{} · ", m.label)
            } else {
                String::new()
            };
            let (wsn, branch) = ws.unwrap_or_default();
            let label = format!(
                "{on}{} {name} · {wsn}",
                crate::draw::harness_icon(&r.harness)
            );
            v.push(Entry {
                mi,
                run: r.id.clone(),
                pane: r.pane.clone(),
                label,
                extra: format!(
                    "{} {} {} {}",
                    r.harness,
                    state,
                    branch.unwrap_or_default(),
                    m.label
                ),
                glyph,
                state,
                urgency: crate::draw::urgency(app, m, r),
                since_ms: open.map(|i| i.opened_at_ms).unwrap_or(r.execution.since_ms),
                interaction: open.map(|i| i.id.clone()),
            });
        }
    }
    v.sort_by(|a, b| {
        b.urgency
            .cmp(&a.urgency)
            .then(a.since_ms.cmp(&b.since_ms))
            .then(a.mi.cmp(&b.mi))
            .then(a.label.cmp(&b.label))
    });
    v
}

/// Entries matching `filter` (attention order kept), with highlight positions in the label.
pub fn ranked(app: &App, filter: &str) -> Vec<(Entry, Vec<usize>)> {
    entries(app)
        .into_iter()
        .filter_map(|e| {
            let full = format!("{} {}", e.label, e.extra);
            let m = fuzzy(filter, &full)?;
            let n = e.label.chars().count();
            let pos = m.positions.into_iter().filter(|p| *p < n).collect();
            Some((e, pos))
        })
        .collect()
}

pub fn open(app: &mut App) {
    app.mode = Mode::Popup(Popup::Agents {
        filter: String::new(),
        sel: 0,
    });
}

pub fn action(app: &mut App, action: &str) -> bool {
    if matches!(action, "agent_list" | "agents") {
        open(app);
        return true;
    }
    false
}

pub fn key(app: &mut App, ev: KeyEvent, filter: String, sel: usize) {
    let list = ranked(app, &filter);
    match list_key(&ev, filter, sel, list.len()) {
        ListKey::Close => {}
        ListKey::Enter(i) => {
            if let Some((e, _)) = list.get(i) {
                if !app.machines[e.mi].connected() {
                    app.toast(format!("{} is offline", app.machines[e.mi].label));
                    return;
                }
                app.focus_pane(e.mi, &e.pane);
            }
        }
        ListKey::EnterAlt(i) => {
            if let Some((e, _)) = list.get(i) {
                match &e.interaction {
                    Some(it) => crate::popup_pane::open_card(app, e.mi, it),
                    None => app.focus_pane(e.mi, &e.pane),
                }
            }
        }
        ListKey::Stay(filter, sel) => app.mode = Mode::Popup(Popup::Agents { filter, sel }),
    }
}

pub fn draw(app: &App, g: &mut Grid, filter: &str, sel: usize) -> (u16, u16) {
    let list = ranked(app, filter);
    let total = entries(app).len();
    let machines = app.machines.len();
    let (x, y, w, rows) = list_frame(
        app,
        g,
        &format!(
            "agents · {total} on {machines} machine(s) · by attention · enter jump · alt+enter card"
        ),
        filter,
    );
    let t = app.theme;
    let hi = Style {
        attrs: attr::BOLD | attr::UNDERLINE,
        ..t.s(t.accent)
    };
    let now = now_ms();
    let skip = sel.saturating_sub(rows.saturating_sub(1) as usize);
    for (row, (i, (e, pos))) in list
        .iter()
        .enumerate()
        .skip(skip)
        .take(rows as usize)
        .enumerate()
    {
        let color = match e.urgency {
            8 | 6 => t.red,
            7 | 5 => t.yellow,
            4 => t.green,
            3 => t.accent,
            _ => t.muted,
        };
        let mut segs = vec![(format!("{} ", e.glyph), t.bold(color))];
        segs.extend(highlight(&e.label, pos, t.text(), hi));
        segs.push((format!(" · {}", e.state), t.s(color)));
        let age = crate::inbox::fmt_age(now - e.since_ms);
        let at = SRect {
            x,
            y: y + row as u16,
            w,
            h: 1,
        };
        list_row(g, at, segs, &age, i == sel, app);
    }
    if list.is_empty() {
        let msg = if total == 0 {
            "no agents running on any machine"
        } else {
            "nothing matches"
        };
        g.put_str(x + 1, y, msg, t.dim(), w);
    }
    (
        crate::nav::filter_col(x) + UnicodeWidthStr::width(filter) as u16,
        y - 1,
    )
}

#[cfg(test)]
#[path = "agent_list_tests.rs"]
mod tests;
