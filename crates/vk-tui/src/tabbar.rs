//! Tab bar overflow, middle-click close and drag to reorder (08 §3).
//!
//! - **Overflow.** When the tabs don't fit the tab row, a `‹` / `›` arrow marks hidden tabs on
//!   either side; clicking an arrow scrolls by one tab. The window follows the focused tab: a
//!   manual scroll holds until the focus moves to another tab. `prefix+g` (goto) still reaches
//!   every tab.
//! - **Middle click** closes the tab, asking first when one of its panes runs a non-shell
//!   process.
//! - **Drag.** Pressing a tab focuses it (as before); dragging it over other tabs and releasing
//!   moves it there (`tab.move {tab, delta}`; tab numbers stay as they are). A `▏` marks the drop
//!   slot while dragging.
//! - `ui.tabs.show_numbers = false` drops the number from labels.

use crate::app::{Action, App, Mode, Pending, Popup};
use crossterm::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::json;
use vk_proto::model::Tab;

#[derive(Debug, Default, Clone)]
pub struct State {
    /// Manual scroll: (first visible tab index, focused tab when scrolled).
    pub scroll: Option<(usize, Option<String>)>,
    /// A tab press that may become a drag: (tab id, index, current drop index).
    pub drag: Option<(String, usize, usize)>,
}

/// A laid-out tab bar: visible entries with their columns, and the arrows.
#[derive(Debug, Clone, Default)]
pub struct Laid {
    pub entries: Vec<(Tab, String, u16, u16)>,
    /// Column of `‹` when tabs are hidden on the left.
    pub left: Option<u16>,
    /// Column of `›` when tabs are hidden on the right.
    pub right: Option<u16>,
    pub first: usize,
}

/// Lay out `raw` (tab, label, width) between `x0` and `end` (exclusive).
pub fn layout(app: &App, raw: Vec<(Tab, String, u16)>, x0: u16, end: u16) -> Laid {
    let total: u16 = raw.iter().map(|(_, _, w)| w + 1).sum();
    let place = |from: usize, mut x: u16, stop: u16| {
        let mut v = Vec::new();
        for (t, l, w) in raw.iter().skip(from) {
            if x + w > stop {
                break;
            }
            v.push((t.clone(), l.clone(), x, x + w));
            x += w + 1;
        }
        v
    };
    if x0 + total <= end || raw.is_empty() {
        return Laid {
            entries: place(0, x0, end),
            ..Default::default()
        };
    }
    // Overflow: reserve a column on each side for the arrows.
    let (lo, hi) = (x0 + 2, end.saturating_sub(2));
    let focused = app.m().focus.tab.clone();
    let fi = raw
        .iter()
        .position(|(t, _, _)| Some(&t.id) == focused.as_ref())
        .unwrap_or(0);
    let fits = |first: usize| {
        let mut x = lo;
        let mut last = first;
        for (i, (_, _, w)) in raw.iter().enumerate().skip(first) {
            if x + w > hi {
                break;
            }
            last = i;
            x += w + 1;
        }
        last
    };
    let mut first = match &app.ux.tabs.scroll {
        Some((f, at)) if *at == focused => (*f).min(raw.len() - 1),
        _ => fi.min(raw.len() - 1),
    };
    let manual = matches!(&app.ux.tabs.scroll, Some((_, at)) if *at == focused);
    if !manual {
        // Show as many tabs as fit ending at the focused one.
        while first > 0 && fits(first - 1) >= fi {
            first -= 1;
        }
    }
    let entries = place(first, lo, hi);
    let shown_last = first + entries.len();
    Laid {
        left: (first > 0).then_some(x0),
        right: (shown_last < raw.len()).then_some(end.saturating_sub(1)),
        entries,
        first,
    }
}

fn tabs_of_focus(app: &App) -> Vec<Tab> {
    let m = app.m();
    let Some(ws) = &m.focus.workspace else {
        return vec![];
    };
    m.model
        .tabs
        .iter()
        .filter(|t| &t.workspace == ws)
        .cloned()
        .collect()
}

fn tab_busy(app: &App, t: &Tab) -> bool {
    let shell = |s: &str| {
        matches!(
            s.rsplit('/').next().unwrap_or(s).trim_start_matches('-'),
            "zsh" | "bash" | "fish" | "sh" | "dash" | "nu" | "tcsh" | "ksh"
        )
    };
    let ids = t.layout.panes();
    app.m().model.panes.iter().any(|p| {
        (ids.contains(&p.id) || t.floating.iter().any(|f| f.pane == p.id))
            && !p.fg_cmdline.is_empty()
            && !shell(&p.fg_cmdline[0])
    })
}

/// Close a tab from a middle click: ask when something runs in it.
pub fn close_tab(app: &mut App, t: &Tab) {
    if tab_busy(app, t) {
        app.mode = Mode::Popup(Popup::Confirm {
            message: format!("Close tab {}? A process is running.", t.number),
            action: Box::new(Action::CloseTab(t.id.clone())),
        });
    } else {
        app.command("tab.close", json!({"tab": t.id}), Pending::Ignore);
    }
}

pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    let Some(row) = crate::chrome::tab_row(app) else {
        app.ux.tabs.drag = None;
        return false;
    };
    // A drag in progress follows the pointer anywhere on the row.
    if let Some((id, from, to)) = app.ux.tabs.drag.clone() {
        match me.kind {
            MouseEventKind::Drag(CtButton::Left) => {
                let laid = crate::draw::tab_layout(app);
                let to = laid
                    .entries
                    .iter()
                    .enumerate()
                    .find(|(_, (_, _, a, b))| me.column < (*a + *b) / 2 + 1)
                    .map(|(i, _)| laid.first + i)
                    .unwrap_or(laid.first + laid.entries.len().saturating_sub(1));
                app.ux.tabs.drag = Some((id, from, to));
                app.dirty = true;
                return true;
            }
            MouseEventKind::Up(CtButton::Left) => {
                app.ux.tabs.drag = None;
                if to != from {
                    app.command(
                        "tab.move",
                        json!({"tab": id, "delta": to as i64 - from as i64}),
                        Pending::Ignore,
                    );
                }
                app.dirty = true;
                return true;
            }
            _ => {}
        }
    }
    if me.row != row {
        return false;
    }
    let laid = crate::draw::tab_layout(app);
    match me.kind {
        MouseEventKind::Down(CtButton::Left) => {
            if laid.left == Some(me.column) {
                let focused = app.m().focus.tab.clone();
                app.ux.tabs.scroll = Some((laid.first.saturating_sub(1), focused));
                return true;
            }
            if laid.right == Some(me.column) {
                let focused = app.m().focus.tab.clone();
                let n = tabs_of_focus(app).len();
                app.ux.tabs.scroll = Some(((laid.first + 1).min(n.saturating_sub(1)), focused));
                return true;
            }
            if let Some((i, (t, _, _, _))) = laid
                .entries
                .iter()
                .enumerate()
                .find(|(_, (_, _, a, b))| me.column >= *a && me.column < *b)
            {
                let idx = laid.first + i;
                app.ux.tabs.drag = Some((t.id.clone(), idx, idx));
                app.command("tab.focus", json!({"tab": t.id}), Pending::Ignore);
                return true;
            }
            false
        }
        MouseEventKind::Down(CtButton::Middle) => {
            if let Some((t, _, _, _)) = laid
                .entries
                .iter()
                .find(|(_, _, a, b)| me.column >= *a && me.column < *b)
            {
                let t = t.clone();
                close_tab(app, &t);
                return true;
            }
            false
        }
        _ => false,
    }
}

/// The drop marker column while dragging (before the target tab).
pub fn drop_marker(app: &App, laid: &Laid) -> Option<u16> {
    let (_, from, to) = app.ux.tabs.drag.as_ref()?;
    if from == to {
        return None;
    }
    let i = to.checked_sub(laid.first)?;
    let (_, _, a, b) = laid.entries.get(i)?;
    Some(if to > from { *b } else { a.saturating_sub(1) })
}

#[cfg(test)]
#[path = "tabbar_tests.rs"]
mod tests;
