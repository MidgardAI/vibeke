//! One drawing layer for the TUI's own image views (spec 18 §3): the gallery and its diff
//! view, the image viewer, the attach pop-up and sidebar preview thumbnails.
//!
//! Views describe **placements** with pure functions of the app state (a slot such as
//! `gallery`, `diff` or `thumb:<machine>:<preview>`, an image key and a cell rectangle);
//! [`placements`] collects them. The protocol follows [`gfx`]:
//!
//! - **Kitty**: each slot gets its own image id from [`ID_BASE`]`..+`[`ID_SPAN`], so several
//!   views can be on screen at once. A PNG is transmitted once per id and size (before the
//!   grid diff, [`before_draw`]) and its cells are unicode placeholders ([`draw`]). Ids that are
//!   no longer placed are deleted.
//! - **iTerm2**: the cells stay blank; after the frame is written the PNG is sent with OSC 1337
//!   over the rectangle ([`after_write`]). An inline image is not part of the grid, so
//!   [`after_compose`] hides a placement while anything in the composed frame overlaps it, and
//!   marks the cells of hidden, moved or removed images for a repaint ([`App::invalidate`]).
//! - **None** (also inside tmux, §3.5): the view shows a text hint ([`hint`]).
//!
//! Image bytes (PNG) live in a small cache keyed by [`key`]: `<machine>:<id>` for an image read
//! from this machine's disk, `<machine>:<id>@<px>` for one fetched with `screenshot.get
//! {inline, thumb: px}` (the transfer stays the size of the placement).

use crate::app::{App, Pending, RpcErr};
use crate::browser::Gfx;
use crate::screen::{Cell, Grid, Rect};
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// First kitty image id for image views; ids `ID_BASE..ID_BASE + ID_SPAN` (above the gallery's
/// old id 16 700 001 and `vibeke preview show`'s 16 700 010, below 2^24 so no MSB diacritic is
/// needed).
pub const ID_BASE: u32 = 16_720_000;
pub const ID_SPAN: u32 = 256;
/// Largest image decoded or transmitted.
pub const MAX_IMAGE: usize = 16 << 20;
/// Cached images (PNG bytes).
const CACHE: usize = 16;

/// Where one image goes this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// The view and slot (`gallery`, `diff`, `viewer`, `card`, `thumb:<machine>:<preview>`).
    pub slot: String,
    /// Image key in the cache ([`key`]).
    pub key: String,
    pub rect: Rect,
}

/// Why a capable terminal still shows no image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// Larger than [`MAX_IMAGE`], or the server would not inline it.
    TooLarge(String),
    /// The file is gone (retention).
    Gone,
    /// The transfer failed.
    Failed(String),
}

#[derive(Debug, Default)]
pub struct State {
    images: Vec<(String, Vec<u8>)>,
    pub fetching: HashSet<String>,
    pub problems: HashMap<String, Problem>,
    /// Inside tmux: no image views (§3.5).
    pub tmux: bool,
    /// Kitty: slot → image id.
    ids: HashMap<String, u32>,
    /// Kitty: image id → (key, cols, rows) the terminal holds.
    kitty: HashMap<u32, (String, u16, u16)>,
    /// iTerm2: slot → (key, rect) on screen.
    iterm: HashMap<String, (String, Rect)>,
    /// iTerm2: images to send after this frame is written.
    queued: Vec<(String, Rect)>,
    /// Keys every view placed on the last frame, covered or not (never evicted).
    placed: HashSet<String>,
    /// Directory for `t=t` temp files (`None`: `$TMPDIR/vibeke-gfx-<uid>`; tests point it at a
    /// temp dir of their own).
    pub gfx_dir: Option<std::path::PathBuf>,
}

impl State {
    pub fn new() -> State {
        State {
            tmux: std::env::var_os("TMUX").is_some_and(|v| !v.is_empty()),
            ..Default::default()
        }
    }

    pub fn image(&self, key: &str) -> Option<&[u8]> {
        self.images
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, d)| d.as_slice())
    }

    /// Cache `data` under `key`. The oldest image goes when the cache is full, unless a view
    /// placed it on the last frame, even under a toast or menu (a view would lose it until the
    /// next key, and a remote one is only fetched when asked for).
    pub fn put(&mut self, key: String, data: Vec<u8>) {
        self.problems.remove(&key);
        self.images.retain(|(k, _)| *k != key);
        self.images.push((key, data));
        if self.images.len() > CACHE {
            let shown: HashSet<&str> = self
                .kitty
                .values()
                .map(|(k, _, _)| k.as_str())
                .chain(self.iterm.values().map(|(k, _)| k.as_str()))
                .chain(self.placed.iter().map(String::as_str))
                .collect();
            let last = self.images.len() - 1;
            if let Some(i) = (0..last).find(|i| !shown.contains(self.images[*i].0.as_str())) {
                self.images.remove(i);
            }
        }
    }

    /// Forget every image of screenshot `id` on machine `mi` (it was deleted): full size,
    /// fetched at any size, and the sidebar thumbnail.
    pub fn forget(&mut self, mi: usize, id: &str) {
        let local = format!("{mi}:{id}");
        let remote = format!("{local}@");
        let thumb = format!("thumb:{local}");
        let gone = |k: &String| *k == local || *k == thumb || k.starts_with(&remote);
        self.images.retain(|(k, _)| !gone(k));
        self.problems.retain(|k, _| !gone(k));
    }

    pub fn problem(&self, key: &str) -> Option<&Problem> {
        self.problems.get(key)
    }
}

/// The protocol image views use: the host's, except inside tmux.
pub fn gfx(app: &App) -> Gfx {
    if app.imgview.tmux {
        Gfx::None
    } else {
        crate::browser::gfx(app)
    }
}

/// Is machine `mi` this machine (its blobs are readable from disk)? The WASM TUI never is.
pub fn readable(app: &App, mi: usize) -> bool {
    !cfg!(target_arch = "wasm32") && app.machines.get(mi).is_some_and(|m| m.local)
}

/// Thumbnail width to fetch for a placement `cols` cells wide, rounded up to 64 px so a small
/// layout change does not fetch again (the server clamps to 64..=1024).
pub fn thumb_px(app: &App, cols: u16) -> u32 {
    let (cw, _, _) = crate::browser::cell_geom(app);
    (cols as u32 * cw as u32)
        .div_ceil(64)
        .saturating_mul(64)
        .clamp(64, 1024)
}

/// Cache key of screenshot `id` on machine `mi`: read from disk (`px` unused) or fetched at
/// `px` pixels wide.
pub fn key(app: &App, mi: usize, id: &str, px: u32) -> String {
    if readable(app, mi) {
        format!("{mi}:{id}")
    } else {
        format!("{mi}:{id}@{px}")
    }
}

/// Read a local image into the cache unless it is there (or known to be unusable).
pub fn load_local(app: &mut App, key: &str, path: &str, exists: bool) {
    if app.imgview.image(key).is_some() || app.imgview.problem(key).is_some() {
        return;
    }
    if !exists || path.is_empty() {
        app.imgview.problems.insert(key.into(), Problem::Gone);
        return;
    }
    match std::fs::metadata(path) {
        Ok(m) if m.len() as usize > MAX_IMAGE => {
            app.imgview.problems.insert(
                key.into(),
                Problem::TooLarge(format!("{} MiB", m.len() >> 20)),
            );
        }
        Ok(_) => match std::fs::read(path) {
            Ok(data) => app.imgview.put(key.into(), data),
            Err(e) => {
                app.imgview
                    .problems
                    .insert(key.into(), Problem::Failed(e.to_string()));
            }
        },
        Err(_) => {
            app.imgview.problems.insert(key.into(), Problem::Gone);
        }
    }
}

/// Fetch screenshot `id` from machine `mi` at `px` pixels wide (`screenshot.get {inline,
/// thumb}`), unless it is cached, known to be unusable or already on its way. `pending`
/// routes the reply; its handler calls [`on_fetched`].
pub fn fetch(app: &mut App, mi: usize, id: &str, px: u32, key: &str, pending: Pending) {
    // Offline: nothing is sent, so nothing is marked as on its way (it is asked for again on a
    // later frame once the machine is back).
    if app.imgview.image(key).is_some()
        || app.imgview.problem(key).is_some()
        || !app.machines.get(mi).is_some_and(|m| m.connected())
        || !app.imgview.fetching.insert(key.to_string())
    {
        return;
    }
    app.command_on(
        mi,
        "screenshot.get",
        json!({"id": id, "inline": true, "thumb": px}),
        pending,
    );
}

/// Machine `mi` disconnected: replies to its fetches will never arrive, so they may be asked
/// for again (every key of the machine starts with `<mi>:`).
pub fn on_disconnected(app: &mut App, mi: usize) {
    let prefix = format!("{mi}:");
    app.imgview.fetching.retain(|k| !k.starts_with(&prefix));
}

/// The reply to a fetch (or another call that answers with `data_b64`). Stores the image, or
/// records and returns why there is none.
pub fn on_fetched(app: &mut App, key: &str, res: &Result<Value, RpcErr>) -> Option<Problem> {
    app.imgview.fetching.remove(key);
    let ok = res.as_ref().ok();
    let data = ok
        .and_then(|x| x.get("data_b64").and_then(Value::as_str))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
    let problem = match (data, res) {
        (Some(d), _) if d.len() <= MAX_IMAGE => {
            app.imgview.put(key.to_string(), d);
            return None;
        }
        (Some(d), _) => Problem::TooLarge(format!("{} MiB", d.len() >> 20)),
        (None, Err(e)) => Problem::Failed(e.message.clone()),
        (None, Ok(x)) => match x.get("inline_skipped").and_then(Value::as_str) {
            Some(s) if s.contains("unreadable") => Problem::Gone,
            Some(s) => Problem::TooLarge(s.to_string()),
            None => Problem::Failed("no image data".into()),
        },
    };
    app.imgview
        .problems
        .insert(key.to_string(), problem.clone());
    Some(problem)
}

/// The hint a view shows instead of the image: `None` while the image can still be drawn
/// (or is loading). `open` names the action that still works (`o open locally`).
pub fn hint(app: &App, key: &str, open: &str) -> Option<String> {
    if gfx(app) == Gfx::None {
        return Some(if app.imgview.tmux {
            format!("Images are not shown inside tmux — {open}")
        } else {
            format!("This terminal shows no graphics — {open}")
        });
    }
    app.imgview.problem(key).map(|p| match p {
        Problem::TooLarge(s) => format!("The image is too large to show ({s}) — {open}"),
        Problem::Gone => "The image file is gone (retention)".to_string(),
        Problem::Failed(e) => format!("The image could not be loaded: {e} — {open}"),
    })
}

// ---- geometry ------------------------------------------------------------------------------------

/// Cells for an image of `w × h` px fitted into `max_c × max_r` cells (aspect kept).
pub fn fit_cells(app: &App, w: u32, h: u32, max_c: u16, max_r: u16) -> (u16, u16) {
    if w == 0 || h == 0 || max_c == 0 || max_r == 0 {
        return (0, 0);
    }
    let (cw, ch, _) = crate::browser::cell_geom(app);
    let sx = (max_c as f64 * cw as f64) / w as f64;
    let sy = (max_r as f64 * ch as f64) / h as f64;
    let s = sx.min(sy);
    let c = ((w as f64 * s) / cw as f64).floor().max(1.0) as u16;
    let r = ((h as f64 * s) / ch as f64).floor().max(1.0) as u16;
    (c.min(max_c), r.min(max_r))
}

/// PNG dimensions from the IHDR chunk.
pub fn png_size(d: &[u8]) -> Option<(u32, u32)> {
    if d.len() < 24 || &d[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(d[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(d[20..24].try_into().ok()?);
    Some((w, h))
}

/// A placement for the cached image `key` fitted into `area` (top-left aligned), if the
/// image is loaded and image views are drawn at all.
pub fn fit(app: &App, slot: &str, key: &str, area: Rect) -> Option<Placement> {
    if gfx(app) == Gfx::None {
        return None;
    }
    let (w, h) = png_size(app.imgview.image(key)?)?;
    let (c, r) = fit_cells(app, w, h, area.w, area.h);
    if c == 0 || r == 0 {
        return None;
    }
    Some(Placement {
        slot: slot.into(),
        key: key.into(),
        rect: Rect {
            x: area.x,
            y: area.y,
            w: c,
            h: r,
        },
    })
}

/// Every placement of this frame, from every image view.
pub fn placements(app: &App) -> Vec<Placement> {
    if gfx(app) == Gfx::None {
        return Vec::new();
    }
    let mut v = Vec::new();
    v.extend(crate::gallery::placements(app));
    v.extend(crate::preview_ui::placements(app));
    v.extend(crate::viewer::placements(app));
    v.extend(crate::card::placements(app));
    v
}

// ---- kitty ---------------------------------------------------------------------------------------

fn alloc_id(st: &mut State, slot: &str) -> Option<u32> {
    if let Some(id) = st.ids.get(slot) {
        return Some(*id);
    }
    let used: HashSet<u32> = st.ids.values().copied().collect();
    let id =
        (ID_BASE..ID_BASE + ID_SPAN).find(|i| !used.contains(i) && !st.kitty.contains_key(i))?;
    st.ids.insert(slot.to_string(), id);
    Some(id)
}

/// Kitty: transmit new or resized images and delete the ones no longer placed. Called from
/// `App::draw_bytes` before composing (the output goes before the grid diff).
pub fn before_draw(app: &mut App) {
    let placed = placements(app);
    app.imgview.placed = placed.iter().map(|p| p.key.clone()).collect();
    let want = if gfx(app) == Gfx::Kitty {
        placed
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    // Slots no longer placed give their id back; the terminal's copy goes.
    let live: HashSet<&str> = want.iter().map(|p| p.slot.as_str()).collect();
    app.imgview
        .ids
        .retain(|slot, _| live.contains(slot.as_str()));
    let held: HashSet<u32> = app.imgview.ids.values().copied().collect();
    let stale: Vec<u32> = app
        .imgview
        .kitty
        .keys()
        .copied()
        .filter(|id| !held.contains(id))
        .collect();
    for id in stale {
        vk_browser::kitty::delete_image(&mut out, id);
        app.imgview.kitty.remove(&id);
    }
    for p in &want {
        let Some(id) = alloc_id(&mut app.imgview, &p.slot) else {
            continue;
        };
        let have = (p.key.clone(), p.rect.w, p.rect.h);
        if app.imgview.kitty.get(&id) == Some(&have) {
            continue;
        }
        let Some(data) = app.imgview.image(&p.key).map(<[u8]>::to_vec) else {
            continue;
        };
        if app.imgview.kitty.remove(&id).is_some() {
            vk_browser::kitty::delete_image(&mut out, id);
        }
        let mut h = vk_browser::kitty::Header::new(id, vk_browser::kitty::PixelFormat::Png, 0, 0);
        h.virtual_cells = Some((p.rect.w, p.rect.h));
        h.placement = Some(1);
        transmit_png(app, &mut out, &h, &data);
        app.imgview.kitty.insert(id, have);
    }
    app.browser.out.extend_from_slice(&out);
}

/// Draw placement `p`'s cells: kitty placeholders once its image is transmitted, blank cells
/// for iTerm2 (the image is sent after the frame). Returns whether the image will show.
pub fn draw(app: &App, g: &mut Grid, p: &Placement) -> bool {
    match gfx(app) {
        Gfx::Kitty => {
            let Some(id) = app.imgview.ids.get(&p.slot).copied() else {
                return false;
            };
            if app
                .imgview
                .kitty
                .get(&id)
                .is_none_or(|(k, c, r)| *k != p.key || *c != p.rect.w || *r != p.rect.h)
            {
                return false;
            }
            for row in 0..p.rect.h {
                for col in 0..p.rect.w {
                    g.put_grapheme(
                        p.rect.x + col,
                        p.rect.y + row,
                        &crate::browser::placeholder(id, row, col),
                        crate::browser::id_style(id),
                    );
                }
            }
            true
        }
        Gfx::Iterm => {
            g.fill(p.rect, app.theme.text());
            true
        }
        Gfx::None => false,
    }
}

/// Transmit a PNG for `h`. When the host terminal is on this machine (not behind SSH) the PNG
/// goes through a temp file the terminal reads and deletes (`t=t`, 06 B8) instead of base64
/// through the pty; otherwise (or when no private temp file can be made, or
/// `VIBEKE_GFX_TRANSFER=direct`) it is chunked `t=d`.
pub fn transmit_png(app: &mut App, out: &mut Vec<u8>, h: &vk_browser::kitty::Header, data: &[u8]) {
    if !app.caps.host_remote
        && std::env::var("VIBEKE_GFX_TRANSFER").as_deref() != Ok("direct")
        && let Some(dir) = private_gfx_dir(app.imgview.gfx_dir.as_deref())
        && let Ok(path) = vk_browser::kitty::write_temp_file(&dir, data)
    {
        vk_browser::kitty::transmit_temp_file(out, h, &path, data.len());
        return;
    }
    vk_browser::kitty::transmit_direct(out, h, data);
}

/// A directory only this user can use (0700, ours, not a symlink); stale files from terminals
/// that never read them are removed.
#[cfg(not(target_arch = "wasm32"))]
fn private_gfx_dir(custom: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let dir = custom
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("vibeke-gfx-{uid}")));
    if std::fs::symlink_metadata(&dir).is_err() {
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    }
    let md = std::fs::symlink_metadata(&dir).ok()?;
    if !md.is_dir() || md.uid() != uid || md.mode() & 0o077 != 0 {
        return None;
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let old = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > crate::time::Duration::from_secs(120));
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Some(dir)
}

#[cfg(target_arch = "wasm32")]
fn private_gfx_dir(_: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    None
}

// ---- iTerm2 --------------------------------------------------------------------------------------

/// Is placement `p` uncovered in the composed frame (its cells are still the blanks [`draw`]
/// put there: no toast, menu, popup or plugin surface over it)?
fn uncovered(app: &App, grid: &Grid, p: &Placement) -> bool {
    let blank = Cell::blank(app.theme.text());
    (p.rect.y..p.rect.y + p.rect.h).all(|y| {
        (p.rect.x..p.rect.x + p.rect.w).all(|x| grid.get(x, y).is_some_and(|c| *c == blank))
    })
}

/// iTerm2: after composing, before the grid diff. Decides which placements are shown, queues
/// the images to send and marks the cells of hidden, moved or removed images for a repaint.
pub fn after_compose(app: &mut App, grid: &Grid) {
    let iterm = gfx(app) == Gfx::Iterm;
    // A full repaint (resize, first frame) clears the screen: everything is sent again.
    if app.prev.cols != grid.cols || app.prev.rows != grid.rows {
        app.imgview.iterm.clear();
    }
    let mut now: HashMap<String, (String, Rect)> = HashMap::new();
    if iterm {
        for p in placements(app) {
            if app.imgview.image(&p.key).is_some() && uncovered(app, grid, &p) {
                now.insert(p.slot, (p.key, p.rect));
            }
        }
    }
    let old = std::mem::take(&mut app.imgview.iterm);
    for (slot, was) in &old {
        if now.get(slot) != Some(was) {
            app.invalidate(was.1);
        }
    }
    for (slot, shown) in &now {
        if old.get(slot) != Some(shown) {
            app.imgview.queued.push(shown.clone());
        }
    }
    // An image moved onto cells another one left keeps its turn: the repaint goes first.
    app.imgview.iterm = now;
}

/// iTerm2: send the queued images (after the frame and the browser panes' frames).
pub fn after_write(app: &mut App, out: &mut Vec<u8>) {
    for (key, r) in std::mem::take(&mut app.imgview.queued) {
        if let Some(png) = app.imgview.image(&key) {
            out.extend_from_slice(inline_image(png, r).as_bytes());
        }
    }
}

/// OSC 1337 inline image over `r`, aspect kept, cursor left where it was.
pub fn inline_image(png: &[u8], r: Rect) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        "\x1b7\x1b[{};{}H\x1b]1337;File=inline=1;size={};width={};height={};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07\x1b8",
        r.y + 1,
        r.x + 1,
        png.len(),
        r.w,
        r.h
    )
}

#[cfg(test)]
#[path = "image_view_tests.rs"]
mod tests;
