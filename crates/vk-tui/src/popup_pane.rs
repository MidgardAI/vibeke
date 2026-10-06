//! User popups and interaction-card gating (08 §5, §8; 03 §11.3).
//!
//! - `[[keys.command]] type = "popup"` opens the command in a session-modal popup:
//!   `pane.float {popup: {width, height}, command, title, cwd, rect}`. The server tags the pane
//!   like a plugin popup, so the popup machinery in [`crate::plugins`] draws it (rounded accent
//!   frame, `prefix+x closes`), keeps it modal and dismisses it when the command exits; when it
//!   is gone the focus returns to the pane that had it. `type = "float"` opens an ordinary
//!   floating pane.
//! - Popups can be **moved** (drag the top border) and **resized** (drag the bottom-right corner);
//!   the offset is this client's and lasts as long as the popup. While a popup is open the rest
//!   of the pane area is **dimmed**.
//! - **Edit scrollback in a popup**: on the local machine the editor runs in a popup pane over
//!   the TUI (the temp file is deleted when the popup goes away); remote panes keep the suspend
//!   path because the server there can't read this client's temp file.
//! - `ui.interaction_overlay`: `off` never opens a card (the pane is focused instead, so the
//!   agent's own dialog answers it), `unfocused` (default) refuses a card for the focused pane,
//!   `always` allows it on explicit invocation.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::screen::Grid;
use crossterm::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::{Value, json};
use std::collections::HashMap;
use vk_config::InteractionOverlay;
use vk_proto::layout::Rect;
use vk_proto::render::attr;

type Key = (usize, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    Move,
    Resize,
}

#[derive(Debug, Clone)]
pub struct Drag {
    pub pane: String,
    pub kind: DragKind,
    pub from: (u16, u16),
    pub base: (i32, i32, i32, i32),
}

#[derive(Default)]
pub struct State {
    /// Popup pane → the pane focused before it opened.
    pub prev: HashMap<Key, String>,
    /// Popup pane → its edit-scrollback temp file (deleted when the popup goes away).
    pub files: HashMap<Key, crate::scrollback::TempFile>,
    /// An editor file waiting for its popup's `pane.float` reply.
    pub pending_file: Option<crate::scrollback::TempFile>,
    /// Client-side move/resize of a popup: (dx, dy, dw, dh) in cells.
    pub offsets: HashMap<Key, (i32, i32, i32, i32)>,
    pub drag: Option<Drag>,
    /// Tests: run edit-scrollback's editor with the TUI suspended even locally.
    pub editor_suspends: bool,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Opened { prev: Option<String>, editor: bool },
}

fn pct(spec: Option<&str>) -> f64 {
    spec.and_then(|s| s.trim().strip_suffix('%'))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(80.0)
        .clamp(10.0, 100.0)
}

/// Open `argv` in a popup on the focused tab of the current machine.
pub fn open_popup(
    app: &mut App,
    argv: Vec<String>,
    title: &str,
    width: Option<&str>,
    height: Option<&str>,
    cwd: Option<String>,
    editor: bool,
) -> bool {
    let Some(tab) = app.focused_tab() else {
        app.toast("no tab for the popup");
        return false;
    };
    let (w, h) = (pct(width), pct(height));
    let prev = app.focused_pane();
    app.command(
        "pane.float",
        json!({
            "tab": tab.id, "command": argv, "title": title, "cwd": cwd, "focus": true,
            "popup": {"width": width.unwrap_or("80%"), "height": height.unwrap_or("80%")},
            "rect": {"x": (100.0 - w) / 2.0, "y": (100.0 - h) / 2.0, "w": w, "h": h},
        }),
        Pending::Ux(crate::ux::Reply::Popup(Reply::Opened { prev, editor })),
    );
    true
}

/// The working directory for a `[[keys.command]]` (`cwd = "pane" | "workspace" | path`).
pub fn command_cwd(app: &App, c: &vk_config::KeyCommand) -> Option<String> {
    let pane_cwd = || {
        let p = app.focused_pane()?;
        app.m().model.panes.iter().find(|x| x.id == p)?.cwd.clone()
    };
    match c.cwd.as_deref() {
        None | Some("pane") => pane_cwd(),
        Some("workspace") => app.focused_ws().map(|w| w.root_path),
        Some(p) => Some(match p.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
            None => p.to_string(),
        }),
    }
}

/// `[[keys.command]]` of type popup / float. True when handled.
pub fn run_key_command(app: &mut App, c: &vk_config::KeyCommand) -> bool {
    let argv = vec!["/bin/sh".to_string(), "-c".into(), c.command.clone()];
    let title = c.title.clone().unwrap_or_else(|| c.command.clone());
    let cwd = command_cwd(app, c);
    match c.kind {
        vk_config::CommandType::Popup => {
            open_popup(
                app,
                argv,
                &title,
                c.width.as_deref(),
                c.height.as_deref(),
                cwd,
                false,
            );
            true
        }
        vk_config::CommandType::Float => {
            if let Some(tab) = app.focused_tab() {
                app.command(
                    "pane.float",
                    json!({"tab": tab.id, "command": argv, "title": title, "cwd": cwd, "focus": true}),
                    Pending::Ignore,
                );
            }
            true
        }
        _ => false,
    }
}

/// Edit-scrollback's editor: a popup on the local machine, else the suspend path.
pub fn editor(app: &mut App, machine: usize, argv: Vec<String>, file: crate::scrollback::TempFile) {
    let local = app.machines.get(machine).is_some_and(|m| m.local) && app.m().local;
    if local && !app.ux.popups.editor_suspends && app.focused_tab().is_some() {
        app.ux.popups.pending_file = Some(file);
        app.mode = Mode::Normal;
        open_popup(
            app,
            argv,
            "scrollback (read-only copy)",
            Some("90%"),
            Some("90%"),
            None,
            true,
        );
    } else {
        app.external = Some(crate::scrollback::External { argv, file });
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    let Reply::Opened { prev, editor } = r;
    let file = if editor {
        app.ux.popups.pending_file.take()
    } else {
        None
    };
    match res {
        Ok(v) => {
            let Some(id) = v["pane"]["id"].as_str().map(str::to_string) else {
                return;
            };
            if let Some(p) = prev {
                app.ux.popups.prev.insert((mi, id.clone()), p);
            }
            if let Some(f) = file {
                app.ux.popups.files.insert((mi, id), f);
            }
        }
        Err(e) => app.toast(format!("✗ popup: {}", e.message)),
    }
}

/// Once per wakeup: popups that went away give the focus back and drop their temp files.
pub fn tick(app: &mut App) {
    let gone: Vec<Key> = app
        .ux
        .popups
        .prev
        .keys()
        .chain(app.ux.popups.files.keys())
        .filter(|(mi, p)| {
            app.machines
                .get(*mi)
                .is_none_or(|m| m.connected() && !m.model.panes.iter().any(|x| &x.id == p))
        })
        .cloned()
        .collect();
    for k in gone {
        app.ux.popups.files.remove(&k);
        app.ux.popups.offsets.remove(&k);
        if let Some(prev) = app.ux.popups.prev.remove(&k)
            && app.machines[k.0].model.panes.iter().any(|x| x.id == prev)
        {
            app.focus_pane(k.0, &prev);
        }
    }
}

/// Apply this client's move/resize to a popup surface.
pub fn adjust(app: &App, s: &mut crate::plugins::Surface) {
    if !s.info.is_popup() {
        return;
    }
    let Some(&(dx, dy, dw, dh)) = app.ux.popups.offsets.get(&(app.cur, s.pane.clone())) else {
        return;
    };
    let area = app.pane_area();
    let w = (s.outer.w as i32 + dw).clamp(crate::floats::MIN_W as i32 + 2, area.w as i32) as u16;
    let h = (s.outer.h as i32 + dh).clamp(crate::floats::MIN_H as i32 + 2, area.h as i32) as u16;
    let x = (s.outer.x as i32 + dx).clamp(area.x as i32, (area.x + area.w - w) as i32) as u16;
    let y = (s.outer.y as i32 + dy).clamp(area.y as i32, (area.y + area.h - h) as i32) as u16;
    s.outer = Rect { x, y, w, h };
    s.inner = crate::floats::inner_rect(s.outer);
}

/// Drag a popup's top border (move) or bottom-right corner (resize).
pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    if let Some(d) = app.ux.popups.drag.clone() {
        match me.kind {
            MouseEventKind::Drag(CtButton::Left) => {
                let ddx = me.column as i32 - d.from.0 as i32;
                let ddy = me.row as i32 - d.from.1 as i32;
                let (bx, by, bw, bh) = d.base;
                let v = match d.kind {
                    DragKind::Move => (bx + ddx, by + ddy, bw, bh),
                    DragKind::Resize => (bx, by, bw + ddx, bh + ddy),
                };
                app.ux.popups.offsets.insert((app.cur, d.pane), v);
                app.dirty = true;
                return true;
            }
            MouseEventKind::Up(_) => {
                app.ux.popups.drag = None;
                app.send_view_hints(false);
                return true;
            }
            _ => return true,
        }
    }
    let MouseEventKind::Down(CtButton::Left) = me.kind else {
        return false;
    };
    let Some(p) = crate::plugins::popup(app) else {
        return false;
    };
    let o = p.outer;
    let (x1, y1) = (o.x + o.w - 1, o.y + o.h - 1);
    let kind = if me.row == o.y && me.column > o.x && me.column < x1 {
        DragKind::Move
    } else if me.row == y1 && me.column == x1 {
        DragKind::Resize
    } else {
        return false;
    };
    let base = app
        .ux
        .popups
        .offsets
        .get(&(app.cur, p.pane.clone()))
        .copied()
        .unwrap_or_default();
    app.ux.popups.drag = Some(Drag {
        pane: p.pane,
        kind,
        from: (me.column, me.row),
        base,
    });
    true
}

/// Dim the pane area under an open popup (drawn before the popup itself).
pub fn dim(app: &App, g: &mut Grid) {
    if crate::plugins::popup(app).is_none() {
        return;
    }
    let a = app.pane_area();
    for y in a.y..a.y + a.h {
        g.add_attrs(a.x, y, a.w, attr::DIM);
    }
}

/// Open the card for an interaction (explicit invocation), following `ui.interaction_overlay`.
pub fn open_card(app: &mut App, mi: usize, interaction: &str) {
    let pane = app.machines[mi]
        .model
        .interactions
        .iter()
        .find(|i| i.id == interaction)
        .map(|i| i.pane.clone());
    let focused = mi == app.cur && app.focused_pane().is_some() && app.focused_pane() == pane;
    match app.config.ui.interaction_overlay {
        InteractionOverlay::Off => {
            if let Some(p) = pane {
                app.focus_pane(mi, &p);
            }
            app.toast("interaction cards are off — answer in the agent's own dialog");
        }
        InteractionOverlay::Unfocused if focused => {
            app.toast("this agent is focused — answer in its own dialog");
        }
        _ => {
            app.cur = mi;
            app.mode = Mode::Popup(Popup::Card {
                interaction: interaction.to_string(),
                sel: 0,
            });
        }
    }
}

#[cfg(test)]
#[path = "popup_pane_tests.rs"]
mod tests;
