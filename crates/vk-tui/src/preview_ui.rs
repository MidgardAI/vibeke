//! Preview extras in the TUI chrome (06 B2/B5/B8):
//!
//! - **`!N` on preview chips and sidebar rows**: pushed `preview.console_error` events count per
//!   preview until the preview is opened (as a pane, a window or through the proxy).
//! - **Sidebar thumbnails** (`ui.sidebar.preview_thumbnails`, off by default): with kitty
//!   graphics or iTerm2 inline images, each preview row gets a tiny image of its latest
//!   screenshot at the right end of the row. The image goes through [`crate::image_view`]
//!   (spec 18 §3.6) under the slot `thumb:<machine>:<preview>`. A machine on this host is read
//!   from the local blob store and scaled down here to a few dozen pixels; any other machine (and
//!   the WASM TUI) fetches a `screenshot.get {inline, thumb}` of that size.

use crate::app::{App, Pending};
use crate::browser::Gfx;
use crate::gallery::Reply;
use crate::image_view::{self, Placement, Problem};
use crate::screen::{Grid, Rect};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use vk_proto::model::Preview;

/// Thumbnail size in cells.
pub const THUMB_COLS: u16 = 4;
/// Largest PNG decoded for a thumbnail.
const MAX_PNG: u64 = 16 << 20;

/// The newest screenshot of one preview and where its thumbnail image is cached.
struct Thumb {
    shot: String,
    /// Key in the image cache: `thumb:<machine>:<shot>` (scaled down from disk) or the fetched
    /// image's [`image_view::key`].
    key: String,
    /// The PNG on this machine's disk (machines on this host only).
    path: String,
}

#[derive(Default)]
pub struct State {
    /// (machine, preview handle) → console errors since the preview was last opened.
    pub errors: HashMap<(usize, String), u32>,
    thumbs: HashMap<(usize, String), Thumb>,
    /// Previews whose latest screenshot was asked for (cleared by `screenshot.captured`).
    asked: HashSet<(usize, String)>,
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
    app.config.ui.sidebar.preview_thumbnails && app.sidebar && image_view::gfx(app) != Gfx::None
}

/// Previews that get a thumbnail.
fn wanted(app: &App) -> Vec<(usize, Preview)> {
    if !enabled(app) {
        return Vec::new();
    }
    crate::browser::preview_entries(app)
        .into_iter()
        // `[preview] inline_thumbnails` (a trusted repo's value for its previews, 08 §11.1).
        .filter(|(mi, p)| crate::repo_preview::for_preview(app, *mi, p).inline_thumbnails)
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

/// Screenshots `ids` of machine `mi` were deleted: a preview whose thumbnail showed one loses
/// it and looks for its newest screenshot again.
pub fn on_forgotten(app: &mut App, mi: usize, ids: &[String]) {
    let st = &mut app.previews_ui;
    let gone: Vec<(usize, String)> = st
        .thumbs
        .iter()
        .filter(|((m, _), t)| *m == mi && ids.contains(&t.shot))
        .map(|(k, _)| k.clone())
        .collect();
    for k in gone {
        st.thumbs.remove(&k);
        st.asked.remove(&k);
    }
}

/// Machine `mi` disconnected: list requests on the old connection get no reply, so its
/// previews are asked for again.
pub fn on_disconnected(app: &mut App, mi: usize) {
    app.previews_ui.asked.retain(|(m, _)| *m != mi);
}

/// Ask for the newest screenshot of every wanted preview that has no answer yet, and make sure
/// the image of the one we know is loaded (called from `App::draw_bytes` before the image layer).
pub fn observe(app: &mut App) {
    for (mi, p) in wanted(app) {
        let key = (mi, p.handle.clone());
        if app.previews_ui.asked.insert(key.clone()) {
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
        ensure(app, &key);
    }
}

/// Load or fetch the thumbnail image of a preview's newest screenshot unless it is cached (or
/// known to be unusable, or already on its way).
fn ensure(app: &mut App, k: &(usize, String)) {
    let Some(t) = app.previews_ui.thumbs.get(k) else {
        return;
    };
    let (shot, key, path) = (t.shot.clone(), t.key.clone(), t.path.clone());
    if image_view::readable(app, k.0) {
        load_local(app, &key, &path);
    } else {
        let px = image_view::thumb_px(app, THUMB_COLS);
        image_view::fetch(
            app,
            k.0,
            &shot,
            px,
            &key,
            Pending::Gallery(Reply::ThumbImage { key: key.clone() }),
        );
    }
}

/// Read the PNG at `path`, scale it down to the thumbnail size and cache it as `key`. The
/// full-size image of the same screenshot (gallery, viewer) has another key and stays as it is.
fn load_local(app: &mut App, key: &str, path: &str) {
    if app.imgview.image(key).is_some() || app.imgview.problem(key).is_some() {
        return;
    }
    let ok = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= MAX_PNG);
    let Some(rgba) = ok
        .then(|| std::fs::read(path).ok())
        .flatten()
        .and_then(|b| vk_browser::frame::decode(&b).ok())
    else {
        app.imgview.problems.insert(key.into(), Problem::Gone);
        return;
    };
    let (cw, ch, _) = crate::browser::cell_geom(app);
    let small = vk_browser::frame::scale_to_fit(&rgba, cw as u32 * THUMB_COLS as u32, ch as u32);
    match vk_browser::frame::encode_png(small.width, small.height, &small.data, true) {
        Ok(png) => app.imgview.put(key.into(), png),
        Err(e) => {
            app.imgview
                .problems
                .insert(key.into(), Problem::Failed(e.to_string()));
        }
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
    if id.is_empty()
        || app
            .previews_ui
            .thumbs
            .get(&key)
            .is_some_and(|t| t.shot == id)
    {
        return;
    }
    let path = shot["path_on_machine"].as_str().unwrap_or("").to_string();
    let image_key = if image_view::readable(app, mi) {
        // Not `<machine>:<id>`: that one is the full-size image the gallery may hold.
        format!("thumb:{mi}:{id}")
    } else {
        let px = image_view::thumb_px(app, THUMB_COLS);
        image_view::key(app, mi, &id, px)
    };
    app.previews_ui.thumbs.insert(
        key.clone(),
        Thumb {
            shot: id,
            key: image_key,
            path,
        },
    );
    ensure(app, &key);
}

/// Reply to the `screenshot.get {inline, thumb}` of a remote thumbnail.
pub fn on_image(app: &mut App, key: &str, res: &Result<Value, crate::app::RpcErr>) {
    let _ = image_view::on_fetched(app, key, res);
}

/// The thumbnails of the sidebar's preview rows (sidebar at `sx`, `w` wide) whose image is
/// loaded, at the right end of each row where the row's own text leaves room.
fn thumb_placements(app: &App, sx: u16, w: u16) -> Vec<Placement> {
    if !enabled(app) || app.previews_ui.thumbs.is_empty() {
        return Vec::new();
    }
    let entries = crate::browser::preview_entries(app);
    let need = THUMB_COLS + 1;
    let mut v = Vec::new();
    for (row, r) in crate::draw::sidebar_rows(app).iter().enumerate() {
        // Preview rows only (not an expanded row's chips).
        let Some((mi, p)) = r
            .preview
            .as_ref()
            .filter(|_| r.chips.is_empty())
            .and_then(|(mi, id)| entries.iter().find(|(m, p)| m == mi && &p.id == id))
        else {
            continue;
        };
        let Some(t) = app.previews_ui.thumbs.get(&(*mi, p.handle.clone())) else {
            continue;
        };
        let y = row as u16 + 1;
        if y >= app.size.1 {
            continue;
        }
        // Only where the row's own text leaves room.
        let used: usize = r
            .segs
            .iter()
            .map(|(s, _)| unicode_width::UnicodeWidthStr::width(s.as_str()))
            .sum();
        if used as u16 + need > w {
            continue;
        }
        let area = Rect {
            x: sx + w - need,
            y,
            w: THUMB_COLS,
            h: 1,
        };
        let slot = format!("thumb:{mi}:{}", p.handle);
        v.extend(image_view::fit(app, &slot, &t.key, area));
    }
    v
}

/// Every thumbnail of this frame (for [`image_view::placements`]).
pub fn placements(app: &App) -> Vec<Placement> {
    match crate::chrome::sidebar_x(app) {
        Some(sx) => thumb_placements(app, sx, app.sidebar_w),
        None => Vec::new(),
    }
}

/// Draw the cells of the thumbnails at the right end of the sidebar's preview rows.
/// `sx`/`w`: the sidebar's x and width.
pub fn draw_thumbs(app: &App, g: &mut Grid, sx: u16, w: u16) {
    for p in thumb_placements(app, sx, w) {
        image_view::draw(app, g, &p);
    }
}

#[cfg(test)]
#[path = "preview_ui_tests.rs"]
mod tests;
