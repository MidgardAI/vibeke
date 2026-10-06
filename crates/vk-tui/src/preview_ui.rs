//! Preview extras in the TUI chrome (06 B2/B5/B8):
//!
//! - **`!N` on preview chips and sidebar rows**: pushed `preview.console_error` events count per
//!   preview until the preview is opened (as a pane, a window or through the proxy).
//! - **Sidebar thumbnails** (`ui.sidebar.preview_thumbnails`, off by default): with kitty
//!   graphics, each preview row of a machine on this host gets a tiny image of its latest
//!   screenshot, drawn with unicode placeholders at the right end of the row. The PNG is read
//!   from the local blob store (never fetched from a remote machine), scaled down here to a few
//!   dozen pixels and sent as a raw RGBA virtual placement under ids in the reserved range
//!   [`THUMB_ID_BASE`]`..+`[`ID_SPAN`] (the gallery uses 16 700 001, `vibeke preview show` 16 700 010).

use crate::app::{App, Pending};
use crate::gallery::Reply;
use crate::screen::Grid;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use vk_browser::frame::Rgba;
use vk_proto::model::Preview;

/// First kitty image id for thumbnails; ids `BASE..BASE + ID_SPAN`.
pub const THUMB_ID_BASE: u32 = 16_710_000;
pub const ID_SPAN: u32 = 90;
/// Thumbnail size in cells.
pub const THUMB_COLS: u16 = 4;
/// Largest PNG decoded for a thumbnail.
const MAX_PNG: u64 = 16 << 20;

struct Thumb {
    shot: String,
    rgba: Rgba,
}

#[derive(Default)]
pub struct State {
    /// (machine, preview handle) → console errors since the preview was last opened.
    pub errors: HashMap<(usize, String), u32>,
    thumbs: HashMap<(usize, String), Thumb>,
    /// Previews whose latest screenshot was asked for (cleared by `screenshot.captured`).
    asked: HashSet<(usize, String)>,
    ids: HashMap<(usize, String), u32>,
    next_id: u32,
    /// Image id → (preview, screenshot id) currently held by the terminal.
    sent: HashMap<u32, ((usize, String), String)>,
}

// ---- `!N` --------------------------------------------------------------------------------------

/// A pushed `preview.console_error`.
pub fn on_console_error(app: &mut App, mi: usize, v: &Value) {
    let Some(p) = v["subject"]["preview"].as_str() else {
        return;
    };
    let n = v["data"]["count"].as_u64().unwrap_or(1).clamp(1, 999) as u32;
    let e = app
        .previews_ui
        .errors
        .entry((mi, p.to_string()))
        .or_insert(0);
    *e = e.saturating_add(n);
}

pub fn badge(app: &App, mi: usize, handle: &str) -> Option<u32> {
    app.previews_ui
        .errors
        .get(&(mi, handle.to_string()))
        .copied()
        .filter(|n| *n > 0)
}

/// ` !3` (capped at `!99+`), empty without errors.
pub fn badge_text(app: &App, mi: usize, handle: &str) -> String {
    match badge(app, mi, handle) {
        None => String::new(),
        Some(n) if n > 99 => " !99+".into(),
        Some(n) => format!(" !{n}"),
    }
}

/// The preview was opened: its errors were seen.
pub fn clear(app: &mut App, mi: usize, handle: &str) {
    app.previews_ui.errors.remove(&(mi, handle.to_string()));
}

// ---- thumbnails --------------------------------------------------------------------------------

fn enabled(app: &App) -> bool {
    app.config.ui.sidebar.preview_thumbnails
        && app.sidebar
        && crate::browser::gfx(app) == crate::browser::Gfx::Kitty
}

/// Previews that get a thumbnail: those of machines on this host (their blobs are readable).
fn wanted(app: &App) -> Vec<(usize, Preview)> {
    if !enabled(app) {
        return Vec::new();
    }
    crate::browser::preview_entries(app)
        .into_iter()
        .filter(|(mi, _)| app.machines[*mi].local)
        .collect()
}

/// A pushed `screenshot.captured`: look at that preview's newest screenshot again.
pub fn on_captured(app: &mut App, mi: usize, v: &Value) {
    match v["data"]["preview"].as_str() {
        Some(p) => {
            app.previews_ui.asked.remove(&(mi, p.to_string()));
        }
        None => app.previews_ui.asked.retain(|(m, _)| *m != mi),
    }
}

/// Ask for the newest screenshot of every wanted preview that has no answer yet.
pub fn observe(app: &mut App) {
    for (mi, p) in wanted(app) {
        let key = (mi, p.handle.clone());
        if !app.previews_ui.asked.insert(key) {
            continue;
        }
        app.command_on(
            mi,
            "screenshot.list",
            json!({"preview": p.handle, "limit": 1}),
            Pending::Gallery(Reply::Thumb {
                machine: mi,
                preview: p.handle.clone(),
            }),
        );
    }
}

/// Reply to the `screenshot.list {preview, limit: 1}` of [`observe`].
pub fn on_reply(app: &mut App, mi: usize, preview: String, res: Result<Value, crate::app::RpcErr>) {
    let key = (mi, preview);
    let Some(shot) = res
        .ok()
        .and_then(|v| v["screenshots"].as_array().and_then(|a| a.first().cloned()))
    else {
        app.previews_ui.thumbs.remove(&key);
        return;
    };
    let id = shot["id"].as_str().unwrap_or("").to_string();
    if app
        .previews_ui
        .thumbs
        .get(&key)
        .is_some_and(|t| t.shot == id)
    {
        return;
    }
    let Some(path) = shot["path_on_machine"].as_str().filter(|p| !p.is_empty()) else {
        return;
    };
    let ok = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= MAX_PNG);
    let Some(rgba) = ok
        .then(|| std::fs::read(path).ok())
        .flatten()
        .and_then(|b| vk_browser::frame::decode(&b).ok())
    else {
        return;
    };
    let (cw, ch, _) = crate::browser::cell_geom(app);
    let small = vk_browser::frame::scale_to_fit(&rgba, cw as u32 * THUMB_COLS as u32, ch as u32);
    app.previews_ui.thumbs.insert(
        key,
        Thumb {
            shot: id,
            rgba: small,
        },
    );
}

fn id_for(app: &mut App, key: &(usize, String)) -> u32 {
    if let Some(i) = app.previews_ui.ids.get(key) {
        return *i;
    }
    let i = THUMB_ID_BASE + app.previews_ui.next_id % ID_SPAN;
    app.previews_ui.next_id += 1;
    app.previews_ui.ids.insert(key.clone(), i);
    i
}

/// Transmit new thumbnails and delete the ones no longer shown (called from `App::draw` before
/// composing, like the gallery image).
pub fn before_draw(app: &mut App) {
    observe(app);
    let want: Vec<(usize, String)> = wanted(app)
        .into_iter()
        .map(|(mi, p)| (mi, p.handle))
        .filter(|k| app.previews_ui.thumbs.contains_key(k))
        .collect();
    let mut out = Vec::new();
    // Drop what is no longer wanted (or replaced by a newer screenshot).
    let stale: Vec<u32> = app
        .previews_ui
        .sent
        .iter()
        .filter(|(_, (key, shot))| {
            !want.contains(key)
                || app
                    .previews_ui
                    .thumbs
                    .get(key)
                    .is_none_or(|t| &t.shot != shot)
        })
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        vk_browser::kitty::delete_image(&mut out, id);
        app.previews_ui.sent.remove(&id);
    }
    for key in want {
        let id = id_for(app, &key);
        let Some(t) = app.previews_ui.thumbs.get(&key) else {
            continue;
        };
        if app
            .previews_ui
            .sent
            .get(&id)
            .is_some_and(|(_, s)| *s == t.shot)
        {
            continue;
        }
        let mut h = vk_browser::kitty::Header::new(
            id,
            vk_browser::kitty::PixelFormat::Rgba,
            t.rgba.width,
            t.rgba.height,
        );
        h.virtual_cells = Some((THUMB_COLS, 1));
        h.placement = Some(1);
        // A reused id: the previous image goes first.
        if app.previews_ui.sent.contains_key(&id) {
            vk_browser::kitty::delete_image(&mut out, id);
        }
        vk_browser::kitty::transmit_direct(&mut out, &h, &t.rgba.data);
        let shot = t.shot.clone();
        app.previews_ui.sent.insert(id, (key, shot));
    }
    app.browser.out.extend_from_slice(&out);
}

/// Draw the placeholder cells of the thumbnails at the right end of the sidebar's preview rows.
/// `sx`/`w`: the sidebar's x and width.
pub fn draw_thumbs(app: &App, g: &mut Grid, sx: u16, w: u16) {
    if app.previews_ui.sent.is_empty() || !enabled(app) {
        return;
    }
    let entries = crate::browser::preview_entries(app);
    let rows = crate::draw::sidebar_rows(app);
    let Some(first) = rows.len().checked_sub(entries.len()) else {
        return;
    };
    let need = THUMB_COLS + 1;
    for (k, (mi, p)) in entries.iter().enumerate() {
        let key = (*mi, p.handle.clone());
        let Some(id) = app.previews_ui.ids.get(&key) else {
            continue;
        };
        if app
            .previews_ui
            .sent
            .get(id)
            .is_none_or(|(sk, _)| *sk != key)
        {
            continue;
        }
        let y = (first + k) as u16 + 1;
        if y >= app.size.1 {
            continue;
        }
        // Only where the row's own text leaves room.
        let used: usize = rows[first + k]
            .segs
            .iter()
            .map(|(s, _)| unicode_width::UnicodeWidthStr::width(s.as_str()))
            .sum();
        if used as u16 + need > w {
            continue;
        }
        let x0 = sx + w - need;
        for col in 0..THUMB_COLS {
            g.put_grapheme(
                x0 + col,
                y,
                &crate::browser::placeholder(*id, 0, col),
                crate::browser::id_style(*id),
            );
        }
    }
}

#[cfg(test)]
#[path = "preview_ui_tests.rs"]
mod tests;
