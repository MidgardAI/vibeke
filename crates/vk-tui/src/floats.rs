//! Floating panes (08 §5, D#782): drawn over the tiling in z order with a frame and title,
//! focused/moved/resized with the mouse (title row drags, the bottom-right corner resizes) or in
//! resize mode (`m` toggles move), hidden per tab (`tab.floats`), floated/embedded from the
//! palette. Geometry is percent of the pane area (`Tab.floating`); the content rect inside the
//! frame is what `App::pane_rects` and `ViewHint` report, so float PTYs get real sizes.

use crate::app::{App, Mode, Pending};
use crate::screen::{Grid, Rect as SRect};
use crossterm::event::{MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::json;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::layout::Rect;
use vk_proto::model::{FloatingPane, Tab};

/// Smallest float frame in cells (a 4×1 content area).
pub const MIN_W: u16 = 6;
pub const MIN_H: u16 = 3;

#[derive(Default)]
pub struct State {
    /// Resize mode on a float: `m` switches h/j/k/l from resizing to moving.
    pub move_mode: bool,
    pub drag: Option<Drag>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    Move,
    Resize,
}

#[derive(Debug, Clone)]
pub struct Drag {
    pub mi: usize,
    pub kind: DragKind,
    pub start: (u16, u16),
    pub orig: FloatingPane,
    pub cur: FloatingPane,
    pub area: Rect,
}

/// A float as drawn this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct VisibleFloat {
    pub pane: String,
    /// Frame (border included).
    pub outer: Rect,
    /// Content area.
    pub inner: Rect,
    pub z: u32,
}

/// The frame rect of a float inside `area`: percent → cells, at least `MIN_W`×`MIN_H`, kept
/// inside the area. `None` when the area can't hold a float.
pub fn outer_rect(f: &FloatingPane, area: Rect) -> Option<Rect> {
    if area.w < MIN_W || area.h < MIN_H {
        return None;
    }
    let cells =
        |pct: f32, total: u16| ((pct.clamp(0.0, 100.0) / 100.0) * total as f32).round() as u16;
    let w = cells(f.w, area.w).clamp(MIN_W, area.w);
    let h = cells(f.h, area.h).clamp(MIN_H, area.h);
    let x = cells(f.x, area.w).min(area.w - w);
    let y = cells(f.y, area.h).min(area.h - h);
    Some(Rect {
        x: area.x + x,
        y: area.y + y,
        w,
        h,
    })
}

pub fn inner_rect(o: Rect) -> Rect {
    Rect {
        x: o.x + 1,
        y: o.y + 1,
        w: o.w.saturating_sub(2),
        h: o.h.saturating_sub(2),
    }
}

/// Visible floats of `tab` in z order (lowest first). None while the tab's floats are hidden or
/// a pane is zoomed (a zoomed float fills the area through the zoom path instead).
pub fn visible_in(tab: &Tab, area: Rect) -> Vec<VisibleFloat> {
    if tab.floats_hidden || tab.zoomed_pane.is_some() {
        return vec![];
    }
    let mut v: Vec<VisibleFloat> = tab
        .floating
        .iter()
        .filter_map(|f| {
            let outer = outer_rect(f, area)?;
            Some(VisibleFloat {
                pane: f.pane.clone(),
                outer,
                inner: inner_rect(outer),
                z: f.z,
            })
        })
        .collect();
    v.sort_by_key(|f| f.z);
    v
}

pub fn visible(app: &App) -> Vec<VisibleFloat> {
    let m = app.m();
    let Some(tid) = m.focus.tab.as_ref() else {
        return vec![];
    };
    match m.model.tabs.iter().find(|t| &t.id == tid) {
        // Plugin popups/overlays float too but are drawn by `crate::plugins`.
        Some(tab) => visible_in(tab, app.pane_area())
            .into_iter()
            .filter(|f| !crate::plugins::is_surface(app, &f.pane))
            .collect(),
        None => vec![],
    }
}

/// The focused pane's float entry, when it is floating in the focused tab.
pub fn focused_float(app: &App) -> Option<FloatingPane> {
    let pane = app.focused_pane()?;
    if crate::plugins::is_surface(app, &pane) {
        return None;
    }
    let tab = app.focused_tab()?;
    tab.floating.into_iter().find(|f| f.pane == pane)
}

fn tab_of_float_mut<'a>(app: &'a mut App, mi: usize, pane: &str) -> Option<&'a mut Tab> {
    app.machines[mi]
        .model
        .tabs
        .iter_mut()
        .find(|t| t.floating.iter().any(|f| f.pane == pane))
}

/// Set a float's geometry locally (drawn at once) and, with `send`, on the server.
pub fn set_rect(app: &mut App, mi: usize, f: &FloatingPane, send: bool) {
    if let Some(tab) = tab_of_float_mut(app, mi, &f.pane)
        && let Some(slot) = tab.floating.iter_mut().find(|x| x.pane == f.pane)
    {
        slot.x = f.x;
        slot.y = f.y;
        slot.w = f.w;
        slot.h = f.h;
    }
    app.dirty = true;
    if send {
        app.command_on(
            mi,
            "pane.float",
            json!({"pane": f.pane, "rect": {"x": f.x, "y": f.y, "w": f.w, "h": f.h}}),
            Pending::Ignore,
        );
    }
}

/// Bring a float to the top (locally and with `pane.float {pane}` on the server).
pub fn raise(app: &mut App, mi: usize, pane: &str) {
    let Some(tab) = tab_of_float_mut(app, mi, pane) else {
        return;
    };
    let top = tab.floating.iter().map(|f| f.z).max().unwrap_or(0);
    let Some(z) = tab.floating.iter().find(|f| f.pane == pane).map(|f| f.z) else {
        return;
    };
    if z == top && tab.floating.iter().filter(|x| x.z == top).count() == 1 {
        return;
    }
    if let Some(f) = tab.floating.iter_mut().find(|f| f.pane == pane) {
        f.z = top + 1;
    }
    app.command_on(mi, "pane.float", json!({"pane": pane}), Pending::Ignore);
}

/// Move by (dx, dy) or resize by (dw, dh) in percent, clamped to the pane area.
pub fn nudge(f: &FloatingPane, area: Rect, kind: DragKind, dx: f32, dy: f32) -> FloatingPane {
    let mut n = f.clone();
    let min_w = MIN_W as f32 * 100.0 / area.w.max(1) as f32;
    let min_h = MIN_H as f32 * 100.0 / area.h.max(1) as f32;
    match kind {
        DragKind::Move => {
            n.x = (f.x + dx).clamp(0.0, (100.0 - f.w).max(0.0));
            n.y = (f.y + dy).clamp(0.0, (100.0 - f.h).max(0.0));
        }
        DragKind::Resize => {
            n.w = (f.w + dx).clamp(min_w.min(100.0), (100.0 - f.x).max(min_w));
            n.h = (f.h + dy).clamp(min_h.min(100.0), (100.0 - f.y).max(min_h));
        }
    }
    n
}

pub fn action(app: &mut App, action: &str) -> bool {
    let cur = app.cur;
    match action {
        "float_new" => {
            let Some(tab) = app.focused_tab() else {
                return true;
            };
            app.command(
                "pane.float",
                json!({"tab": tab.id, "focus": true}),
                Pending::Ignore,
            );
        }
        "toggle_floats" => {
            let Some(tab) = app.focused_tab() else {
                return true;
            };
            if tab.floating.is_empty() {
                app.toast("no floating panes in this tab — prefix+f makes one");
                return true;
            }
            let hiding = !tab.floats_hidden;
            // Optimistic, so the next frame already shows the change.
            if let Some(t) = app.m_mut().model.tabs.iter_mut().find(|t| t.id == tab.id) {
                t.floats_hidden = hiding;
            }
            app.command(
                "tab.floats",
                json!({"tab": tab.id, "visible": !hiding}),
                Pending::Ignore,
            );
            // Keys must not keep going to a pane that just disappeared.
            if hiding
                && let Some(p) = app.focused_pane()
                && tab.floating.iter().any(|f| f.pane == p)
                && let Some(first) = tab.layout.panes().first().cloned()
            {
                app.focus_pane(cur, &first);
            }
        }
        "float_pane" | "embed_pane" => {
            let Some(p) = app.focused_pane() else {
                return true;
            };
            if focused_float(app).is_some() {
                app.command("pane.embed", json!({"pane": p}), Pending::Ignore);
            } else if action == "embed_pane" {
                app.toast("this pane isn't floating");
            } else {
                app.command(
                    "pane.float",
                    json!({"pane": p, "focus": true}),
                    Pending::Ignore,
                );
            }
        }
        _ => return false,
    }
    true
}

/// Resize mode on a focused float: h/j/k/l resize (shift: ×5), `m` toggles moving, `=`
/// recentres at 70%×70%, esc/enter/q leave. False when the focused pane isn't floating.
pub fn resize_key(app: &mut App, ev: &KeyEvent) -> bool {
    let Some(f) = focused_float(app) else {
        app.parity.floats.move_mode = false;
        return false;
    };
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Resize;
        return true;
    }
    let big = ev.mods.shift() || matches!(ev.key, Key::Char(c) if c.is_uppercase());
    let step = if big { 10.0 } else { 2.0 };
    let (dx, dy) = match ev.key {
        Key::Char('h' | 'H') | Key::Named(NamedKey::Left) => (-step, 0.0),
        Key::Char('l' | 'L') | Key::Named(NamedKey::Right) => (step, 0.0),
        Key::Char('k' | 'K') | Key::Named(NamedKey::Up) => (0.0, -step),
        Key::Char('j' | 'J') | Key::Named(NamedKey::Down) => (0.0, step),
        Key::Char('m') => {
            app.parity.floats.move_mode = !app.parity.floats.move_mode;
            app.mode = Mode::Resize;
            return true;
        }
        Key::Char('=') => {
            let mut c = FloatingPane::centred(&f.pane, f.z);
            c.z = f.z;
            let cur = app.cur;
            set_rect(app, cur, &c, true);
            app.mode = Mode::Resize;
            return true;
        }
        Key::Named(NamedKey::Escape | NamedKey::Enter) | Key::Char('q') => {
            app.parity.floats.move_mode = false;
            return true;
        }
        _ => {
            app.mode = Mode::Resize;
            return true;
        }
    };
    let kind = if app.parity.floats.move_mode {
        DragKind::Move
    } else {
        DragKind::Resize
    };
    let n = nudge(&f, app.pane_area(), kind, dx, dy);
    let cur = app.cur;
    set_rect(app, cur, &n, true);
    app.mode = Mode::Resize;
    true
}

/// Mouse on a float frame: press on the title row starts a move, on the bottom-right corner a
/// resize; drag updates locally, release sends `pane.float {rect}`. Presses inside a float's
/// content raise it and fall through (focus + forwarding use `App::pane_rects`, floats first).
pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    let (x, y) = (me.column, me.row);
    if let Some(d) = app.parity.floats.drag.clone() {
        match me.kind {
            MouseEventKind::Drag(CtButton::Left) | MouseEventKind::Up(CtButton::Left) => {
                let dx = (x as f32 - d.start.0 as f32) * 100.0 / d.area.w.max(1) as f32;
                let dy = (y as f32 - d.start.1 as f32) * 100.0 / d.area.h.max(1) as f32;
                let n = nudge(&d.orig, d.area, d.kind, dx, dy);
                let up = matches!(me.kind, MouseEventKind::Up(_));
                set_rect(app, d.mi, &n, up);
                if up {
                    app.parity.floats.drag = None;
                } else if let Some(dr) = app.parity.floats.drag.as_mut() {
                    dr.cur = n;
                }
                return true;
            }
            _ => app.parity.floats.drag = None,
        }
    }
    let Some(f) = visible(app)
        .into_iter()
        .rev()
        .find(|f| f.outer.contains(x, y))
    else {
        return false;
    };
    let cur = app.cur;
    let down = matches!(me.kind, MouseEventKind::Down(CtButton::Left));
    if f.inner.contains(x, y) {
        if matches!(me.kind, MouseEventKind::Down(_)) {
            raise(app, cur, &f.pane);
        }
        return false;
    }
    // The frame belongs to Vibeke: never forwarded to the pane or what's under it.
    if down {
        let corner = x + 1 == f.outer.x + f.outer.w && y + 1 == f.outer.y + f.outer.h;
        let title_row = y == f.outer.y;
        if app.focused_pane().as_deref() != Some(f.pane.as_str()) {
            app.focus_pane(cur, &f.pane);
        }
        raise(app, cur, &f.pane);
        let tab = app.focused_tab();
        let fp = tab.and_then(|t| t.floating.into_iter().find(|x| x.pane == f.pane));
        if let Some(fp) = fp
            && (corner || title_row)
        {
            app.parity.floats.drag = Some(Drag {
                mi: cur,
                kind: if corner {
                    DragKind::Resize
                } else {
                    DragKind::Move
                },
                start: (x, y),
                cur: fp.clone(),
                orig: fp,
                area: app.pane_area(),
            });
        }
    }
    true
}

/// A model update during a drag keeps the dragged geometry (the server hasn't seen it yet).
pub fn on_model(app: &mut App) {
    if let Some(d) = app.parity.floats.drag.clone() {
        set_rect(app, d.mi, &d.cur, false);
    }
}

fn title_of(app: &App, pane: &str) -> String {
    let m = app.m();
    if let Some(r) = m.model.runs.iter().find(|r| r.pane == pane) {
        return r.label().to_string();
    }
    m.model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .map(|p| p.display_title().to_string())
        .unwrap_or_else(|| "float".into())
}

/// Clear the float's rect (tiled cells and image placeholders under it are overwritten) and
/// draw its frame: accent when focused, title on the top row, `◢` resize handle.
pub fn draw_frame(app: &App, g: &mut Grid, f: &VisibleFloat, focused: bool) {
    let t = app.theme;
    let o = f.outer;
    g.fill(
        SRect {
            x: o.x,
            y: o.y,
            w: o.w,
            h: o.h,
        },
        vk_proto::render::Style::default(),
    );
    let b = t.border(focused);
    let (x1, y1) = (o.x + o.w - 1, o.y + o.h - 1);
    for x in o.x..=x1 {
        g.put_str(x, o.y, "─", b, 1);
        g.put_str(x, y1, "─", b, 1);
    }
    for y in o.y..=y1 {
        g.put_str(o.x, y, "│", b, 1);
        g.put_str(x1, y, "│", b, 1);
    }
    g.put_str(o.x, o.y, "╭", b, 1);
    g.put_str(x1, o.y, "╮", b, 1);
    g.put_str(o.x, y1, "╰", b, 1);
    g.put_str(x1, y1, "◢", b, 1);
    let moving = focused && app.parity.floats.move_mode && matches!(app.mode, Mode::Resize);
    let title = format!(
        " {}{} ",
        crate::draw::truncate(&title_of(app, &f.pane), o.w.saturating_sub(8) as usize),
        if moving { " · move" } else { "" }
    );
    let st = if focused { t.bold(t.accent) } else { t.dim() };
    g.put_str(o.x + 2, o.y, &title, st, o.w.saturating_sub(4));
}
