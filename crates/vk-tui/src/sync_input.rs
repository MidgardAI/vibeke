//! Synchronized input (08 §5): type into several panes of a tab at once.
//!
//! `prefix+shift+s` (`sync_input`) toggles sync for the focused tab. While it is on, every key
//! and paste sent to the focused pane is also sent to the other panes of the tab that are in the
//! sync set: all terminal panes of the tab, minus the ones taken out with `prefix+alt+s`
//! (`sync_input_pane`). Agent panes are **excluded by default** (`ui.sync_input.include_agents =
//! false`) so a command never prompts N agents by accident; `prefix+alt+s` on an agent pane adds
//! it explicitly. Browser panes and popups never take part. The fan-out happens in this client
//! (one `Key`/`Paste` frame per pane, each with its own input id); the focused pane is only a
//! source when it is itself in the set.
//!
//! Indicator: a bright ` SYNC n ` badge on the right of the tab bar (n = panes receiving), a
//! `⇉` on the tab's label and the status bar's `sync_input` segment. Quick exit: the same
//! `prefix+shift+s`, the palette's `sync_input_off`, or clicking the badge.

use crate::app::App;
use std::collections::{HashMap, HashSet};
use vk_proto::input::KeyEvent;
use vk_proto::render::{ClientFrame, SyncPayload};

#[derive(Debug, Default, Clone)]
pub struct Set {
    /// Terminal panes taken out of the set.
    pub excluded: HashSet<String>,
    /// Agent panes added explicitly.
    pub agents: HashSet<String>,
}

#[derive(Debug, Default, Clone)]
pub struct State {
    /// (machine, tab) → sync set, for tabs with sync on.
    pub tabs: HashMap<(usize, String), Set>,
}

fn is_agent(app: &App, mi: usize, pane: &str) -> bool {
    app.machines[mi].model.runs.iter().any(|r| r.pane == pane)
}

/// Panes receiving input in a synced tab (the focused one included when it is a member).
pub fn members(app: &App, mi: usize, tab: &str) -> Vec<String> {
    let Some(set) = app.ux.sync.tabs.get(&(mi, tab.to_string())) else {
        return vec![];
    };
    let m = &app.machines[mi];
    let Some(t) = m.model.tabs.iter().find(|t| t.id == tab) else {
        return vec![];
    };
    let mut ids = t.layout.panes();
    ids.extend(t.floating.iter().map(|f| f.pane.clone()));
    ids.into_iter()
        .filter(|id| {
            let Some(p) = m.model.panes.iter().find(|p| &p.id == id) else {
                return false;
            };
            if p.is_browser() || p.plugin_surface().is_some() || p.exited {
                return false;
            }
            if set.excluded.contains(id) {
                return false;
            }
            !is_agent(app, mi, id)
                || app.config.ui.sync_input.include_agents
                || set.agents.contains(id)
        })
        .collect()
}

fn focused_key(app: &App) -> Option<(usize, String)> {
    app.m().focus.tab.clone().map(|t| (app.cur, t))
}

/// Sync is on for the focused tab.
pub fn active(app: &App) -> bool {
    focused_key(app).is_some_and(|k| app.ux.sync.tabs.contains_key(&k))
}

/// The other panes a key/paste to `pane` (focused) is mirrored to.
pub fn targets(app: &App, pane: &str) -> Vec<String> {
    let Some((mi, tab)) = focused_key(app) else {
        return vec![];
    };
    let ms = members(app, mi, &tab);
    if !ms.iter().any(|p| p == pane) {
        return vec![];
    }
    ms.into_iter().filter(|p| p != pane).collect()
}

/// The server feature that enforces agent exclusion for mirrored input (`SyncInput` frames).
pub const FEATURE: &str = "sync_input";

/// Whether the user explicitly let `pane` receive synced input although it runs an agent
/// (`prefix+alt+s`, or `ui.sync_input.include_agents`).
fn agent_included(app: &App, pane: &str) -> bool {
    app.config.ui.sync_input.include_agents
        || focused_key(app)
            .and_then(|k| app.ux.sync.tabs.get(&k))
            .is_some_and(|s| s.agents.contains(pane))
}

/// Send one mirrored input. To a server with [`FEATURE`] it goes as `SyncInput`, so the server
/// drops it for a pane whose agent started after this client's model was current; older
/// servers get plain `Key`/`Paste` frames (client-side exclusion only).
fn mirror(app: &mut App, pane: String, input: SyncPayload) {
    let id = app.next_input;
    app.next_input += 1;
    let include_agent = agent_included(app, &pane);
    let enforced = app.m().features.iter().any(|f| f == FEATURE);
    let frame = match (enforced, input) {
        (true, input) => ClientFrame::SyncInput {
            input_id: id,
            pane,
            input,
            include_agent,
        },
        (false, SyncPayload::Key(key)) => ClientFrame::Key {
            input_id: id,
            pane,
            key,
        },
        (false, SyncPayload::Paste(text)) => ClientFrame::Paste {
            input_id: id,
            pane,
            text,
        },
    };
    app.m().send(frame);
}

pub fn mirror_key(app: &mut App, pane: &str, ev: &KeyEvent) {
    for p in targets(app, pane) {
        mirror(app, p, SyncPayload::Key(ev.clone()));
    }
}

pub fn mirror_paste(app: &mut App, pane: &str, text: &str) {
    for p in targets(app, pane) {
        mirror(app, p, SyncPayload::Paste(text.to_string()));
    }
}

/// The tab label suffix for a synced tab.
pub fn tab_badge(app: &App, tab: &str) -> &'static str {
    if app.ux.sync.tabs.contains_key(&(app.cur, tab.to_string())) {
        " ⇉"
    } else {
        ""
    }
}

/// ` SYNC n ` for the right cluster / status bar while the focused tab is synced.
pub fn badge(app: &App) -> Option<String> {
    let (mi, tab) = focused_key(app)?;
    app.ux.sync.tabs.get(&(mi, tab.clone()))?;
    Some(format!(" SYNC {} ", members(app, mi, &tab).len()))
}

pub fn action(app: &mut App, action: &str) -> bool {
    match action {
        "sync_input" | "sync_input_off" => {
            let Some(k) = focused_key(app) else {
                return true;
            };
            if app.ux.sync.tabs.remove(&k).is_some() {
                app.toast("sync input off");
            } else if action == "sync_input" {
                app.ux.sync.tabs.insert(k.clone(), Set::default());
                let n = members(app, k.0, &k.1).len();
                let agents = if app.config.ui.sync_input.include_agents {
                    ""
                } else {
                    " (agents excluded; prefix+alt+s adds one)"
                };
                app.toast(format!(
                    "SYNC on: typing goes to {n} pane(s){agents} — prefix+shift+s stops"
                ));
            }
            true
        }
        "sync_input_pane" => {
            let (Some(k), Some(p)) = (focused_key(app), app.focused_pane()) else {
                return true;
            };
            let agent = is_agent(app, k.0, &p);
            let include_agents = app.config.ui.sync_input.include_agents;
            let set = app.ux.sync.tabs.entry(k).or_default();
            let now_in = if agent && !include_agents {
                if set.agents.remove(&p) {
                    false
                } else {
                    set.agents.insert(p.clone());
                    true
                }
            } else if set.excluded.remove(&p) {
                true
            } else {
                set.excluded.insert(p.clone());
                false
            };
            app.toast(if now_in {
                format!("{p} receives synced input")
            } else {
                format!("{p} left the sync set")
            });
            true
        }
        _ => false,
    }
}

/// Forget synced tabs that are gone.
pub fn tick(app: &mut App) {
    let live: HashSet<(usize, String)> = app
        .machines
        .iter()
        .enumerate()
        .flat_map(|(mi, m)| m.model.tabs.iter().map(move |t| (mi, t.id.clone())))
        .collect();
    let offline: HashSet<usize> = (0..app.machines.len())
        .filter(|i| !app.machines[*i].connected())
        .collect();
    app.ux
        .sync
        .tabs
        .retain(|k, _| live.contains(k) || offline.contains(&k.0));
}

#[cfg(test)]
#[path = "sync_input_tests.rs"]
mod tests;
