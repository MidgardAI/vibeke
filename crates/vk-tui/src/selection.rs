//! Mouse selection and copy-on-select (03 §11.1, 08 §13 D#748; `clipboard.copy_on_select`,
//! `clipboard.primary_selection`, `clipboard.mouse_select_in_apps`).
//!
//! **Who gets the mouse.** Vibeke selects in a pane whose app has no mouse reporting. Over an
//! app that turned mouse reporting on (agent TUIs, vim, htop), `shift`+drag or `alt`/`option`+
//! drag selects in Vibeke and a plain drag goes to the app — unless
//! `mouse_select_in_apps = "always"`: then every left drag selects in Vibeke and the app gets
//! clicks (sent on release, once it is clear the press was not a drag) and the wheel. Host
//! terminals take some modifiers for their own selection: iTerm2 keeps `option`+drag
//! (and Ghostty `shift`+drag) unless configured otherwise, so each has one of the two left.
//!
//! **Gestures.** A left drag enters copy mode with a character selection from the press to
//! the pointer (highlighted as it grows); a double click selects the word, a triple click the
//! logical line (soft wraps joined); dragging after a double/triple click extends it. Dragging
//! past the pane's top or bottom edge scrolls (and keeps scrolling while the pointer stays
//! there); the wheel scrolls during a drag and the selection follows the pointer. On release:
//!
//! - `copy_on_select = true` (default): the selection is copied at once (soft wraps joined,
//!   trailing blanks trimmed, as `y`), a `copied N chars` toast confirms, copy mode closes;
//! - otherwise copy mode stays open with the selection, for `y` / `enter` (or more keys).
//!
//! A plain click does nothing new. In copy mode a press starts a new selection, a drag extends
//! it, a click clears it and the wheel scrolls. With `primary_selection = true` every copy also
//! sets PRIMARY and a middle click pastes the last copy into the pane.
//!
//! The scrollback viewer has its own mouse selection over its wrapped lines
//! ([`crate::scrollback::on_mouse`]) with the same gestures and [`word_class`].

use crate::app::{App, Mode};
use crossterm::event::{KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};
use vk_proto::layout::Rect;

/// Presses this close together on the same cell count as a double/triple click.
pub const MULTI_CLICK: Duration = Duration::from_millis(500);
/// While the pointer is past a pane edge during a drag, scroll one row this often.
pub const AUTOSCROLL: Duration = Duration::from_millis(60);

#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    /// Left button pressed in a pane (pane, local col, local row), not yet dragged.
    pub press: Option<(String, u16, u16)>,
    /// The current copy-mode selection was made with the mouse and has been dragged (or made
    /// by a double/triple click): the release finishes it.
    pub dragged: bool,
    /// A mouse selection is in progress in copy mode on this pane: drags reach it even outside
    /// the pane.
    pub drag_pane: Option<String>,
    /// Last pointer position of the drag, relative to the pane (may be outside it).
    pub pointer: (i32, i32),
    /// Autoscroll direction while the pointer is past an edge (-1 up, 1 down) and when next.
    pub autoscroll: Option<(i32, Instant)>,
    /// Last press: when, where (pane, col, row) and its click count (1–3).
    pub last_click: Option<(Instant, String, u16, u16, u8)>,
    /// `mouse_select_in_apps = "always"`: a press held back from the app until it is clear it
    /// is a click (sent on release) and not a drag (never sent).
    pub held: Option<(String, MouseEvent)>,
}

/// Character classes for double-click word selection: 0 blank, 1 word, 2 other (selected
/// alone). Word characters are everything but blanks, quotes, brackets, `,` `;` `|` and box
/// drawing, so paths, URLs, `file.rs:12` and `--flags` select whole.
pub fn word_class(g: &str) -> u8 {
    let Some(c) = g.chars().next() else {
        return 0;
    };
    if c.is_whitespace() {
        0
    } else if matches!(
        c,
        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | '|'
    ) || ('\u{2500}'..='\u{259f}').contains(&c)
    {
        2
    } else {
        1
    }
}

/// Count this press as a single, double or triple click.
pub fn click_count(st: &mut State, pane: &str, col: u16, row: u16, now: Instant) -> u8 {
    let n = match &st.last_click {
        Some((at, p, c, r, n))
            if p == pane
                && (*c, *r) == (col, row)
                && now.duration_since(*at) <= MULTI_CLICK
                && *n < 3 =>
        {
            n + 1
        }
        _ => 1,
    };
    st.last_click = Some((now, pane.to_string(), col, row, n));
    n
}

/// Does Vibeke (rather than the pane's app) own a left-button gesture with these modifiers?
pub fn owns(mouse_mode: bool, mods: KeyModifiers) -> bool {
    !mouse_mode || mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
}

fn always(app: &App) -> bool {
    matches!(
        app.config.clipboard.mouse_select_in_apps,
        vk_config::MouseSelectInApps::Always
    )
}

/// Mouse over pane `pane` at `r` (already focused). Returns true when handled here.
pub fn on_mouse(app: &mut App, me: &MouseEvent, pane: &str, r: Rect, mouse_mode: bool) -> bool {
    let (col, row) = (me.column - r.x, me.row - r.y);
    // Copy mode on this pane: the mouse selects and scrolls.
    if let Mode::Copy(cm) = &app.mode
        && cm.pane == pane
    {
        copy_mode_mouse(app, me, pane, r, (col as i32, row as i32));
        return true;
    }
    if !matches!(app.mode, Mode::Normal) {
        app.parity.selection.press = None;
        return false;
    }
    if crate::browser::browser_of(app, app.cur, pane).is_some() {
        return false;
    }
    let mine = owns(mouse_mode, me.modifiers);
    let alw = always(app);
    let primary = app.config.clipboard.primary_selection;
    let st = &mut app.parity.selection;
    match me.kind {
        MouseEventKind::Down(CtButton::Left) => {
            st.held = None;
            if !mine && !alw {
                st.press = None;
                return false;
            }
            let n = if mine {
                click_count(st, pane, col, row, Instant::now())
            } else {
                st.held = Some((pane.to_string(), *me));
                1
            };
            if n >= 2 {
                st.press = None;
                app.enter_copy(None);
                start_drag(app, pane, (col as i32, row as i32));
                if let Mode::Copy(cm) = &mut app.mode {
                    cm.mouse = true;
                    if n == 2 {
                        cm.select_word_at(row, col);
                    } else {
                        cm.select_line_at(row);
                    }
                }
                app.parity.selection.dragged = true;
                app.dirty = true;
                return true;
            }
            app.parity.selection.press = Some((pane.to_string(), col, row));
            true
        }
        MouseEventKind::Drag(CtButton::Left) => {
            let Some((p, c0, r0)) = st.press.take() else {
                return false;
            };
            if p != pane {
                return false;
            }
            // The app never sees a press that became a Vibeke drag.
            st.held = None;
            app.enter_copy(None);
            if let Mode::Copy(cm) = &mut app.mode {
                cm.mouse = true;
                cm.mouse_select(r0, c0, true);
                cm.mouse_select(row, col, false);
            }
            start_drag(app, pane, (col as i32, row as i32));
            app.parity.selection.dragged = true;
            app.dirty = true;
            true
        }
        MouseEventKind::Up(CtButton::Left) => {
            let had = st.press.take().is_some();
            if let Some((p, down)) = st.held.take()
                && p == pane
            {
                // A click in `always` mode: the app gets press and release now.
                app.forward_mouse(pane, &down, r);
                app.forward_mouse(pane, me, r);
                return true;
            }
            had
        }
        MouseEventKind::Down(CtButton::Middle) if mine && primary => {
            match app.last_copy.clone() {
                Some(t) if !t.is_empty() => app.on_paste(t),
                _ => {}
            }
            true
        }
        MouseEventKind::Up(CtButton::Middle) if mine && primary => true,
        _ => false,
    }
}

fn start_drag(app: &mut App, pane: &str, at: (i32, i32)) {
    let st = &mut app.parity.selection;
    st.drag_pane = Some(pane.to_string());
    st.pointer = at;
    st.autoscroll = None;
}

/// A drag or release while a copy-mode selection is in progress: handled here wherever the
/// pointer is (past the pane's edges included). True when handled.
pub fn on_active_drag(app: &mut App, me: &MouseEvent) -> bool {
    let Some(pane) = app.parity.selection.drag_pane.clone() else {
        return false;
    };
    if !matches!(&app.mode, Mode::Copy(cm) if cm.pane == pane) {
        app.parity.selection.drag_pane = None;
        app.parity.selection.autoscroll = None;
        return false;
    }
    if !matches!(
        me.kind,
        MouseEventKind::Drag(CtButton::Left) | MouseEventKind::Up(CtButton::Left)
    ) {
        return false;
    }
    let Some((_, r)) = app.pane_rects().into_iter().find(|(p, _)| *p == pane) else {
        return false;
    };
    let at = (me.column as i32 - r.x as i32, me.row as i32 - r.y as i32);
    copy_mode_mouse(app, me, &pane, r, at);
    true
}

/// The mouse in copy mode on `pane` (at `at`, relative to the pane and maybe outside it).
fn copy_mode_mouse(app: &mut App, me: &MouseEvent, pane: &str, r: Rect, at: (i32, i32)) {
    let (col, row) = clamp_to(r, at);
    match me.kind {
        MouseEventKind::Down(CtButton::Left) => {
            let n = click_count(&mut app.parity.selection, pane, col, row, Instant::now());
            if let Mode::Copy(cm) = &mut app.mode {
                match n {
                    1 => {
                        cm.mouse_select(row, col, true);
                    }
                    2 => {
                        cm.select_word_at(row, col);
                    }
                    _ => {
                        cm.select_line_at(row);
                    }
                }
                cm.mouse = true;
            }
            start_drag(app, pane, at);
            app.parity.selection.dragged = n >= 2;
        }
        MouseEventKind::Drag(CtButton::Left) => {
            app.parity.selection.pointer = at;
            app.parity.selection.dragged = true;
            let dir = edge(r, at);
            if dir != 0 && app.parity.selection.autoscroll.is_none() {
                // Past the edge: scroll now, then on a timer while the pointer stays there.
                scroll_by(app, dir);
            }
            app.parity.selection.autoscroll =
                (dir != 0).then(|| (dir, Instant::now() + AUTOSCROLL));
            if let Mode::Copy(cm) = &mut app.mode {
                cm.mouse_select(row, col, false);
            }
        }
        MouseEventKind::Up(CtButton::Left) => {
            let st = &mut app.parity.selection;
            st.drag_pane = None;
            st.autoscroll = None;
            if !std::mem::take(&mut st.dragged) {
                // A click just moves the cursor.
                if let Mode::Copy(cm) = &mut app.mode {
                    cm.clear_selection();
                }
            } else {
                finish_drag(app);
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = matches!(me.kind, MouseEventKind::ScrollUp);
            wheel(app, up, 3);
        }
        _ => {}
    }
    app.dirty = true;
}

/// Pane-relative position clamped into the pane.
fn clamp_to(r: Rect, (x, y): (i32, i32)) -> (u16, u16) {
    (
        x.clamp(0, r.w.saturating_sub(1) as i32) as u16,
        y.clamp(0, r.h.saturating_sub(1) as i32) as u16,
    )
}

/// -1 above the pane, 1 below it, 0 inside.
fn edge(r: Rect, (_, y): (i32, i32)) -> i32 {
    if y < 0 {
        -1
    } else if y >= r.h as i32 {
        1
    } else {
        0
    }
}

fn scroll_by(app: &mut App, dir: i32) {
    wheel(app, dir < 0, 1);
}

/// Scroll copy mode; during a drag the selection end follows the pointer.
fn wheel(app: &mut App, up: bool, n: usize) {
    let Mode::Copy(cm) = &mut app.mode else {
        return;
    };
    let out = cm.wheel(up, n);
    let Mode::Copy(cm) = std::mem::replace(&mut app.mode, Mode::Normal) else {
        unreachable!()
    };
    app.copy_outcome(cm, out);
    let st = &app.parity.selection;
    if let Some(pane) = st.drag_pane.clone()
        && st.dragged
        && let Some((_, r)) = app.pane_rects().into_iter().find(|(p, _)| *p == pane)
    {
        let (col, row) = clamp_to(r, st.pointer);
        if let Mode::Copy(cm) = &mut app.mode {
            cm.mouse_select(row, col, false);
        }
    }
}

/// Autoscroll timer while the pointer is past an edge (03 §11.1).
pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    if let Some((_, at)) = app.parity.selection.autoscroll {
        d.at("selection_autoscroll", at);
    }
}

pub fn tick(app: &mut App, now: Instant) {
    let Some((dir, at)) = app.parity.selection.autoscroll else {
        return;
    };
    if now < at {
        return;
    }
    if !matches!(app.mode, Mode::Copy(_)) || app.parity.selection.drag_pane.is_none() {
        app.parity.selection.autoscroll = None;
        return;
    }
    scroll_by(app, dir);
    app.parity.selection.autoscroll = Some((dir, now + AUTOSCROLL));
    app.dirty = true;
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
