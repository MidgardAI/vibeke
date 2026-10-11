//! The image viewer (spec 18 §5): one image over the pane area with direct actions.
//!
//! - Header `s5 · login page · attached by claude · 2m ago`, the image (slot `viewer`, drawn
//!   through [`crate::image_view`]), and the action line `esc dismiss · o open on this machine ·
//!   x remove · ←/→ older/newer · g gallery · y copy path`.
//! - Opening it counts as asking for the image: a local image is read from disk, a remote one is
//!   fetched at the width of the view (`screenshot.get {inline, thumb}`). Every image it shows
//!   counts as seen.
//! - `x` removes with `screenshot.delete`: an attached image takes a second `x` (shown in the
//!   action line, no dialog), a captured screenshot keeps the gallery's `y` step, and one kept
//!   for a review acceptance needs `!`. After a removal the next image shows, or the viewer
//!   closes when none is left.
//! - `←`/`→` step through every machine's recent images by time, the unseen ones first when it
//!   was opened for them. The order is fixed when the viewer opens (deletions leave it).
//! - Opened by `open_images` (`prefix+shift+b`, palette **View new images**: the newest unseen
//!   image, else the gallery), sidebar image rows and 📷N badges.

use crate::app::{App, Mode, Pending, Popup};
use crate::browser::Gfx;
use crate::drafts::Area;
use crate::gallery::{self, Reply, Scope, Shot, View};
use crate::image_view::{self as iv, Placement};
use crate::images;
use crate::screen::{Grid, Rect};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

#[derive(Debug, Clone, PartialEq)]
pub struct Viewer {
    /// The image shown: (machine, screenshot id).
    pub cur: (usize, String),
    /// `←`/`→` order (older = later), fixed when the viewer opens.
    pub order: Vec<(usize, String)>,
    /// The records of `order`, kept here so a refresh of the bounded recent cache (a new image
    /// pushing an old one out) does not take the image on screen away.
    pub shots: Vec<(usize, Shot)>,
    pub notice: Option<String>,
}

// ---- opening -------------------------------------------------------------------------------------

/// Open the viewer on `target`; `unseen_first` steps through the unseen images before the rest.
/// An image the recent cache does not know opens the gallery on it instead.
pub fn open(app: &mut App, target: (usize, String), unseen_first: bool) {
    if images::shot(app, target.0, &target.1).is_none() {
        gallery::open_on(app, target.0, &target.1);
        return;
    }
    let shots: Vec<(usize, Shot)> = images::ordered(app, unseen_first)
        .into_iter()
        .map(|(mi, s)| (mi, s.clone()))
        .collect();
    let order = shots.iter().map(|(mi, s)| (*mi, s.id.clone())).collect();
    app.images_ui.confirm = None;
    app.images_ui.viewer = Some(Viewer {
        cur: target,
        order,
        shots,
        notice: None,
    });
    app.mode = Mode::Popup(Popup::ImageViewer);
    show(app);
}

/// Open the newest unseen image (stepping through the unseen ones first). False when no unseen
/// image is in the recent cache.
pub fn open_newest_unseen(app: &mut App) -> bool {
    let Some(t) = images::unseen(app)
        .into_iter()
        .find(|(mi, id)| images::shot(app, *mi, id).is_some())
    else {
        return false;
    };
    open(app, t, true);
    true
}

/// `open_images`: the newest unseen image, else the gallery.
pub fn action(app: &mut App, action: &str) -> bool {
    if action != "open_images" {
        return false;
    }
    // While the attach pop-up shows, this opens that card's image (§5.3).
    if !crate::card::open(app) && !open_newest_unseen(app) {
        let mi = app.cur;
        gallery::open(app, mi, Scope::All, View::List, false);
    }
    true
}

fn close(app: &mut App) {
    app.images_ui.viewer = None;
    app.images_ui.confirm = None;
    if matches!(app.mode, Mode::Popup(Popup::ImageViewer)) {
        app.mode = Mode::Normal;
    }
}

/// The current image is on screen: it is seen, and its pixels are asked for.
fn show(app: &mut App) {
    let Some((mi, id)) = app.images_ui.viewer.as_ref().map(|v| v.cur.clone()) else {
        return;
    };
    images::mark_seen(app, mi, &id);
    ensure_image(app);
}

/// Screenshot `id` of machine `mi`: the recent cache's record (the freshest), else the one the
/// viewer kept when it opened.
pub fn shot<'a>(app: &'a App, mi: usize, id: &str) -> Option<&'a Shot> {
    images::shot(app, mi, id).or_else(|| {
        app.images_ui
            .viewer
            .as_ref()?
            .shots
            .iter()
            .find(|(m, s)| *m == mi && s.id == id)
            .map(|(_, s)| s)
    })
}

/// What the viewer shows an image for this frame: (machine, shot, cache key, area it may fill).
fn wanted(app: &App) -> Option<(usize, &Shot, String, Rect)> {
    let v = app.images_ui.viewer.as_ref()?;
    let (mi, id) = (v.cur.0, v.cur.1.as_str());
    let s = shot(app, mi, id)?;
    let a = app.pane_area();
    let lines =
        u16::from(v.notice.is_some()) + u16::from(gallery::too_large_to_open(app, mi, s).is_some());
    let top = a.y + 1 + lines;
    let area = Rect {
        x: a.x + 1,
        y: top,
        w: a.w.saturating_sub(2),
        h: (a.y + a.h).saturating_sub(top + 1),
    };
    let key = iv::key(app, mi, id, iv::thumb_px(app, area.w));
    Some((mi, s, key, area))
}

/// Read (local) or fetch (remote, at the width of the view) the current image.
pub(crate) fn ensure_image(app: &mut App) {
    if !matches!(app.mode, Mode::Popup(Popup::ImageViewer)) || iv::gfx(app) == Gfx::None {
        return;
    }
    let Some((mi, s, key, area)) = wanted(app) else {
        return;
    };
    let (id, path, exists) = (s.id.clone(), s.path.clone(), s.exists);
    if iv::readable(app, mi) {
        iv::load_local(app, &key, &path, exists);
    } else {
        let px = iv::thumb_px(app, area.w);
        iv::fetch(
            app,
            mi,
            &id,
            px,
            &key,
            Pending::Gallery(Reply::Viewer { key: key.clone() }),
        );
    }
}

/// Screenshots `ids` of machine `mi` were deleted: they leave the order; when the current one
/// went, the next image shows, or the viewer closes when none is left.
pub(crate) fn on_forgotten(app: &mut App, mi: usize, ids: &[String]) {
    let Some(v) = &mut app.images_ui.viewer else {
        return;
    };
    let gone = |t: &(usize, String)| t.0 == mi && ids.contains(&t.1);
    let old = std::mem::take(&mut v.order);
    let at = old.iter().position(|t| *t == v.cur).unwrap_or(0);
    let next = old[at..]
        .iter()
        .find(|t| !gone(t))
        .or_else(|| old[..at].iter().rev().find(|t| !gone(t)))
        .cloned();
    v.order = old.into_iter().filter(|t| !gone(t)).collect();
    v.shots.retain(|(m, s)| !(*m == mi && ids.contains(&s.id)));
    if !gone(&v.cur) {
        return;
    }
    match next {
        Some(n) => {
            v.cur = n;
            v.notice = None;
            show(app);
        }
        None => close(app),
    }
}

// ---- keys ----------------------------------------------------------------------------------------

/// `←` (older) or `→` (newer): the next image in the order that the recent cache still knows.
fn step(app: &mut App, older: bool) {
    let Some(v) = app.images_ui.viewer.as_ref() else {
        return;
    };
    let known = |t: &&(usize, String)| shot(app, t.0, &t.1).is_some();
    let next = match v.order.iter().position(|t| *t == v.cur) {
        Some(i) if older => v.order[i + 1..].iter().find(known).cloned(),
        Some(i) => v.order[..i].iter().rev().find(known).cloned(),
        None => v.order.iter().find(known).cloned(),
    };
    let Some(v) = app.images_ui.viewer.as_mut() else {
        return;
    };
    match next {
        Some(n) => {
            v.cur = n;
            app.images_ui.confirm = None;
            show(app);
        }
        None => {
            v.notice = Some(if older {
                "No older image".into()
            } else {
                "No newer image".into()
            });
        }
    }
}

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::ImageViewer);
    if ev.kind == KeyKind::Release {
        return;
    }
    let Some((mi, id)) = app.images_ui.viewer.as_ref().map(|v| v.cur.clone()) else {
        app.mode = Mode::Normal;
        return;
    };
    if let Some(v) = &mut app.images_ui.viewer {
        v.notice = None;
    }
    if images::confirm_key(app, &ev, Some((mi, &id))) == Some(true) {
        return;
    }
    let shot = shot(app, mi, &id).cloned();
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {
            images::mark_seen(app, mi, &id);
            close(app);
            return;
        }
        Key::Char('o') => {
            if let Some(s) = &shot
                && let Some(msg) = gallery::open_shot(app, mi, s)
                && let Some(v) = &mut app.images_ui.viewer
            {
                v.notice = Some(msg);
            }
        }
        Key::Char('x') => images::ask_remove(app, mi, id),
        Key::Named(NamedKey::Left) | Key::Char('h') => step(app, true),
        Key::Named(NamedKey::Right) | Key::Char('l') => step(app, false),
        Key::Char('g') => {
            images::mark_seen(app, mi, &id);
            close(app);
            gallery::open_on(app, mi, &id);
            return;
        }
        Key::Char('y') => {
            if let Some(s) = &shot {
                let txt = format!("{} {}", s.handle, s.path);
                app.set_clipboard(txt.as_bytes(), false);
            }
        }
        _ => {}
    }
    ensure_image(app);
}

// ---- placement and drawing -----------------------------------------------------------------------

/// The viewer's image placement this frame, if its image is loaded.
pub fn placements(app: &App) -> Option<Placement> {
    if !matches!(app.mode, Mode::Popup(Popup::ImageViewer)) {
        return None;
    }
    let (_, _, key, area) = wanted(app)?;
    iv::fit(app, "viewer", &key, area)
}

/// `s5 · login page · attached by claude · devbox · 2m ago`.
pub fn header(app: &App, mi: usize, s: &Shot) -> String {
    let who = images::agent(app, mi, s.pane.as_deref());
    let by = match (s.is_agent(), who) {
        (true, Some(a)) => format!("attached by {a}"),
        (true, None) => "attached by agent".into(),
        (false, Some(a)) if s.label.is_empty() => format!("captured by {a}"),
        (false, Some(a)) => format!("{} · {a}", s.label),
        (false, None) => s.label.clone(),
    };
    let mut parts = vec![s.handle.clone(), images::title(s), by];
    if let Some(m) = app.machines.get(mi).filter(|m| !m.local) {
        parts.push(m.label.clone());
    }
    parts.push(format!("{} ago", gallery::age(s.created_at_ms)));
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

/// The `o` entry of the action line (`o download` in the WASM TUI).
pub fn open_text(wasm: bool) -> &'static str {
    if wasm {
        "o download"
    } else {
        "o open on this machine"
    }
}

/// The action line; `o` is left out for a remote original too large to transfer.
pub fn action_line(open: bool, wasm: bool) -> String {
    let mut v = vec!["esc dismiss"];
    if open {
        v.push(open_text(wasm));
    }
    v.extend(["x remove", "←/→ older/newer", "g gallery", "y copy path"]);
    v.join(" · ")
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = &app.images_ui.viewer else {
        return;
    };
    let t = app.theme;
    let (mi, id) = (v.cur.0, v.cur.1.as_str());
    let Some(s) = shot(app, mi, id) else {
        let mut a = Area::open(app, g, "image");
        a.line(
            "This image is no longer in the recent list — g opens the gallery",
            t.dim(),
        );
        a.footer("esc close · g gallery", t.dim());
        return;
    };
    let mut a = Area::open(app, g, &header(app, mi, s));
    if let Some(n) = &v.notice {
        a.line(n, t.bold(t.yellow));
    }
    let large = gallery::too_large_to_open(app, mi, s);
    if let Some(m) = &large {
        a.line(m, t.bold(t.yellow));
    }
    let key = wanted(app).map(|w| w.2).unwrap_or_default();
    let open = if large.is_some() {
        "y copies the path on the host"
    } else if cfg!(target_arch = "wasm32") {
        "[o] download the image"
    } else {
        "[o] open it on this machine"
    };
    match iv::hint(app, &key, open) {
        Some(h) => a.line(&h, t.dim()),
        None => match placements(app) {
            Some(p) if iv::draw(app, a.g, &p) => a.y = a.y.max(p.rect.y + p.rect.h),
            _ => a.line("loading image…", t.dim()),
        },
    }
    match images::confirm_for(app, mi, id) {
        Some(step) => a.footer(images::confirm_text(step), t.bold(t.yellow)),
        None => a.footer(
            &action_line(large.is_none(), cfg!(target_arch = "wasm32")),
            t.dim(),
        ),
    }
}

#[cfg(test)]
#[path = "viewer_tests.rs"]
mod tests;
