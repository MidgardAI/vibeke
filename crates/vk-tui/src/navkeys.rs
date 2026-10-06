//! Navigate-mode keys and palette arguments (08 §6.1, §6.3).
//!
//! Navigate mode (`prefix+w`) gains `/` (type to filter the sidebar rows; `enter` keeps the
//! filter and returns to moving, `esc` clears it), `t` (new task in the selected row's repo) and
//! `p` (pin the selected pane). The filter lasts until navigate mode ends.
//!
//! Palette entries for actions that take an argument ask for it inline before running:
//! `split_vertical`/`split_horizontal` → `size?` (`30%`, empty = half), `switch_tab` → tab number,
//! `switch_workspace` → workspace number, `focus_agent` → agent number (sidebar order),
//! `new_tab` → command (empty = shell). Their keybindings run without asking, as before.

use crate::app::{App, Mode, Pending, Prompt, PromptKind};
use serde_json::json;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

#[derive(Debug, Default, Clone)]
pub struct State {
    /// Sidebar filter in navigate mode.
    pub filter: Option<String>,
    /// Typing into the filter.
    pub typing: bool,
}

/// The active sidebar filter (navigate mode only).
pub fn filter(app: &App) -> Option<&str> {
    // Cleared by `tick` once navigate mode ends (the mode is taken out while keys run).
    app.ux.nav.filter.as_deref().filter(|f| !f.is_empty())
}

/// Keep only rows matching the navigate filter (and their selectable ones).
pub fn apply_filter(app: &App, rows: &mut Vec<crate::draw::SideRow>) {
    let Some(f) = filter(app) else {
        return;
    };
    let f = f.to_lowercase();
    rows.retain(|r| {
        r.selectable() && {
            let text: String = r.segs.iter().map(|(s, _)| s.as_str()).collect();
            text.to_lowercase().contains(&f)
        }
    });
}

pub fn navigate_key(app: &mut App, ev: &KeyEvent, sel: usize) -> bool {
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Navigate { sel };
        return true;
    }
    if app.ux.nav.typing {
        let f = app.ux.nav.filter.get_or_insert_with(String::new);
        match ev.key {
            Key::Named(NamedKey::Escape) => {
                app.ux.nav.filter = None;
                app.ux.nav.typing = false;
            }
            Key::Named(NamedKey::Enter) => app.ux.nav.typing = false,
            Key::Named(NamedKey::Backspace) => {
                f.pop();
            }
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => f.push(c),
            _ => {}
        }
        app.mode = Mode::Navigate { sel: 0 };
        return true;
    }
    let row = crate::draw::sidebar_targets(app).get(sel).cloned();
    match ev.key {
        Key::Char('/') => {
            app.ux.nav.typing = true;
            app.ux.nav.filter.get_or_insert_with(String::new);
            app.mode = Mode::Navigate { sel: 0 };
            true
        }
        Key::Named(NamedKey::Escape) if app.ux.nav.filter.is_some() => {
            app.ux.nav.filter = None;
            app.mode = Mode::Navigate { sel: 0 };
            true
        }
        Key::Char('t') => {
            if let Some((mi, pane)) = row
                && app.machines[mi].model.panes.iter().any(|p| p.id == pane)
            {
                app.focus_pane(mi, &pane);
            }
            app.mode = Mode::Prompt(Prompt {
                kind: PromptKind::TaskTitle,
                label: "new task title".into(),
                input: String::new(),
            });
            true
        }
        Key::Char('p') => {
            if let Some((mi, pane)) = row
                && app.machines[mi].model.panes.iter().any(|p| p.id == pane)
            {
                app.command_on(mi, "pane.pin", json!({"pane": pane}), Pending::Ignore);
            }
            app.mode = Mode::Navigate { sel };
            true
        }
        _ => false,
    }
}

/// Leave the filter behind when navigate mode ends.
pub fn tick(app: &mut App) {
    if !matches!(app.mode, Mode::Navigate { .. }) && app.ux.nav.filter.is_some() {
        app.ux.nav = State::default();
    }
}

/// Actions whose palette entry asks for an argument: (action, prompt).
pub const ARG_ACTIONS: &[(&str, &str)] = &[
    (
        "split_vertical",
        "split side by side: size? (30%, empty = half)",
    ),
    (
        "split_horizontal",
        "split stacked: size? (30%, empty = half)",
    ),
    ("switch_tab", "switch to tab number?"),
    ("switch_workspace", "switch to workspace number?"),
    ("focus_agent", "focus agent number? (sidebar order)"),
    ("new_tab", "new tab: command? (empty = shell)"),
];

/// From the palette: ask for the argument first. True when a prompt opened.
pub fn wants_arg(app: &mut App, action: &str) -> bool {
    let Some((_, label)) = ARG_ACTIONS.iter().find(|(a, _)| *a == action) else {
        return false;
    };
    app.mode = Mode::Prompt(Prompt {
        kind: PromptKind::ActionArg {
            action: action.to_string(),
        },
        label: label.to_string(),
        input: String::new(),
    });
    true
}

/// Parse `30%`, `0.3` or `30` (percent) into the new pane's share.
pub fn parse_size(v: &str) -> Option<f64> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    let n: f64 = v.trim_end_matches('%').trim().parse().ok()?;
    let r = if n > 1.0 { n / 100.0 } else { n };
    (r > 0.0 && r < 1.0).then_some(r)
}

/// Run an action with the argument typed into its prompt.
pub fn run_with_arg(app: &mut App, action: &str, v: &str) {
    let num = || v.trim().parse::<usize>().ok().filter(|n| *n >= 1);
    match action {
        "split_vertical" | "split_horizontal" => {
            let Some(p) = app.focused_pane() else {
                return;
            };
            let dir = if action == "split_vertical" {
                "right"
            } else {
                "down"
            };
            let mut params = json!({"pane": p, "direction": dir, "focus": true});
            match parse_size(v) {
                Some(r) => params["ratio"] = json!(r),
                None if !v.trim().is_empty() => {
                    app.toast(format!("size `{v}`: use 30% or 0.3"));
                    return;
                }
                None => {}
            }
            app.command("pane.split", params, Pending::Ignore);
        }
        "switch_tab" => match num() {
            Some(n) => app.action("switch_tab", Some(n - 1)),
            None => app.toast("a tab number, 1 or more"),
        },
        "switch_workspace" => {
            let Some(n) = num() else {
                return app.toast("a workspace number, 1 or more");
            };
            let ws = app.m().model.workspaces.get(n - 1).map(|w| w.id.clone());
            match ws {
                Some(w) => {
                    let cur = app.cur;
                    crate::nav::focus_workspace(app, cur, &w);
                }
                None => app.toast(format!("no workspace {n}")),
            }
        }
        "focus_agent" => {
            let Some(n) = num() else {
                return app.toast("an agent number, 1 or more");
            };
            let runs: Vec<(usize, String)> = app
                .machines
                .iter()
                .enumerate()
                .flat_map(|(mi, m)| m.model.runs.iter().map(move |r| (mi, r.pane.clone())))
                .collect();
            match runs.get(n - 1).cloned() {
                Some((mi, p)) => app.focus_pane(mi, &p),
                None => app.toast(format!("no agent {n}")),
            }
        }
        "new_tab" => {
            let Some(w) = app.focused_ws() else {
                return;
            };
            let cmd = v.trim();
            let mut params = json!({"workspace": w.id, "focus": true});
            if !cmd.is_empty() {
                params["command"] = json!(["/bin/sh", "-c", cmd]);
                params["title"] = json!(cmd);
            }
            app.command("tab.create", params, Pending::Ignore);
        }
        _ => app.action(action, None),
    }
}

#[cfg(test)]
#[path = "navkeys_tests.rs"]
mod tests;
