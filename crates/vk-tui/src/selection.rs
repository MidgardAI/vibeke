//! Mouse selection and copy-on-select (03 §11.1, 08 §13 D#748; `clipboard.copy_on_select`,
//! `clipboard.primary_selection`).
//!
//! In a pane without mouse reporting (or with `shift` held over one that has it), a left-button
//! drag enters copy mode with a character selection from the press to the pointer; dragging
//! extends it. On release:
//!
//! - `copy_on_select = true`: the selection is copied at once (soft wraps joined, as `y`) and
//!   copy mode closes;
//! - otherwise copy mode stays open with the selection, for `y` / `enter` (or more keys).
//!
//! A plain click (no drag) does nothing new. In copy mode a press starts a new selection, a drag
//! extends it and the wheel scrolls. Every copy — copy-mode yank or copy-on-select — also sets
//! the PRIMARY selection when `primary_selection = true` (OSC 52 `p`, else `wl-copy --primary`
//! / `xclip -selection primary` / `xsel -p`; macOS has no PRIMARY).

use crate::app::{App, Mode};
use crossterm::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use vk_proto::layout::Rect;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    /// Left button pressed in a pane (pane, local col, local row), not yet dragged.
    pub press: Option<(String, u16, u16)>,
    /// The current copy-mode selection was made with the mouse and has been dragged.
    pub dragged: bool,
}

/// Mouse over pane `pane` at `r` (already focused). Returns true when handled here.
pub fn on_mouse(
    app: &mut App,
    me: &MouseEvent,
    pane: &str,
    r: Rect,
    mouse_mode: bool,
    shift: bool,
) -> bool {
    let (col, row) = (me.column - r.x, me.row - r.y);
    // Copy mode on this pane: the mouse selects and scrolls.
    if let Mode::Copy(cm) = &mut app.mode
        && cm.pane == pane
    {
        match me.kind {
            MouseEventKind::Down(CtButton::Left) => {
                cm.mouse_select(row, col, true);
                cm.mouse = true;
                app.parity.selection.dragged = false;
            }
            MouseEventKind::Drag(CtButton::Left) => {
                cm.mouse_select(row, col, false);
                app.parity.selection.dragged = true;
            }
            MouseEventKind::Up(CtButton::Left) => {
                if !app.parity.selection.dragged {
                    // A click just moves the cursor.
                    cm.clear_selection();
                    return true;
                }
                app.parity.selection.dragged = false;
                finish_drag(app);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(me.kind, MouseEventKind::ScrollUp);
                let out = cm.wheel(up, 3);
                let Mode::Copy(cm) = std::mem::replace(&mut app.mode, Mode::Normal) else {
                    unreachable!()
                };
                app.copy_outcome(cm, out);
            }
            _ => {}
        }
        app.dirty = true;
        return true;
    }
    if !matches!(app.mode, Mode::Normal) || (mouse_mode && !shift) {
        app.parity.selection.press = None;
        return false;
    }
    if crate::browser::browser_of(app, app.cur, pane).is_some() {
        return false;
    }
    match me.kind {
        MouseEventKind::Down(CtButton::Left) => {
            app.parity.selection.press = Some((pane.to_string(), col, row));
            true
        }
        MouseEventKind::Drag(CtButton::Left) => {
            let Some((p, c0, r0)) = app.parity.selection.press.take() else {
                return false;
            };
            if p != pane {
                return false;
            }
            app.enter_copy(None);
            if let Mode::Copy(cm) = &mut app.mode {
                cm.mouse = true;
                cm.mouse_select(r0, c0, true);
                cm.mouse_select(row, col, false);
            }
            app.parity.selection.dragged = true;
            app.dirty = true;
            true
        }
        MouseEventKind::Up(CtButton::Left) => app.parity.selection.press.take().is_some(),
        _ => false,
    }
}

/// The drag ended: copy-on-select copies and leaves copy mode; otherwise the selection stays
/// for `y`.
fn finish_drag(app: &mut App) {
    let Mode::Copy(cm) = &app.mode else {
        return;
    };
    let text = cm.selection_text().filter(|t| !t.is_empty());
    if !app.config.clipboard.copy_on_select {
        if let Mode::Copy(cm) = &mut app.mode {
            cm.set_message("selected — y copies, esc clears");
        }
        return;
    }
    if let Some(t) = text {
        app.copy_text(&t);
    }
    app.mode = Mode::Normal;
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
