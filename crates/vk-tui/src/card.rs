//! The pop-up card when an agent attaches an image to the focused pane (spec 18 §6).
//!
//! - **When**: `ui.images.popup`, an image attached by an agent (`data.environment = agent` and
//!   `data.taken_by = agent`), in this client's focused pane on the machine it shows, in
//!   `Mode::Normal` with no plugin surface open, and the host terminal focused (when it reports
//!   focus events). The conditions are checked when the event arrives and again when the image
//!   bytes do (a local image once the recent list knows its path, a remote one fetched at the
//!   card's width); if they no longer hold, no card shows.
//! - **Non-modal**: every key, paste and mouse event still goes to the pane. The first key press
//!   or paste the pane receives closes the card ([`on_key_to_pane`], [`close`]); releases,
//!   repeats, modifier-only presses and the prefix key do not. `open_images`
//!   (`prefix+alt+i`) or a click on the card opens its image in the viewer. Closing by typing
//!   leaves the image unseen.
//! - Several images attached before or while it shows: one card for the newest, with a count
//!   line (`3 new images`); the viewer opens with the unseen images first.
//! - It also closes when the focused pane or machine changes, when the mode leaves
//!   Normal/Prefix or a plugin surface opens ([`observe`]), and when its image is deleted.
//! - Layout: at most half the pane each way, in the corner away from the pane cursor, with a
//!   border, the caption, the image (slot `card` on [`crate::image_view`], so the iTerm2
//!   occlusion and repaint rules apply) and `prefix+alt+i open · closes when you type`.
//!   Without graphics it shows the caption text only.

use crate::app::{App, Mode, Pending};
use crate::browser::Gfx;
use crate::gallery::Reply;
use crate::image_view::{self as iv, Placement};
use crate::images;
use crate::screen::{Grid, Rect};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// Narrowest card drawn (border included).
const MIN_W: u16 = 12;

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub machine: usize,
    pub pane: String,
    /// The newest image (screenshot id) and what the event said about it.
    pub id: String,
    pub handle: String,
    pub caption: Option<String>,
    /// Images this card stands for.
    pub count: usize,
    /// Cache key of the image ([`iv::key`]; empty without graphics).
    pub key: String,
    /// False while the image bytes are on their way (nothing is drawn yet).
    pub shown: bool,
}

// ---- when it appears -----------------------------------------------------------------------------

/// The conditions of §6.2 for a card in `pane` of machine `mi`, now.
pub fn eligible(app: &App, mi: usize, pane: &str) -> bool {
    app.config.ui.images.popup
        && mi == app.cur
        && app.focused_pane().as_deref() == Some(pane)
        && matches!(app.mode, Mode::Normal)
        && crate::plugins::surfaces(app).is_empty()
        && (app.host_focused || !app.caps.focus_events)
}

/// A pushed `screenshot.captured` (after [`images::on_captured`]).
pub fn on_captured(app: &mut App, mi: usize, v: &Value) {
    let d = &v["data"];
    if d["environment"].as_str() != Some("agent") || d["taken_by"].as_str() != Some("agent") {
        return;
    }
    let Some(id) = d["id"]
        .as_str()
        .or_else(|| v["subject"]["screenshot"].as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(pane) = v["subject"]["pane"]
        .as_str()
        .or_else(|| d["pane"].as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    if !eligible(app, mi, pane) {
        return;
    }
    let text = |x: &Value| {
        x.as_str()
            .filter(|s| !s.is_empty())
            .map(|s| vk_proto::text::escape_controls(s).into_owned())
    };
    // A card for the same pane (showing, or waiting for its bytes) takes the newest image.
    let prev = app
        .images_ui
        .card
        .take()
        .filter(|c| c.machine == mi && c.pane == pane);
    let count = match &prev {
        Some(c) if c.id == id => c.count,
        Some(c) => c.count + 1,
        None => 1,
    };
    app.images_ui.card = Some(Card {
        machine: mi,
        pane: pane.to_string(),
        id: id.to_string(),
        handle: text(&d["handle"]).unwrap_or_else(|| "image".into()),
        caption: text(&d["caption"]),
        count,
        key: String::new(),
        shown: prev.is_some_and(|c| c.shown),
    });
    load(app);
}

/// Width of the widest image the card can hold in the focused pane (fetch size).
fn max_image_cols(app: &App, pane: &str) -> u16 {
    pane_rect(app, pane).map_or(32, |r| (r.w / 2).saturating_sub(2).max(1))
}

/// Ask for the card's image: read it from disk once the recent list knows its path, or fetch
/// it at the card's width. Without graphics there is nothing to wait for.
fn load(app: &mut App) {
    let Some(c) = app.images_ui.card.as_ref() else {
        return;
    };
    if iv::gfx(app) == Gfx::None {
        ready(app);
        return;
    }
    let (mi, id) = (c.machine, c.id.clone());
    let px = iv::thumb_px(app, max_image_cols(app, &c.pane));
    let key = iv::key(app, mi, &id, px);
    if let Some(c) = app.images_ui.card.as_mut() {
        c.key = key.clone();
    }
    if iv::readable(app, mi) {
        // The event carries no path: wait for the recent list ([`on_recent`]).
        let Some(s) = images::shot(app, mi, &id) else {
            return;
        };
        let (path, exists) = (s.path.clone(), s.exists);
        iv::load_local(app, &key, &path, exists);
    } else {
        iv::fetch(
            app,
            mi,
            &id,
            px,
            &key,
            Pending::Gallery(Reply::Card { key: key.clone() }),
        );
    }
    ready(app);
}

/// The bytes may be here: show the card if the conditions still hold, else drop it.
fn ready(app: &mut App) {
    let Some(c) = app.images_ui.card.as_ref() else {
        return;
    };
    app.dirty = true;
    if c.shown {
        return;
    }
    let arrived = iv::gfx(app) == Gfx::None
        || app.imgview.image(&c.key).is_some()
        || app.imgview.problem(&c.key).is_some();
    if !arrived {
        return;
    }
    let (mi, pane) = (c.machine, c.pane.clone());
    if eligible(app, mi, &pane) {
        if let Some(c) = app.images_ui.card.as_mut() {
            c.shown = true;
        }
    } else {
        app.images_ui.card = None;
    }
}

/// Machine `mi`'s recent list arrived: a local card image's path may be known now.
pub fn on_recent(app: &mut App, mi: usize) {
    if app.images_ui.card.as_ref().is_some_and(|c| c.machine == mi) {
        load(app);
    }
}

/// The reply to the card's fetch.
pub fn on_fetched(app: &mut App, key: &str, res: &Result<Value, crate::app::RpcErr>) {
    iv::on_fetched(app, key, res);
    if app.images_ui.card.as_ref().is_some_and(|c| c.key == key) {
        ready(app);
    }
}

// ---- when it closes ------------------------------------------------------------------------------

/// Close the card (a paste reached the pane). The image stays unseen.
pub fn close(app: &mut App) {
    if app.images_ui.card.take().is_some() {
        app.dirty = true;
    }
}

/// A key on its way to the pane: a press closes the card; releases, repeats and modifier-only
/// presses do not.
pub fn on_key_to_pane(app: &mut App, ev: &KeyEvent) {
    let modifier = matches!(
        ev.key,
        Key::Named(
            NamedKey::LeftShift
                | NamedKey::RightShift
                | NamedKey::LeftControl
                | NamedKey::RightControl
                | NamedKey::LeftAlt
                | NamedKey::RightAlt
                | NamedKey::LeftSuper
                | NamedKey::RightSuper
        )
    );
    if ev.kind == KeyKind::Press && !modifier {
        close(app);
    }
}

/// Before each frame: the card goes once the focus moved to another pane or machine, the mode
/// left Normal/Prefix (a popup opened) or a plugin surface opened.
pub fn observe(app: &mut App) {
    let Some(c) = &app.images_ui.card else {
        return;
    };
    let keep = c.machine == app.cur
        && app.focused_pane().as_deref() == Some(c.pane.as_str())
        && matches!(app.mode, Mode::Normal | Mode::Prefix(_))
        && crate::plugins::surfaces(app).is_empty();
    if !keep {
        close(app);
    }
}

/// Screenshots `ids` of machine `mi` were deleted.
pub(crate) fn on_forgotten(app: &mut App, mi: usize, ids: &[String]) {
    if app
        .images_ui
        .card
        .as_ref()
        .is_some_and(|c| c.machine == mi && ids.contains(&c.id))
    {
        close(app);
    }
}

// ---- opening -------------------------------------------------------------------------------------

/// The card on screen now, if one is.
pub fn showing(app: &App) -> Option<&Card> {
    app.images_ui.card.as_ref().filter(|c| {
        c.shown
            && c.machine == app.cur
            && app.focused_pane().as_deref() == Some(c.pane.as_str())
            && matches!(app.mode, Mode::Normal | Mode::Prefix(_))
    })
}

/// `open_images` or a click while the card shows: its image in the viewer, the other new
/// images after it. False when no card shows.
pub fn open(app: &mut App) -> bool {
    if showing(app).is_none() {
        return false;
    }
    let Some(c) = app.images_ui.card.take() else {
        return false;
    };
    crate::viewer::open(app, (c.machine, c.id), true);
    true
}

/// A left click on the card opens the viewer (and does not reach the pane); other mouse events
/// pass through.
pub fn on_mouse(app: &mut App, me: &crate::event::MouseEvent) -> bool {
    use crate::event::{MouseButton, MouseEventKind};
    if !matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let Some((_, l)) = layout(app) else {
        return false;
    };
    if !inside(l.outer, me.column, me.row) {
        return false;
    }
    open(app)
}

// ---- layout and drawing --------------------------------------------------------------------------

struct Layout {
    /// The card, border included.
    outer: Rect,
    /// Row of the image, or of the hint / loading line.
    image_y: u16,
    /// Graphics are drawn (else the card shows text only).
    graphics: bool,
    placement: Option<Placement>,
}

fn inside(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

fn pane_rect(app: &App, pane: &str) -> Option<Rect> {
    app.pane_rects()
        .into_iter()
        .find(|(p, _)| p == pane)
        .map(|(_, r)| Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        })
}

/// `prefix+alt+i open · closes when you type`, with the bound key.
fn footer(app: &App) -> String {
    let key = app
        .keymap
        .binding_for("open_images")
        .unwrap_or_else(|| "prefix+alt+i".into());
    format!("{key} open · closes when you type")
}

/// `s5 · login page`: the caption, else what the recent list says, else `attached image`.
fn caption_line(app: &App, c: &Card) -> String {
    let what = c
        .caption
        .clone()
        .or_else(|| images::shot(app, c.machine, &c.id).map(|s| s.agent_text()))
        .unwrap_or_else(|| "attached image".into());
    format!("{} · {what}", c.handle)
}

/// ` 📷 attached by claude `.
fn title(app: &App, c: &Card) -> String {
    let who = images::agent(app, c.machine, Some(&c.pane)).unwrap_or_else(|| "agent".into());
    format!(" 📷 attached by {who} ")
}

fn layout(app: &App) -> Option<(&Card, Layout)> {
    let c = showing(app)?;
    let r = pane_rect(app, &c.pane)?;
    let (max_w, max_h) = (r.w / 2, r.h / 2);
    // Border, caption, the count line, the footer.
    let fixed = 2 + 1 + u16::from(c.count > 1) + 1;
    let graphics = iv::gfx(app) != Gfx::None;
    if max_w < MIN_W || max_h < fixed + u16::from(graphics) {
        return None;
    }
    let avail_w = max_w - 2;
    let text_w = [
        caption_line(app, c).width(),
        footer(app).width(),
        title(app, c).width() + 2,
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
    .min(avail_w as usize) as u16;
    let (inner_w, rows, fitted) = if graphics {
        let avail_h = max_h - fixed;
        let area = Rect {
            x: 0,
            y: 0,
            w: avail_w,
            h: avail_h,
        };
        match iv::fit(app, "card", &c.key, area) {
            Some(p) => (p.rect.w.max(text_w), p.rect.h, Some(p)),
            // Loading, or the hint: one line.
            None => (avail_w, 1, None),
        }
    } else {
        (text_w, 0, None)
    };
    let (w, h) = (inner_w + 2, fixed + rows);
    // The corner away from the pane cursor.
    let (cx, cy) = app
        .m()
        .panes
        .get(&c.pane)
        .map_or((0, 0), |b| (b.cursor.col, b.cursor.row));
    let x = if cx >= r.w / 2 { r.x } else { r.x + r.w - w };
    let y = if cy >= r.h / 2 { r.y } else { r.y + r.h - h };
    let outer = Rect { x, y, w, h };
    let image_y = y + 1 + 1 + u16::from(c.count > 1);
    let placement = fitted.map(|mut p| {
        p.rect.x = x + 1;
        p.rect.y = image_y;
        p
    });
    Some((
        c,
        Layout {
            outer,
            image_y,
            graphics,
            placement,
        },
    ))
}

/// The card's image placement this frame, if its image is loaded.
pub fn placements(app: &App) -> Option<Placement> {
    layout(app)?.1.placement
}

/// Draw the card over the focused pane (after panes and floats, before popups). Returns its
/// rectangle, so the pane cursor under it can be hidden.
pub fn draw(app: &App, g: &mut Grid) -> Option<Rect> {
    let (c, l) = layout(app)?;
    let t = app.theme;
    let o = l.outer;
    g.fill(o, t.text());
    let b = t.border(true);
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
    g.put_str(x1, y1, "╯", b, 1);
    g.put_str(
        o.x + 1,
        o.y,
        &title(app, c),
        t.bold(t.accent),
        o.w.saturating_sub(2),
    );
    let (x, w) = (o.x + 1, o.w - 2);
    g.put_str(x, o.y + 1, &caption_line(app, c), t.bold(t.fg), w);
    if c.count > 1 {
        g.put_str(
            x,
            o.y + 2,
            &format!("{} new images", c.count),
            t.s(t.accent),
            w,
        );
    }
    if l.graphics {
        let key = app
            .keymap
            .binding_for("open_images")
            .unwrap_or_else(|| "prefix+alt+i".into());
        match iv::hint(app, &c.key, &format!("{key} to view")) {
            Some(h) => {
                g.put_str(x, l.image_y, &h, t.dim(), w);
            }
            None => match &l.placement {
                Some(p) if iv::draw(app, g, p) => {}
                _ => {
                    g.put_str(x, l.image_y, "loading image…", t.dim(), w);
                }
            },
        }
    }
    g.put_str(x, y1 - 1, &footer(app), t.dim(), w);
    Some(o)
}

#[cfg(test)]
#[path = "card_tests.rs"]
mod tests;
