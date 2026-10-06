//! Browser panes and previews in the TUI (06 B2, B3.2).
//!
//! - **Views**: every browser pane visible in the current tab is reported to the *media host*
//!   (the local machine's server when there is one, else the pane's own machine) as a
//!   [`MediaPane`] with its content rect, the host cell size and DPR. The media host runs
//!   Chromium and streams changed tiles back on the media channel.
//! - **Drawing**: each tile is a kitty image with a virtual placement; the pane's content
//!   cells hold unicode placeholders (`U+10EEEE` + row/column diacritics, image id in the
//!   foreground colour), so popups and splits clip images exactly like text. Tile pixels go
//!   to the host as shared memory names (`t=s`, same machine) or chunked zlib RGBA (`t=d`).
//!   Hosts without kitty graphics but with iTerm2 inline images get the whole pane as an
//!   OSC 1337 PNG at ≤ 5 fps; hosts with neither open previews in a window.
//! - **Input**: with a browser pane focused, keys, paste, mouse (SGR-pixels when the host has
//!   it, else cell centres) and wheel (pixel deltas) go to the page; the prefix key still goes
//!   to Vibeke, and a small prefix table holds the browser actions.
//! - **Previews**: the sidebar Previews section, tab-bar chips for the focused tab's previews,
//!   and Ctrl/Alt+click on a `http://localhost:<port>` URL printed in a pane.

use crate::app::{App, Mode, Pending, Prompt, PromptKind};
use crate::screen::{Grid, HostCaps, Rect as SRect};
use base64::Engine as _;
use crossterm::event::{
    KeyModifiers, MouseButton as CtButton, MouseEvent as CtMouse, MouseEventKind,
};
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use vk_browser::frame::Rgba;
use vk_browser::kitty::{self, DIACRITICS, Header, PLACEHOLDER, PixelFormat};
use vk_proto::input::{KeyEvent, Mods, MouseButton, MouseKind};
use vk_proto::layout::Rect;
use vk_proto::model::{BrowserPane, Preview, PreviewStatus};
use vk_proto::render::{
    BrowserCmd, BrowserStatus, ClientFrame, Color, MediaFrame, MediaPane, Style, TileData, attr,
};

/// CSS pixels per wheel notch (Stage 0 drove scrolling with 40 px events).
pub const WHEEL_PX: f32 = 40.0;
/// iTerm2 inline-image fallback frame interval.
const INLINE_INTERVAL: Duration = Duration::from_millis(200);
/// Image ids per pane (tiles); bases start at 1 so ids stay below 2^24 (no MSB diacritic).
const IDS_PER_PANE: u32 = 0x4000;

/// Browser prefix table (active while a browser pane is focused). Keys override global
/// bindings that mean nothing in a browser pane (copy mode, scrollback editing, sync input).
pub const DEFAULT_BROWSER_KEYS: &[(&str, &str)] = &[
    ("browser_address", "prefix+e"),
    ("browser_back", "prefix+["),
    ("browser_forward", "prefix+]"),
    ("browser_reload", "prefix+."),
    ("browser_hard_reload", "prefix+,"),
    ("browser_screenshot", "prefix+shift+s"),
    ("browser_window", "prefix+o"),
    ("browser_console", "prefix+alt+c"),
    ("browser_take_over", "prefix+t"),
    ("browser_paste_image", "prefix+shift+v"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gfx {
    Kitty,
    Iterm,
    None,
}

/// Tile grid of a pane's frames (from the last reset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameGeom {
    pub width: u32,
    pub height: u32,
    pub tile_cols: u16,
    pub tile_rows: u16,
    pub grid_cols: u16,
    pub grid_rows: u16,
    pub cell_w: u16,
    pub cell_h: u16,
}

/// Per browser pane, on this client.
#[derive(Debug, Default)]
pub struct PaneMedia {
    /// Machine (index) whose server renders it.
    pub host: usize,
    pub base: u32,
    /// Frame geometry of the last reset.
    pub geom: Option<FrameGeom>,
    pub status: BrowserStatus,
    pub have_frame: bool,
    /// iTerm2 path: the whole frame, updated tile by tile.
    pub canvas: Option<Rgba>,
    pub canvas_dirty: bool,
    pub last_inline: Option<Instant>,
    pub frames: u64,
    pub tiles: u64,
}

#[derive(Default)]
pub struct BrowserUi {
    /// Browser panes visible on the previous frame (a vanished one forces a full repaint).
    pub visible_prev: Vec<String>,
    pub panes: HashMap<String, PaneMedia>,
    next_base: u32,
    /// Kitty commands to write before the next grid diff.
    pub out: Vec<u8>,
    /// (machine, pane, seq) to ack once written.
    acks: Vec<(usize, String, u64)>,
    /// Last `MediaView` sent to each machine.
    last_view: HashMap<usize, Vec<MediaPane>>,
    /// SGR-pixels mouse currently enabled.
    pub pixels: bool,
    last_click: Option<(Instant, u16, u16, u8)>,
    /// Pane → URL last reported to a remote owner.
    relayed: HashMap<String, String>,
    pub bytes_out: u64,
    /// Last "read-only" hint for a watch pane (rate limit).
    ro_hint: Option<Instant>,
    /// Panes each machine was asked to render on this connection (media frames for other
    /// panes are dropped without touching any shm name they carry).
    requested: HashMap<usize, std::collections::HashSet<String>>,
    /// Media frames dropped as malformed (bad geometry or payload sizes).
    pub rejected_frames: u64,
    /// Active preview mirrors on this machine's server (06 B4), as last reported.
    pub mirrors: Vec<MirrorInfo>,
    /// Preview a palette action applies to (set by right-clicking a chip or sidebar row):
    /// (machine, preview id). Consumed by the next preview action.
    pub target: Option<(usize, String)>,
    /// Hands a URL to the OS opener; `None` = [`os_open`]. Tests substitute a recorder.
    pub opener: Option<fn(&str) -> std::io::Result<()>>,
    mirrors_polled: Option<Instant>,
    /// A clipboard-image read requested for this browser pane (`prefix+shift+v`); the main
    /// loop performs it with the event reader stopped (`browser_io`).
    pub clip_read: Option<String>,
}

/// One active mirror (`preview.mirror`): a remote preview's port bound on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorInfo {
    /// Machine label of the remote.
    pub machine: String,
    pub preview: String,
    pub handle: String,
    pub port: u16,
}

impl MirrorInfo {
    fn parse(v: &serde_json::Value) -> Option<MirrorInfo> {
        Some(MirrorInfo {
            machine: v.get("machine")?.as_str()?.to_string(),
            preview: v.get("preview")?.as_str()?.to_string(),
            handle: v.get("preview_handle")?.as_str()?.to_string(),
            port: u16::try_from(v.get("local_port")?.as_u64()?).ok()?,
        })
    }
}

/// Replies to the preview commands of this module.
#[derive(Debug, Clone)]
pub enum Reply {
    /// `preview.open {proxy: true, no_open: true}`: the result carries the one-time URL.
    Proxy,
    Mirror,
    Unmirror,
    /// `preview.status`: refresh the mirror list.
    Mirrors,
    /// `browser.pane.console`: the console split opened or closed.
    ConsoleSplit,
}

/// Open `url` with the OS opener, detached, output discarded. `VIBEKE_NO_OPEN` disables it.
pub fn os_open(url: &str) -> std::io::Result<()> {
    if std::env::var_os("VIBEKE_NO_OPEN").is_some() {
        return Ok(());
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

impl BrowserUi {
    fn alloc_base(&mut self) -> u32 {
        let b = 1 + (self.next_base % 1000) * IDS_PER_PANE;
        self.next_base += 1;
        b
    }
}

/// What the TUI knows about where its host terminal is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEnv {
    pub term_program: String,
    /// `LC_TERMINAL` (iTerm2 sets it and ssh forwards `LC_*`, unlike `TERM_PROGRAM`).
    pub lc_terminal: String,
    /// `XTVERSION` reply (`iTerm2 3.5.4`, `ghostty 1.2.0`, …).
    pub xtversion: Option<String>,
    /// Running inside an SSH session (`SSH_CONNECTION`/`SSH_TTY`/`SSH_CLIENT`): the host
    /// terminal is on another machine.
    pub ssh: bool,
}

impl HostEnv {
    pub fn from_env(xtversion: Option<&str>) -> HostEnv {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        HostEnv {
            term_program: var("TERM_PROGRAM"),
            lc_terminal: var("LC_TERMINAL"),
            xtversion: xtversion.map(str::to_string),
            ssh: ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
                .iter()
                .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty())),
        }
    }

    pub fn is_iterm2(&self) -> bool {
        self.term_program == "iTerm.app"
            || self.lc_terminal == "iTerm2"
            || self
                .xtversion
                .as_deref()
                .is_some_and(|x| x.to_ascii_lowercase().starts_with("iterm2"))
    }
}

/// The graphics decision table: (kitty placeholders, kitty shm, iTerm2 whole-frame images).
/// iTerm2 accepts kitty transmissions but not unicode-placeholder placement, so it always gets
/// the OSC 1337 whole-frame path. A host terminal behind SSH can't map this machine's shared
/// memory (or read its temp files): direct transmission only, whatever the probe answered.
pub fn decide_gfx(g: &vk_browser::probe::GraphicsCaps, env: &HostEnv) -> (bool, bool, bool) {
    let iterm = env.is_iterm2();
    let kitty = g.kitty_graphics && !iterm;
    let shm = kitty && g.kitty_shm == Some(true) && !env.ssh;
    (kitty, shm, iterm)
}

/// Fold the graphics probe into host capabilities.
pub fn host_caps(g: &vk_browser::probe::GraphicsCaps, xtversion: Option<&str>) -> HostCaps {
    let env = HostEnv::from_env(xtversion);
    let (kitty_graphics, kitty_shm, iterm2_images) = decide_gfx(g, &env);
    let (cw, ch) = crate::term::cell_px()
        .or_else(|| g.cell_px.map(|(w, h)| (w as u16, h as u16)))
        .unwrap_or((0, 0));
    HostCaps {
        kitty_graphics,
        kitty_shm,
        iterm2_images,
        host_remote: env.ssh,
        cell_w: cw,
        cell_h: ch,
        dpr_x100: dpr_x100(ch),
        sgr_pixels: g.sgr_pixels.is_some_and(|m| m.supported())
            && std::env::var("VIBEKE_SGR_PIXELS").map_or(true, |v| v != "0"),
        ..Default::default()
    }
}

/// `VIBEKE_DPR`, else a guess from the cell height (Retina cells are ≥ 28 device px tall).
pub fn dpr_x100(cell_h: u16) -> u16 {
    if let Some(v) = std::env::var("VIBEKE_DPR")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0 && *v <= 4.0)
    {
        return (v * 100.0) as u16;
    }
    if cell_h >= 28 { 200 } else { 100 }
}

pub fn gfx(app: &App) -> Gfx {
    if app.caps.kitty_graphics {
        Gfx::Kitty
    } else if app.caps.iterm2_images {
        Gfx::Iterm
    } else {
        Gfx::None
    }
}

/// Cell size and DPR to size viewports with (fallback 10×20 at DPR 1 when unknown: the host
/// scales tiles into their cells anyway).
pub fn cell_geom(app: &App) -> (u16, u16, f32) {
    let (w, h) = if app.caps.cell_w > 0 && app.caps.cell_h > 0 {
        (app.caps.cell_w, app.caps.cell_h)
    } else {
        (10, 20)
    };
    let dpr = if app.caps.dpr_x100 > 0 {
        app.caps.dpr_x100 as f32 / 100.0
    } else {
        1.0
    };
    (w, h, dpr)
}

/// The machine whose server renders browser panes owned by machine `owner`: the local one.
pub fn media_host(app: &App, owner: usize) -> usize {
    app.machines
        .iter()
        .position(|m| m.local && m.connected())
        .unwrap_or(owner)
}

pub fn browser_of<'a>(app: &'a App, mi: usize, pane: &str) -> Option<&'a BrowserPane> {
    app.machines
        .get(mi)?
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)?
        .browser
        .as_ref()
}

/// The focused pane, if it is a browser pane.
pub fn focused_browser(app: &App) -> Option<String> {
    let p = app.focused_pane()?;
    browser_of(app, app.cur, &p).map(|_| p)
}

/// Watch mode of a browser pane (06 B7): (session, held by this pane, held by anyone).
pub fn watch_of(app: &App, pane: &str) -> Option<(String, bool, bool)> {
    let session = browser_of(app, app.cur, pane)?.watch.clone()?;
    let st = app.browser.panes.get(pane).map(|p| &p.status);
    Some((
        session,
        st.is_some_and(|s| s.controlled_here),
        st.is_some_and(|s| s.human_control),
    ))
}

/// Input to a read-only watch pane is dropped here (the server ignores it too) with a hint.
fn read_only(app: &mut App, pane: &str) -> bool {
    match watch_of(app, pane) {
        Some((_, false, human)) => {
            if app
                .browser
                .ro_hint
                .is_none_or(|t| t.elapsed() > Duration::from_secs(4))
            {
                app.browser.ro_hint = Some(Instant::now());
                app.toast(if human {
                    "taken over elsewhere — read-only here; prefix+t takes it over in this pane"
                } else {
                    "watching (read-only) — prefix+t takes over"
                });
            }
            true
        }
        _ => false,
    }
}

// ---- views ------------------------------------------------------------------------------------

/// Report visible browser panes to their media hosts (only when something changed) and turn
/// SGR-pixels mouse on while one is visible.
pub fn update_views(app: &mut App) {
    let cur = app.cur;
    let (cw, ch, dpr) = cell_geom(app);
    let graphics = gfx(app) != Gfx::None;
    let mut per_host: HashMap<usize, Vec<MediaPane>> = HashMap::new();
    let mut visible = Vec::new();
    if graphics && app.machines[cur].connected() {
        for (pid, r) in app.pane_rects() {
            let Some(spec) = browser_of(app, cur, &pid).cloned() else {
                continue;
            };
            // A watch pane is rendered by its own machine (the one running the agent's
            // browser); its frames come over that machine's render stream.
            let host = if spec.watch.is_some() {
                cur
            } else {
                media_host(app, cur)
            };
            visible.push(pid.clone());
            per_host.entry(host).or_default().push(MediaPane {
                pane: pid,
                owner: if host == cur {
                    String::new()
                } else {
                    app.machines[cur].label.clone()
                },
                spec,
                cols: r.w,
                rows: r.h.saturating_sub(1),
                cell_w: cw,
                cell_h: ch,
                dpr,
            });
        }
    }
    let mut hosts: Vec<usize> = per_host
        .keys()
        .chain(app.browser.last_view.keys())
        .copied()
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    for h in hosts {
        let want = per_host.remove(&h).unwrap_or_default();
        let prev = app.browser.last_view.get(&h).cloned().unwrap_or_default();
        // Specs change on every navigation (persisted URL); only geometry/membership matter.
        let key = |v: &[MediaPane]| {
            v.iter()
                .map(|p| {
                    (
                        p.pane.clone(),
                        p.cols,
                        p.rows,
                        p.cell_w,
                        p.cell_h,
                        // A pinned device/viewport change must reach the media host.
                        p.spec.device.clone(),
                        p.spec.viewport.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        if key(&want) == key(&prev) && !(want.is_empty() && !prev.is_empty()) {
            continue;
        }
        let shm = app.caps.kitty_shm && app.machines[h].local && gfx(app) == Gfx::Kitty;
        let sent = app.machines[h].send(ClientFrame::MediaView {
            panes: want.clone(),
            shm,
            key_releases: app.kitty,
        });
        if sent {
            app.browser
                .requested
                .entry(h)
                .or_default()
                .extend(want.iter().map(|p| p.pane.clone()));
        }
        if want.is_empty() {
            app.browser.last_view.remove(&h);
        } else if sent {
            app.browser.last_view.insert(h, want);
        }
    }
    // Forget media for panes no longer shown (the host deletes their images).
    let gone: Vec<String> = app
        .browser
        .panes
        .keys()
        .filter(|p| !visible.contains(p))
        .cloned()
        .collect();
    let mut repaint = false;
    for p in gone {
        if let Some(pm) = app.browser.panes.remove(&p) {
            if pm.geom.is_some() && gfx(app) == Gfx::Kitty {
                delete_range(&mut app.browser.out, pm.base, IDS_PER_PANE);
            }
            // Images live outside the cell grid: blank cells under a vanished image never
            // differ from the previous frame, so the diff renderer would leave the picture on
            // screen (iTerm2 inline images, kitty placements). Repaint everything once.
            repaint = true;
        }
    }
    // Any browser pane that was on screen and isn't any more (closed, tab switched, zoomed
    // away) leaves image pixels behind unless the screen is repainted.
    if app
        .browser
        .visible_prev
        .iter()
        .any(|p| !visible.contains(p))
    {
        repaint = true;
    }
    app.browser.visible_prev = visible.clone();
    if repaint && gfx(app) != Gfx::None {
        app.prev = crate::screen::Grid::new(0, 0);
        app.dirty = true;
    }
    let want_px = app.caps.sgr_pixels && app.caps.cell_w > 0 && !visible.is_empty();
    if want_px != app.browser.pixels {
        app.browser.pixels = want_px;
        if !cfg!(test) {
            crate::term::sgr_pixels(want_px);
        }
    }
}

/// The host window changed size (or font size): re-read the cell size.
pub fn on_resize(app: &mut App) {
    if cfg!(test) {
        return;
    }
    if let Some((w, h)) = crate::term::cell_px() {
        app.caps.cell_w = w;
        app.caps.cell_h = h;
        app.caps.dpr_x100 = dpr_x100(h);
    }
}

/// After a reconnect the machine's server knows nothing of our views.
pub fn on_connected(app: &mut App, mi: usize) {
    app.browser.last_view.remove(&mi);
    app.browser.requested.remove(&mi);
    app.browser.panes.retain(|_, p| p.host != mi);
}

fn delete_range(out: &mut Vec<u8>, base: u32, n: u32) {
    out.extend_from_slice(
        format!("\x1b_Ga=d,d=R,x={base},y={},q=2\x1b\\", base + n - 1).as_bytes(),
    );
}

// ---- media frames -------------------------------------------------------------------------------

fn zlib_ok() -> bool {
    // Vibeke's own engine (libghostty-vt) rejects dynamic-Huffman `o=z` streams (Goal 03
    // Stage 0); nested Vibeke and `VIBEKE_KITTY_ZLIB=0` get raw RGBA.
    std::env::var("VIBEKE_KITTY_ZLIB").map_or(true, |v| v != "0")
        && std::env::var("TERM_PROGRAM").map_or(true, |v| v != "vibeke")
}

/// Upper bounds for a media frame (device px), whatever the pane size.
const MAX_FRAME_PX: u32 = 16384;
const MAX_CELL_PX: u16 = 512;
const MAX_TILE_CELLS: u16 = 64;

/// Check a media frame against the pane it claims to draw into (content `cols`×`rows` cells):
/// bounded geometry, tiles inside the frame and the pane, ids inside the pane's range,
/// payload sizes matching the declared pixels. Shm names are checked by the caller.
pub(crate) fn validate_frame(m: &MediaFrame, cols: u16, rows: u16) -> Result<(), String> {
    let (cw, ch) = (m.cell_w as u64, m.cell_h as u64);
    if !(1..=MAX_CELL_PX).contains(&m.cell_w) || !(1..=MAX_CELL_PX).contains(&m.cell_h) {
        return Err(format!("cell size {}x{}", m.cell_w, m.cell_h));
    }
    if !(1..=MAX_TILE_CELLS).contains(&m.tile_cols) || !(1..=MAX_TILE_CELLS).contains(&m.tile_rows)
    {
        return Err(format!("tile size {}x{}", m.tile_cols, m.tile_rows));
    }
    if m.width == 0 || m.height == 0 || m.width > MAX_FRAME_PX || m.height > MAX_FRAME_PX {
        return Err(format!("frame {}x{}", m.width, m.height));
    }
    // One cell of slack: CSS rounding at fractional DPR can add a device pixel or two.
    if m.width as u64 > (cols as u64 + 1) * cw || m.height as u64 > (rows as u64 + 1) * ch {
        return Err(format!(
            "frame {}x{} larger than the pane ({cols}x{rows} cells)",
            m.width, m.height
        ));
    }
    let grid = m.grid_cols as u64 * m.grid_rows as u64;
    if grid == 0 || grid > IDS_PER_PANE as u64 || m.tiles.len() as u64 > grid {
        return Err(format!("grid {}x{}", m.grid_cols, m.grid_rows));
    }
    for t in &m.tiles {
        let (w, h) = (t.w as u64, t.h as u64);
        let ok = (t.index as u64) < grid
            && t.cols >= 1
            && t.rows >= 1
            && t.cols <= m.tile_cols
            && t.rows <= m.tile_rows
            && t.col as u64 + t.cols as u64 <= cols as u64 + 1
            && t.row as u64 + t.rows as u64 <= rows as u64 + 1
            && w >= 1
            && h >= 1
            && w <= t.cols as u64 * cw
            && h <= t.rows as u64 * ch
            && t.col as u64 * cw + w <= m.width as u64
            && t.row as u64 * ch + h <= m.height as u64;
        if !ok {
            return Err(format!("tile {} out of bounds", t.index));
        }
        let px = w * h * 4;
        let data_ok = match &t.data {
            TileData::Rgba(p) => p.len() as u64 == px,
            TileData::Shm { len, .. } => *len as u64 == px,
            // Incompressible data grows a little under zlib.
            TileData::ZlibRgba(z) => (z.len() as u64) <= px + px / 64 + 1024,
        };
        if !data_ok {
            return Err(format!(
                "tile {} payload does not match {}x{}",
                t.index, t.w, t.h
            ));
        }
    }
    Ok(())
}

pub fn on_media(app: &mut App, mi: usize, m: MediaFrame) {
    // Shm names are honoured only from the local machine's server, for panes this client
    // asked that server to render, and only names carrying Vibeke's prefix and that pane's
    // tag. Anything else is never opened, forwarded or unlinked.
    let local = app.machines.get(mi).is_some_and(|x| x.local);
    let requested = app
        .browser
        .requested
        .get(&mi)
        .is_some_and(|s| s.contains(&m.pane));
    let name_ok = |name: &str| local && requested && kitty::shm::is_tile_name_for(name, &m.pane);
    let names_ok = m.tiles.iter().all(|t| match &t.data {
        TileData::Shm { name, .. } => name_ok(name),
        _ => true,
    });
    let in_view = app
        .browser
        .last_view
        .get(&mi)
        .is_some_and(|v| v.iter().any(|p| p.pane == m.pane));
    let rect = app
        .pane_rects()
        .into_iter()
        .find(|(p, _)| *p == m.pane)
        .map(|(_, r)| r);
    let visible = in_view && rect.is_some() && browser_of(app, app.cur, &m.pane).is_some();
    let mode = gfx(app);
    let invalid = rect.and_then(|r| validate_frame(&m, r.w, r.h.saturating_sub(1)).err());
    if invalid.is_some() {
        app.browser.rejected_frames += 1;
    }
    if !visible || mode == Gfx::None || !names_ok || invalid.is_some() {
        // Not ours to show (any more), or not acceptable: drop it, freeing only the shm
        // objects that are verifiably this pane's.
        for t in &m.tiles {
            if let TileData::Shm { name, .. } = &t.data
                && name_ok(name)
            {
                kitty::shm::unlink(name);
            }
        }
        app.machines[mi].send(ClientFrame::MediaAck {
            pane: m.pane,
            seq: m.seq,
        });
        return;
    }
    let geom = FrameGeom {
        width: m.width,
        height: m.height,
        tile_cols: m.tile_cols,
        tile_rows: m.tile_rows,
        grid_cols: m.grid_cols,
        grid_rows: m.grid_rows,
        cell_w: m.cell_w,
        cell_h: m.cell_h,
    };
    let zlib = zlib_ok();
    let shm_ok = app.caps.kitty_shm;
    let ui = &mut app.browser;
    let base = match ui.panes.get(&m.pane) {
        Some(p) => p.base,
        None => ui.alloc_base(),
    };
    let pm = ui.panes.entry(m.pane.clone()).or_insert_with(|| PaneMedia {
        host: mi,
        base,
        ..Default::default()
    });
    pm.host = mi;
    if m.reset || pm.geom != Some(geom) {
        if pm.geom.is_some() && mode == Gfx::Kitty {
            delete_range(&mut ui.out, pm.base, IDS_PER_PANE);
        }
        pm.geom = Some(geom);
        if mode == Gfx::Iterm {
            pm.canvas = Some(Rgba::new(m.width, m.height));
        }
    }
    pm.have_frame = true;
    pm.frames += 1;
    pm.tiles += m.tiles.len() as u64;
    let before = ui.out.len();
    for t in &m.tiles {
        let id = pm.base + t.index;
        if id >= pm.base + IDS_PER_PANE {
            continue;
        }
        match mode {
            Gfx::Kitty => {
                let mut h = Header::new(id, PixelFormat::Rgba, t.w, t.h);
                h.virtual_cells = Some((t.cols.max(1), t.rows.max(1)));
                h.placement = Some(1);
                h.quiet = 2;
                let px_len = t.w as usize * t.h as usize * 4;
                match &t.data {
                    // The host maps exactly `len` bytes: only forward an object that holds them.
                    TileData::Shm { name, len } if shm_ok => {
                        if kitty::shm::object_size(name).is_ok_and(|s| s >= *len as u64) {
                            kitty::transmit_shm(&mut ui.out, &h, name, *len as usize);
                        } else {
                            kitty::shm::unlink(name);
                        }
                    }
                    TileData::Shm { name, len } => {
                        let px = kitty::shm::read(name, *len as usize).unwrap_or_default();
                        kitty::shm::unlink(name);
                        if px.len() == px_len {
                            kitty::transmit_direct(&mut ui.out, &h, &px);
                        }
                    }
                    TileData::ZlibRgba(z) if zlib => {
                        h.zlib = true;
                        kitty::write_chunked(&mut ui.out, &h.control('d', None), z, h.quiet);
                    }
                    TileData::ZlibRgba(z) => {
                        if let Ok(px) = kitty::unzlib_limited(z, px_len)
                            && px.len() == px_len
                        {
                            kitty::transmit_direct(&mut ui.out, &h, &px);
                        }
                    }
                    TileData::Rgba(px) => kitty::transmit_direct(&mut ui.out, &h, px),
                }
            }
            Gfx::Iterm => {
                let px_len = t.w as usize * t.h as usize * 4;
                let px = match &t.data {
                    TileData::ZlibRgba(z) => kitty::unzlib_limited(z, px_len).unwrap_or_default(),
                    TileData::Rgba(p) => p.clone(),
                    TileData::Shm { name, len } => {
                        let p = kitty::shm::read(name, *len as usize).unwrap_or_default();
                        kitty::shm::unlink(name);
                        p
                    }
                };
                if let Some(c) = pm.canvas.as_mut() {
                    blit(c, t.col, t.row, m.cell_w, m.cell_h, t.w, t.h, &px);
                    pm.canvas_dirty = true;
                }
            }
            Gfx::None => {}
        }
    }
    ui.bytes_out += (ui.out.len() - before) as u64;
    ui.acks.push((mi, m.pane, m.seq));
}

#[allow(clippy::too_many_arguments)]
fn blit(c: &mut Rgba, col: u16, row: u16, cw: u16, ch: u16, w: u32, h: u32, px: &[u8]) {
    let (x0, y0) = (col as u32 * cw as u32, row as u32 * ch as u32);
    if (px.len() as u64) < w as u64 * h as u64 * 4 || x0 >= c.width {
        return;
    }
    for y in 0..h {
        let dy = y0 + y;
        if dy >= c.height {
            break;
        }
        let n = w.min(c.width.saturating_sub(x0)) as usize * 4;
        let src = (y * w * 4) as usize;
        let dst = ((dy * c.width + x0) * 4) as usize;
        c.data[dst..dst + n].copy_from_slice(&px[src..src + n]);
    }
}

pub fn on_state(app: &mut App, mi: usize, pane: String, st: BrowserStatus) {
    if let Some(n) = &st.notice {
        app.toast(n.clone());
    }
    // Remote owners persist navigation through us (the local media host can't reach their
    // layout); local owners are updated by the media host itself.
    if let Some(owner) = app
        .machines
        .iter()
        .position(|m| m.model.panes.iter().any(|p| p.id == pane))
        && owner != mi
        && !st.url.is_empty()
        && st.url != "about:blank"
        && app.browser.relayed.get(&pane) != Some(&st.url)
        && browser_of(app, owner, &pane).is_some_and(|b| b.url != st.url)
    {
        app.browser.relayed.insert(pane.clone(), st.url.clone());
        app.command_on(
            owner,
            "browser.pane.update",
            json!({"pane": pane, "url": st.url, "title": st.title}),
            Pending::Ignore,
        );
    }
    let base = match app.browser.panes.get(&pane) {
        Some(p) => p.base,
        None => app.browser.alloc_base(),
    };
    let pm = app.browser.panes.entry(pane).or_insert_with(|| PaneMedia {
        host: mi,
        base,
        ..Default::default()
    });
    pm.status = st;
}

/// Kitty commands queued since the last draw (written before the grid diff).
pub fn take_output(app: &mut App) -> Vec<u8> {
    std::mem::take(&mut app.browser.out)
}

/// After the frame was written: iTerm2 inline images, then acks.
pub fn after_write(app: &mut App, out: &mut Vec<u8>) {
    if gfx(app) == Gfx::Iterm && matches!(app.mode, Mode::Normal | Mode::Prefix(_)) {
        for (pid, r) in app.pane_rects() {
            let Some(pm) = app.browser.panes.get_mut(&pid) else {
                continue;
            };
            // A host terminal behind SSH gets half the rate (each frame is a whole PNG).
            let interval = if app.caps.host_remote {
                INLINE_INTERVAL * 2
            } else {
                INLINE_INTERVAL
            };
            if !pm.canvas_dirty || pm.last_inline.is_some_and(|t| t.elapsed() < interval) {
                continue;
            }
            let Some(c) = &pm.canvas else { continue };
            let Ok(png) = vk_browser::frame::encode_png(c.width, c.height, &c.data, true) else {
                continue;
            };
            pm.canvas_dirty = false;
            pm.last_inline = Some(Instant::now());
            out.extend_from_slice(
                inline_image(&png, r.x, r.y + 1, r.w, r.h.saturating_sub(1)).as_bytes(),
            );
        }
    }
    for (mi, pane, seq) in std::mem::take(&mut app.browser.acks) {
        app.machines[mi].send(ClientFrame::MediaAck { pane, seq });
    }
}

/// OSC 1337 inline image at cell (x, y) spanning `w × h` cells.
pub fn inline_image(png: &[u8], x: u16, y: u16, w: u16, h: u16) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        "\x1b7\x1b[{};{}H\x1b]1337;File=inline=1;size={};width={w};height={h};preserveAspectRatio=0;doNotMoveCursor=1:{b64}\x07\x1b8",
        y + 1,
        x + 1,
        png.len()
    )
}

// ---- drawing ------------------------------------------------------------------------------------

/// Placeholder grapheme for cell (`row`, `col`) of image `id`.
pub fn placeholder(id: u32, row: u16, col: u16) -> String {
    let mut s = String::with_capacity(12);
    s.push(PLACEHOLDER);
    s.push(DIACRITICS[(row as usize).min(DIACRITICS.len() - 1)]);
    s.push(DIACRITICS[(col as usize).min(DIACRITICS.len() - 1)]);
    let msb = (id >> 24) as usize;
    if msb != 0 {
        s.push(DIACRITICS[msb]);
    }
    s
}

pub fn id_style(id: u32) -> Style {
    Style {
        fg: Color::Rgb(
            ((id >> 16) & 0xff) as u8,
            ((id >> 8) & 0xff) as u8,
            (id & 0xff) as u8,
        ),
        ..Style::default()
    }
}

/// Chrome row hit areas (pane-local x): back, forward, reload, then the URL.
pub fn chrome_hit(x: u16) -> Option<&'static str> {
    match x {
        0..=1 => Some("back"),
        2..=3 => Some("forward"),
        4..=5 => Some("reload"),
        6 => None,
        _ => Some("address"),
    }
}

/// Draw a browser pane: one chrome row, then placeholders (or a message).
pub fn draw_pane(app: &App, g: &mut Grid, pane: &str, r: Rect) {
    let t = &app.theme;
    let focused = app.focused_pane().as_deref() == Some(pane);
    let spec = browser_of(app, app.cur, pane);
    let pm = app.browser.panes.get(pane);
    let st = pm.map(|p| &p.status);
    let url = st
        .map(|s| s.url.as_str())
        .filter(|u| !u.is_empty())
        .or(spec.map(|s| s.url.as_str()))
        .unwrap_or("");
    let chrome_style = if focused { t.sel(t.fg) } else { t.text() };
    g.fill(
        SRect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: 1,
        },
        chrome_style,
    );
    if let Some(w) = spec.and_then(|s| s.watch.clone()) {
        draw_watch_chrome(app, g, pane, r, &w, url, chrome_style);
    } else {
        let pin = spec.and_then(|s| s.device.clone().or_else(|| s.viewport.clone()));
        draw_nav_chrome(g, r, url, st, pin.as_deref(), chrome_style);
    }
    draw_content(app, g, r, url, st, pm);
}

/// Watch-mode chrome (06 B7): `◉ watching agent session b3 · read-only` (or `✋ you control
/// …`), the agent's URL, and the environment label.
fn draw_watch_chrome(
    app: &App,
    g: &mut Grid,
    pane: &str,
    r: Rect,
    session: &str,
    url: &str,
    chrome_style: Style,
) {
    let t = &app.theme;
    let st = app.browser.panes.get(pane).map(|p| &p.status);
    let here = st.is_some_and(|s| s.controlled_here);
    let human = st.is_some_and(|s| s.human_control);
    let (label, style) = if here {
        (
            format!(" ✋ you control agent session {session} · prefix+t releases "),
            Style {
                attrs: attr::BOLD,
                ..t.sel(t.red)
            },
        )
    } else if human {
        (
            format!(" ◉ watching agent session {session} · taken over elsewhere · read-only "),
            Style {
                attrs: attr::BOLD,
                ..t.sel(t.yellow)
            },
        )
    } else {
        (
            format!(" ◉ watching agent session {session} · read-only "),
            Style {
                attrs: chrome_style.attrs | attr::BOLD,
                ..chrome_style
            },
        )
    };
    let used = g.put_str(r.x, r.y, &label, style, r.w);
    let shown = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let env = st.map(|s| s.env.clone()).unwrap_or_default();
    let env_w = UnicodeWidthStr::width(env.as_str()) as u16;
    let rest = r.w.saturating_sub(used + 1);
    let env_fits = env_w + 2 < rest.saturating_sub(12);
    let url_w = if env_fits { rest - env_w - 2 } else { rest };
    g.put_str(
        r.x + used + 1,
        r.y,
        &crate::draw::truncate(shown, url_w as usize),
        chrome_style,
        url_w,
    );
    if env_fits && !env.is_empty() {
        g.put_str(
            r.x + r.w - env_w - 1,
            r.y,
            &env,
            Style {
                attrs: chrome_style.attrs | attr::DIM,
                ..chrome_style
            },
            env_w,
        );
    }
}

/// ` ← → ⟳ <url>` and the environment label.
fn draw_nav_chrome(
    g: &mut Grid,
    r: Rect,
    url: &str,
    st: Option<&BrowserStatus>,
    pin: Option<&str>,
    chrome_style: Style,
) {
    let on = |b: bool| {
        if b {
            chrome_style
        } else {
            Style {
                attrs: chrome_style.attrs | attr::DIM,
                ..chrome_style
            }
        }
    };
    let can_back = st.is_some_and(|s| s.can_back);
    let can_fwd = st.is_some_and(|s| s.can_forward);
    g.put_str(r.x, r.y, " ←", on(can_back), r.w);
    g.put_str(r.x + 2, r.y, " →", on(can_fwd), r.w.saturating_sub(2));
    let loading = st.is_some_and(|s| s.loading);
    g.put_str(
        r.x + 4,
        r.y,
        if loading { " ◌" } else { " ⟳" },
        on(true),
        r.w.saturating_sub(4),
    );
    let mut env = st.map(|s| s.env.clone()).unwrap_or_default();
    let avail = r.w.saturating_sub(8);
    // A pinned device or viewport (letterboxed) is part of what the pane shows; it wins over
    // the environment label when both don't fit.
    if let Some(p) = pin {
        let both = format!("▯ {p} · {env}");
        env = if !env.is_empty()
            && (UnicodeWidthStr::width(both.as_str()) as u16) + 2 < avail.saturating_sub(12)
        {
            both
        } else {
            format!("▯ {p}")
        };
    }
    let env_w = UnicodeWidthStr::width(env.as_str()) as u16;
    let shown = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let env_fits = env_w + 2 < avail.saturating_sub(12);
    let url_w = if env_fits { avail - env_w - 2 } else { avail };
    g.put_str(
        r.x + 7,
        r.y,
        &crate::draw::truncate(shown, url_w as usize),
        Style {
            attrs: chrome_style.attrs | attr::BOLD,
            ..chrome_style
        },
        url_w,
    );
    if env_fits && !env.is_empty() {
        g.put_str(
            r.x + r.w - env_w - 1,
            r.y,
            &env,
            Style {
                attrs: chrome_style.attrs | attr::DIM,
                ..chrome_style
            },
            env_w,
        );
    }
}

/// The content area: placeholders for the tiles, or a message.
fn draw_content(
    app: &App,
    g: &mut Grid,
    r: Rect,
    url: &str,
    st: Option<&BrowserStatus>,
    pm: Option<&PaneMedia>,
) {
    let t = &app.theme;
    let shown = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let content = SRect {
        x: r.x,
        y: r.y + 1,
        w: r.w,
        h: r.h.saturating_sub(1),
    };
    if content.h == 0 {
        return;
    }
    g.fill(content, Style::default());
    let msg = |g: &mut Grid, text: &str, style: Style| {
        g.put_str(
            content.x + 1,
            content.y,
            text,
            style,
            content.w.saturating_sub(1),
        );
    };
    let mode = gfx(app);
    if mode == Gfx::None {
        msg(
            g,
            "this terminal shows no graphics — prefix+o opens the page in a window",
            t.dim(),
        );
        return;
    }
    if let Some(s) = st {
        if s.windowed {
            msg(
                g,
                "open in a window — prefix+o brings it back here",
                t.dim(),
            );
            return;
        }
        if let Some(e) = &s.error {
            msg(g, &format!("✗ {e}"), t.s(t.red));
            if !pm.is_some_and(|p| p.have_frame) {
                return;
            }
        }
    }
    let Some(pm) = pm.filter(|p| p.have_frame) else {
        msg(g, &format!("loading {shown}…"), t.dim());
        return;
    };
    if mode != Gfx::Kitty {
        return; // iTerm2: the inline image covers the content area.
    }
    let Some(FrameGeom {
        width: w,
        height: h,
        tile_cols: tc,
        tile_rows: tr,
        grid_cols: gc,
        grid_rows: gr,
        cell_w: cw,
        cell_h: ch,
    }) = pm.geom
    else {
        return;
    };
    let (tc, tr) = (tc.max(1), tr.max(1));
    let max_cols = w.div_ceil(cw.max(1) as u32) as u16;
    let max_rows = h.div_ceil(ch.max(1) as u32) as u16;
    for cy in 0..content.h.min(max_rows) {
        for cx in 0..content.w.min(max_cols) {
            let (tx, ty) = (cx / tc, cy / tr);
            if tx >= gc || ty >= gr {
                continue;
            }
            let id = pm.base + ty as u32 * gc as u32 + tx as u32;
            g.put_grapheme(
                content.x + cx,
                content.y + cy,
                &placeholder(id, cy % tr, cx % tc),
                id_style(id),
            );
        }
    }
}

// ---- input --------------------------------------------------------------------------------------

fn host_of_focused(app: &App) -> usize {
    media_host(app, app.cur)
}

fn send_cmd(app: &mut App, pane: &str, cmd: BrowserCmd) {
    let host = app
        .browser
        .panes
        .get(pane)
        .map(|p| p.host)
        .unwrap_or_else(|| {
            if watch_of(app, pane).is_some() {
                app.cur
            } else {
                host_of_focused(app)
            }
        });
    let id = app.next_input;
    app.next_input += 1;
    if !app.machines[host].send(ClientFrame::Browser {
        input_id: id,
        pane: pane.to_string(),
        cmd,
    }) {
        app.toast(format!(
            "{} offline — input not sent",
            app.machines[host].label
        ));
    }
}

/// Keys for a focused browser pane go to the page. Returns false when no browser is focused.
pub fn send_key(app: &mut App, ev: KeyEvent) -> bool {
    let Some(pane) = focused_browser(app) else {
        return false;
    };
    if ev.kind != vk_proto::input::KeyKind::Release && read_only(app, &pane) {
        return true;
    }
    if watch_of(app, &pane).is_some_and(|w| !w.1) {
        return true;
    }
    send_cmd(app, &pane, BrowserCmd::Key(ev));
    true
}

pub fn on_paste(app: &mut App, text: &str) -> bool {
    let Some(pane) = focused_browser(app) else {
        return false;
    };
    if read_only(app, &pane) {
        return true;
    }
    // A terminal drop (the paste is only local file paths): ask before the page gets files.
    if watch_of(app, &pane).is_none() && crate::browser_io::maybe_drop(app, &pane, text) {
        return true;
    }
    send_cmd(app, &pane, BrowserCmd::Text(text.to_string()));
    true
}

/// With SGR-pixels on, crossterm's "cells" are pixels: map to cells and keep the pixels.
pub fn cellify(app: &App, me: CtMouse) -> (CtMouse, Option<(u32, u32)>) {
    if !app.browser.pixels || app.caps.cell_w == 0 || app.caps.cell_h == 0 {
        return (me, None);
    }
    let (px, py) = (me.column as u32, me.row as u32);
    let mut m = me;
    m.column = (px / app.caps.cell_w as u32) as u16;
    m.row = (py / app.caps.cell_h as u32) as u16;
    (m, Some((px, py)))
}

fn mods_of(m: KeyModifiers) -> Mods {
    let mut v = Mods::empty();
    if m.contains(KeyModifiers::SHIFT) {
        v = v | Mods::SHIFT;
    }
    if m.contains(KeyModifiers::CONTROL) {
        v = v | Mods::CTRL;
    }
    if m.contains(KeyModifiers::ALT) {
        v = v | Mods::ALT;
    }
    if m.contains(KeyModifiers::SUPER) {
        v = v | Mods::SUPER;
    }
    v
}

/// CSS position of a mouse event inside a browser pane's content area.
pub fn css_pos(app: &App, r: Rect, cell: (u16, u16), px: Option<(u32, u32)>) -> (f32, f32) {
    let (cw, ch, dpr) = cell_geom(app);
    let (ox, oy) = (r.x as f32 * cw as f32, (r.y + 1) as f32 * ch as f32);
    let (x, y) = match px {
        Some((x, y)) => (x as f32 - ox, y as f32 - oy),
        None => (
            (cell.0.saturating_sub(r.x) as f32 + 0.5) * cw as f32,
            (cell.1.saturating_sub(r.y + 1) as f32 + 0.5) * ch as f32,
        ),
    };
    ((x / dpr).max(0.0), (y / dpr).max(0.0))
}

/// Mouse handling for browser panes, the chrome row, preview rows/chips and Ctrl/Alt+click on
/// localhost URLs. Returns true when handled.
pub fn on_mouse(app: &mut App, me: &CtMouse, px: Option<(u32, u32)>) -> bool {
    let (x, y) = (me.column, me.row);
    let down = matches!(me.kind, MouseEventKind::Down(CtButton::Left));
    // Sidebar preview rows.
    let right = matches!(me.kind, MouseEventKind::Down(CtButton::Right));
    if crate::chrome::in_sidebar(app, x) {
        if (down || right)
            && let Some((mi, p)) = preview_hit(app, y)
        {
            if right {
                preview_menu(app, mi, &p);
            } else {
                open_preview(app, mi, &p, p.pane.clone());
            }
            return true;
        }
        return false;
    }
    if crate::chrome::tab_row(app) == Some(y) {
        if (down || right)
            && let Some((mi, p)) = chip_hit(app, x)
        {
            if right {
                preview_menu(app, mi, &p);
            } else {
                open_preview(app, mi, &p, p.pane.clone());
            }
            return true;
        }
        return false;
    }
    let rects = app.pane_rects();
    let Some((pane, r)) = rects.into_iter().find(|(_, r)| r.contains(x, y)) else {
        return false;
    };
    let cur = app.cur;
    if browser_of(app, cur, &pane).is_none() {
        // Ctrl/Alt+click on a localhost URL printed in a pane opens it in a browser pane.
        let modded = me.modifiers.contains(KeyModifiers::CONTROL)
            || me.modifiers.contains(KeyModifiers::ALT);
        // Plugin link handlers first (07 §7.7): a matching token offers them.
        if down && modded && crate::plugins::click(app, cur, &pane, x - r.x, y - r.y) {
            return true;
        }
        if down
            && modded
            && let Some(url) = url_at(app, cur, &pane, x - r.x, y - r.y)
        {
            open_url(app, cur, &pane, &url);
            return true;
        }
        return false;
    }
    if down && app.focused_pane().as_deref() != Some(&pane) {
        app.focus_pane(cur, &pane);
    }
    let watching = watch_of(app, &pane);
    // Chrome row.
    if y == r.y {
        if watching.is_some() {
            return true;
        }
        if down {
            match chrome_hit(x - r.x) {
                Some("back") => send_cmd(app, &pane, BrowserCmd::Back),
                Some("forward") => send_cmd(app, &pane, BrowserCmd::Forward),
                Some("reload") => send_cmd(app, &pane, BrowserCmd::Reload { hard: false }),
                Some("address") => address_bar(app, &pane),
                _ => {}
            }
        }
        return true;
    }
    if let Some((_, here, _)) = watching
        && !here
    {
        if down
            || matches!(
                me.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            read_only(app, &pane);
        }
        return true;
    }
    let (cx, cy) = css_pos(app, r, (x, y), px);
    let mods = mods_of(me.modifiers);
    let cmd = match me.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => BrowserCmd::Wheel {
            x: cx,
            y: cy,
            dx: 0.0,
            dy: if matches!(me.kind, MouseEventKind::ScrollUp) {
                -WHEEL_PX
            } else {
                WHEEL_PX
            },
            mods,
        },
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => BrowserCmd::Wheel {
            x: cx,
            y: cy,
            dx: if matches!(me.kind, MouseEventKind::ScrollLeft) {
                -WHEEL_PX
            } else {
                WHEEL_PX
            },
            dy: 0.0,
            mods,
        },
        MouseEventKind::Down(b) => {
            let now = Instant::now();
            let clicks = match app.browser.last_click {
                Some((t, lx, ly, n))
                    if now.duration_since(t) < Duration::from_millis(400) && lx == x && ly == y =>
                {
                    (n + 1).min(3)
                }
                _ => 1,
            };
            app.browser.last_click = Some((now, x, y, clicks));
            BrowserCmd::Mouse {
                kind: MouseKind::Press,
                button: btn(b),
                x: cx,
                y: cy,
                mods,
                clicks,
            }
        }
        MouseEventKind::Up(b) => BrowserCmd::Mouse {
            kind: MouseKind::Release,
            button: btn(b),
            x: cx,
            y: cy,
            mods,
            clicks: app.browser.last_click.map_or(1, |c| c.3),
        },
        MouseEventKind::Drag(b) => BrowserCmd::Mouse {
            kind: MouseKind::Drag,
            button: btn(b),
            x: cx,
            y: cy,
            mods,
            clicks: 0,
        },
        MouseEventKind::Moved => BrowserCmd::Mouse {
            kind: MouseKind::Move,
            button: MouseButton::None,
            x: cx,
            y: cy,
            mods,
            clicks: 0,
        },
    };
    send_cmd(app, &pane, cmd);
    true
}

fn btn(b: CtButton) -> MouseButton {
    match b {
        CtButton::Left => MouseButton::Left,
        CtButton::Middle => MouseButton::Middle,
        CtButton::Right => MouseButton::Right,
    }
}

/// `http(s)://localhost|127.0.0.1|[::1]|0.0.0.0[:port]/…` under the pane-local cell.
pub fn url_at(app: &App, mi: usize, pane: &str, col: u16, row: u16) -> Option<String> {
    let buf = app.machines[mi].panes.get(pane)?;
    let line = buf.lines.get(row as usize)?;
    // Columns of each grapheme.
    let mut cells: Vec<(u16, char)> = Vec::new();
    let mut c = 0u16;
    for span in &line.spans {
        for ch in span.text.chars() {
            cells.push((c, ch));
            c += UnicodeWidthStr::width(ch.to_string().as_str()).max(1) as u16;
        }
    }
    let text: String = cells.iter().map(|(_, ch)| *ch).collect();
    let chars: Vec<char> = text.chars().collect();
    for scheme in ["http://", "https://"] {
        let mut from = 0;
        while let Some(off) = text[from..].find(scheme) {
            let start_b = from + off;
            let start = text[..start_b].chars().count();
            let mut end = start;
            while end < chars.len()
                && !chars[end].is_whitespace()
                && !matches!(chars[end], '"' | '\'' | '<' | '>' | '`' | ')' | ']')
            {
                end += 1;
            }
            let (c0, c1) = (cells[start].0, cells.get(end).map_or(c, |x| x.0));
            if col >= c0 && col < c1 {
                let url: String = chars[start..end].iter().collect();
                let url = url.trim_end_matches(['.', ',', ';', ':']).to_string();
                let host = host_of(&url)?;
                if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
                    || host.ends_with(".localhost")
                {
                    return Some(url.replace("://0.0.0.0", "://localhost"));
                }
                return None;
            }
            from = start_b + scheme.len();
        }
    }
    None
}

fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let h = if let Some(r) = auth.strip_prefix('[') {
        r.split(']').next()?.to_string()
    } else {
        auth.split(':').next()?.to_string()
    };
    (!h.is_empty()).then_some(h)
}

/// Open a URL printed in pane `pane` of machine `mi`: a browser pane next to it (that
/// machine's `localhost`), or a window when this terminal has no graphics.
pub fn open_url(app: &mut App, mi: usize, pane: &str, url: &str) {
    if gfx(app) == Gfx::None {
        let local = app.machines.iter().position(|m| m.local).unwrap_or(mi);
        let machine = if local == mi {
            String::new()
        } else {
            app.machines[mi].label.clone()
        };
        app.command_on(
            local,
            "preview.open",
            json!({"url": url, "machine": machine, "window": true}),
            Pending::Toast(format!("opened {url} in a window")),
        );
        return;
    }
    app.command_on(
        mi,
        "browser.pane.create",
        json!({"url": url, "pane": pane, "split": "right", "focus": true}),
        Pending::Ignore,
    );
}

/// Open a preview as a browser pane next to `source` (window fallback without graphics).
pub fn open_preview(app: &mut App, mi: usize, p: &Preview, source: Option<String>) {
    if gfx(app) == Gfx::None {
        let local = app.machines.iter().position(|m| m.local).unwrap_or(mi);
        let target = if local == mi {
            p.handle.clone()
        } else {
            format!("{}/{}", app.machines[mi].label, p.handle)
        };
        app.command_on(
            local,
            "preview.open",
            json!({"preview": target, "window": true}),
            Pending::Toast(format!("opened {} in a window", p.handle)),
        );
        return;
    }
    let mut params = json!({"preview": p.id, "split": "right", "focus": true});
    if let Some(s) = source {
        params["pane"] = json!(s);
    }
    app.command_on(mi, "browser.pane.create", params, Pending::Ignore);
}

/// The local machine (the one whose server runs windows, the proxy and mirrors) and the
/// preview's name there: its handle, or `<machine>/<handle>` for a remote's.
fn local_target(app: &App, mi: usize, p: &Preview) -> (usize, String) {
    let local = app.machines.iter().position(|m| m.local).unwrap_or(mi);
    let target = if local == mi {
        p.handle.clone()
    } else {
        format!("{}/{}", app.machines[mi].label, p.handle)
    };
    (local, target)
}

/// The active mirror of preview `p` of machine `mi`, if any.
pub fn mirror_of<'a>(app: &'a App, mi: usize, p: &Preview) -> Option<&'a MirrorInfo> {
    let label = &app.machines.get(mi)?.label;
    app.browser
        .mirrors
        .iter()
        .find(|m| &m.machine == label && m.preview == p.id)
}

/// The mirror warning badge: `⇄ :5173 mirrored`.
fn mirror_badge(p: &Preview) -> String {
    format!("⇄ :{} mirrored", p.port)
}

/// Right-click on a preview row/chip: make it the target of the preview actions and show them.
fn preview_menu(app: &mut App, mi: usize, p: &Preview) {
    app.browser.target = Some((mi, p.id.clone()));
    crate::nav::open_palette(app, "preview_".into());
}

/// The preview a palette action applies to: the right-clicked row/chip (consumed), else the
/// focused pane's best preview.
fn take_target(app: &mut App) -> Option<(usize, Preview)> {
    if let Some((mi, id)) = app.browser.target.take() {
        let found = app
            .machines
            .get(mi)
            .and_then(|m| m.model.previews.iter().find(|p| p.id == id))
            .filter(|p| p.status != PreviewStatus::Gone)
            .cloned();
        if let Some(p) = found {
            return Some((mi, p));
        }
    }
    let pane = app.focused_pane()?;
    let cur = app.cur;
    focused_preview(app, &pane).map(|p| (cur, p))
}

/// Open a preview in a window of the profile browser (always, whatever the terminal can draw).
fn preview_window(app: &mut App, mi: usize, p: &Preview) {
    let (local, target) = local_target(app, mi, p);
    app.command_on(
        local,
        "preview.open",
        json!({"preview": target, "window": true}),
        Pending::Toast(format!("opened {} in a window", p.handle)),
    );
}

/// Open a preview in the user's normal browser through the reverse proxy (06 B4). The server
/// returns the one-time URL to this full-scope client (`no_open`: the server never opens it
/// for us); only this explicit action hands it to the OS opener.
fn preview_proxy(app: &mut App, mi: usize, p: &Preview) {
    if app.caps.host_remote {
        // The proxy listens on the server's loopback; a browser here could not reach it.
        app.toast(format!(
            "over ssh: the proxy is on the server's loopback; run `vibeke preview open {} --proxy` there",
            p.handle
        ));
        return;
    }
    let (local, target) = local_target(app, mi, p);
    app.command_on(
        local,
        "preview.open",
        json!({"preview": target, "proxy": true, "no_open": true}),
        Pending::Preview(Reply::Proxy),
    );
}

fn preview_mirror(app: &mut App, mi: usize, p: &Preview) {
    let (local, target) = local_target(app, mi, p);
    if local == mi {
        app.toast(format!(
            "{} is on this machine already (localhost:{}); mirroring is for remote previews",
            p.handle, p.port
        ));
        return;
    }
    app.command_on(
        local,
        "preview.mirror",
        json!({"preview": target}),
        Pending::Preview(Reply::Mirror),
    );
}

fn preview_unmirror(app: &mut App, mi: usize, p: &Preview) {
    let (local, target) = local_target(app, mi, p);
    let Some(m) = mirror_of(app, mi, p) else {
        app.toast(format!("{} is not mirrored", p.handle));
        return;
    };
    let port = m.port;
    app.command_on(
        local,
        "preview.unmirror",
        json!({"preview": target, "port": port}),
        Pending::Preview(Reply::Unmirror),
    );
}

/// Replies to the preview commands (see [`Reply`]).
pub fn on_reply(
    app: &mut App,
    _mi: usize,
    r: Reply,
    res: Result<serde_json::Value, crate::app::RpcErr>,
) {
    let v = match res {
        Ok(v) => v,
        // A server without the mirror list is not an error worth a toast.
        Err(_) if matches!(r, Reply::Mirrors) => return,
        Err(e) => {
            app.toast(format!("✗ {}", e.message));
            return;
        }
    };
    match r {
        Reply::Proxy => {
            // The tokenized URL goes to the opener and nowhere else: not a toast, not a log.
            let plain = v["url"].as_str().unwrap_or("the preview").to_string();
            match v["open_url"].as_str() {
                Some(url) => {
                    let open = app.browser.opener.unwrap_or(os_open);
                    match open(url) {
                        Ok(()) => {
                            app.toast(format!("opened {plain} in your browser via the proxy"))
                        }
                        Err(_) => app.toast("could not start the OS opener for the proxy URL"),
                    }
                }
                None => app.toast("the server returned no proxy URL for this client"),
            }
        }
        Reply::Mirror => {
            if let Some(m) = MirrorInfo::parse(&v) {
                app.toast(format!(
                    "⇄ mirroring {}/{} on localhost:{} (unauthenticated port; unmirror when done)",
                    m.machine, m.handle, m.port
                ));
                app.browser.mirrors.retain(|x| x.port != m.port);
                app.browser.mirrors.push(m);
            }
        }
        Reply::Unmirror => {
            if let Some(port) = v["local_port"].as_u64() {
                app.browser.mirrors.retain(|x| u64::from(x.port) != port);
                app.toast(format!("stopped mirroring localhost:{port}"));
            }
        }
        Reply::ConsoleSplit => crate::browser_io::on_console_reply(app, &v),
        Reply::Mirrors => {
            let list: Vec<MirrorInfo> = v["mirrors"]
                .as_array()
                .map(|a| a.iter().filter_map(MirrorInfo::parse).collect())
                .unwrap_or_default();
            app.browser.mirrors = list;
        }
    }
    app.dirty = true;
}

/// Keep the mirror badge honest about mirrors made elsewhere (CLI): while remote previews or
/// mirrors exist, ask the local server for its mirror list every 10 s.
/// The mirror poll (only with mirrors or live remote previews, every 10 s) and, on the iTerm2
/// path, the repaint that writes a canvas update the inline-image rate limit held back.
pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    let remote_previews = app.machines.iter().any(|m| {
        !m.local
            && m.model
                .previews
                .iter()
                .any(|p| p.status != PreviewStatus::Gone)
    });
    if (remote_previews || !app.browser.mirrors.is_empty())
        && app.machines.iter().any(|m| m.local && m.connected())
    {
        d.at(
            "browser.mirrors",
            app.browser
                .mirrors_polled
                .map_or(now, |t| t + Duration::from_secs(10)),
        );
    }
    if gfx(app) == Gfx::Iterm && matches!(app.mode, Mode::Normal | Mode::Prefix(_)) {
        let interval = if app.caps.host_remote {
            INLINE_INTERVAL * 2
        } else {
            INLINE_INTERVAL
        };
        for pm in app.browser.panes.values() {
            if let Some(t) = pm.last_inline
                && pm.canvas_dirty
                && pm.canvas.is_some()
                && t + interval > now
            {
                d.redraw("browser.inline", t + interval);
            }
        }
    }
}

pub fn tick(app: &mut App) {
    let remote_previews = app.machines.iter().any(|m| {
        !m.local
            && m.model
                .previews
                .iter()
                .any(|p| p.status != PreviewStatus::Gone)
    });
    if !remote_previews && app.browser.mirrors.is_empty() {
        return;
    }
    if app
        .browser
        .mirrors_polled
        .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
    {
        return;
    }
    let Some(local) = app.machines.iter().position(|m| m.local && m.connected()) else {
        return;
    };
    app.browser.mirrors_polled = Some(Instant::now());
    app.command_on(
        local,
        "preview.status",
        json!({}),
        Pending::Preview(Reply::Mirrors),
    );
}

fn address_bar(app: &mut App, pane: &str) {
    let url = app
        .browser
        .panes
        .get(pane)
        .map(|p| p.status.url.clone())
        .filter(|u| !u.is_empty())
        .or_else(|| browser_of(app, app.cur, pane).map(|b| b.url.clone()))
        .unwrap_or_default();
    app.mode = Mode::Prompt(Prompt {
        kind: PromptKind::BrowserUrl {
            pane: pane.to_string(),
        },
        label: "url".into(),
        input: url,
    });
}

/// The address bar was submitted.
pub fn navigate(app: &mut App, pane: &str, url: &str) {
    if !url.trim().is_empty() {
        send_cmd(app, pane, BrowserCmd::Navigate(url.trim().to_string()));
    }
}

/// The prefix table while a browser pane is focused. Returns true when the key was a browser
/// action.
pub fn prefix_key(app: &mut App, ev: &KeyEvent) -> bool {
    if focused_browser(app).is_none() {
        return false;
    }
    for (action, default) in DEFAULT_BROWSER_KEYS {
        let spec = app
            .config
            .keys
            .bindings
            .get(*action)
            .map(String::as_str)
            .unwrap_or(default);
        let Ok(b) = vk_term::keygrammar::parse_binding(spec) else {
            continue;
        };
        if b.prefix && b.chords.len() == 1 && crate::keymap::key_matches(&b.chords[0], ev) {
            action_name(app, action);
            return true;
        }
    }
    false
}

/// Browser/preview actions (also from the command palette). Returns true when handled.
pub fn action_name(app: &mut App, action: &str) -> bool {
    let fb = focused_browser(app);
    match (action, fb) {
        ("browser_address", Some(p)) => address_bar(app, &p),
        ("browser_back", Some(p)) => send_cmd(app, &p, BrowserCmd::Back),
        ("browser_forward", Some(p)) => send_cmd(app, &p, BrowserCmd::Forward),
        ("browser_reload", Some(p)) => send_cmd(app, &p, BrowserCmd::Reload { hard: false }),
        ("browser_hard_reload", Some(p)) => send_cmd(app, &p, BrowserCmd::Reload { hard: true }),
        ("browser_stop", Some(p)) => send_cmd(app, &p, BrowserCmd::Stop),
        ("browser_screenshot", Some(p)) => send_cmd(app, &p, BrowserCmd::Screenshot),
        ("browser_window", Some(p)) | ("open_preview", Some(p)) => {
            if gfx(app) == Gfx::None {
                // No pane rendering here: open the pane's URL in a window.
                let url = browser_of(app, app.cur, &p)
                    .map(|b| b.url.clone())
                    .unwrap_or_default();
                let cur = app.cur;
                open_url(app, cur, &p, &url);
            } else {
                let windowed = app.browser.panes.get(&p).is_some_and(|m| m.status.windowed);
                send_cmd(app, &p, BrowserCmd::Window(!windowed));
                app.toast(if windowed {
                    "back to the pane"
                } else {
                    "opening in a window (same profile)"
                });
            }
        }
        ("browser_take_over", Some(p)) => match watch_of(app, &p) {
            Some((session, here, _)) => {
                send_cmd(app, &p, BrowserCmd::TakeOver(!here));
                app.toast(if here {
                    format!("releasing agent session {session}")
                } else {
                    format!("taking over agent session {session}")
                });
            }
            None => app.toast("not watching an agent session (browser watch <session>)"),
        },
        ("browser_console", Some(p)) => {
            if watch_of(app, &p).is_some() {
                app.toast("a watch pane shows an agent session: vibeke browser console <session>");
            } else {
                crate::browser_io::console_split(app, &p);
            }
        }
        ("browser_paste_image", Some(p)) => {
            if read_only(app, &p) {
                return true;
            }
            crate::browser_io::request_image_paste(app, &p);
        }
        (a, None) if a.starts_with("browser_") => app.toast("no browser pane focused"),
        ("open_preview", None) => match take_target(app) {
            Some((mi, p)) => open_preview(app, mi, &p, p.pane.clone()),
            None => app.toast("no preview for this pane"),
        },
        ("preview_window" | "preview_proxy" | "preview_mirror" | "preview_unmirror", _) => {
            let Some((mi, p)) = take_target(app) else {
                app.toast("no preview for this pane (right-click a preview chip or row)");
                return true;
            };
            match action {
                "preview_window" => preview_window(app, mi, &p),
                "preview_proxy" => preview_proxy(app, mi, &p),
                "preview_mirror" => preview_mirror(app, mi, &p),
                _ => preview_unmirror(app, mi, &p),
            }
        }
        // `prefix+o` (open_notification_target): a pending toast target first, else the
        // focused pane's preview.
        ("open_notification_target", None) => {
            if app.toasts.iter().any(|t| t.pane.is_some()) {
                return false;
            }
            return open_focused_preview(app);
        }
        _ => return false,
    }
    true
}

/// `prefix+o` on a pane with a preview opens it as a browser pane next to it.
pub fn open_focused_preview(app: &mut App) -> bool {
    let Some(pane) = app.focused_pane() else {
        return false;
    };
    let cur = app.cur;
    match focused_preview(app, &pane) {
        Some(p) => {
            open_preview(app, cur, &p, Some(pane));
            true
        }
        None => false,
    }
}

/// The best live preview of `pane` on the current machine (up, then declared, then suggested).
fn focused_preview(app: &App, pane: &str) -> Option<Preview> {
    app.machines[app.cur]
        .model
        .previews
        .iter()
        .filter(|p| p.pane.as_deref() == Some(pane) && p.status != PreviewStatus::Gone)
        .min_by_key(|p| match p.status {
            PreviewStatus::Up => 0,
            PreviewStatus::Declared => 1,
            PreviewStatus::Suggested => 2,
            _ => 3,
        })
        .cloned()
}

// ---- previews in chrome -------------------------------------------------------------------------

/// Previews for the sidebar section: every machine, not gone, ports ascending.
pub fn preview_entries(app: &App) -> Vec<(usize, Preview)> {
    let mut v: Vec<(usize, Preview)> = app
        .machines
        .iter()
        .enumerate()
        .flat_map(|(mi, m)| {
            m.model
                .previews
                .iter()
                .filter(|p| p.status != PreviewStatus::Gone)
                .map(move |p| (mi, p.clone()))
        })
        .collect();
    v.sort_by_key(|(mi, p)| (*mi, p.status == PreviewStatus::Suggested, p.port));
    v.truncate(12);
    v
}

/// Sidebar row segments for one preview: `● :5173 vite devbox` (suggestions dimmed, "open?").
pub fn preview_segs(app: &App, mi: usize, p: &Preview) -> Vec<(String, Style)> {
    let t = &app.theme;
    let (dot, color) = match p.status {
        PreviewStatus::Up => ("●", t.green),
        PreviewStatus::Declared => ("◌", t.yellow),
        PreviewStatus::Down => ("○", t.red),
        _ => ("◌", t.muted),
    };
    let suggested = p.status == PreviewStatus::Suggested;
    let text = if suggested { t.dim() } else { t.text() };
    let mirrored = mirror_of(app, mi, p).is_some();
    let mut segs = vec![(format!("  {dot} "), t.s(color))];
    if mirrored {
        // Visible warning while the remote port is exposed on this machine's loopback.
        segs.push((mirror_badge(p), t.bold(t.yellow)));
    } else {
        segs.push((
            format!(":{}", p.port),
            if suggested { t.dim() } else { t.bold(t.fg) },
        ));
        if let Some(l) = &p.label {
            segs.push((format!(" {l}"), text));
        }
    }
    if app.machines.len() > 1 {
        segs.push((format!(" {}", app.machines[mi].label), t.dim()));
    }
    if suggested {
        segs.push((" open?".into(), t.dim()));
    }
    segs
}

/// The preview whose sidebar row is at host row `y` (the section is the last one).
pub fn preview_hit(app: &App, y: u16) -> Option<(usize, Preview)> {
    let rows = crate::draw::sidebar_rows(app).len();
    let entries = preview_entries(app);
    let i = (y as usize).checked_sub(1)?;
    let first = rows.checked_sub(entries.len())?;
    if i < first || i >= rows {
        return None;
    }
    entries.into_iter().nth(i - first)
}

/// Chips for previews of panes in the focused tab: (machine, preview, label, x0, x1), placed
/// right after the tab entries.
pub fn chip_entries(app: &App, tabs_end: u16) -> Vec<(usize, Preview, String, u16, u16)> {
    let cur = app.cur;
    let Some(tab) = app.focused_tab() else {
        return vec![];
    };
    let panes = tab.layout.panes();
    let mut x = tabs_end + 1;
    let mut out = Vec::new();
    let mut ps: Vec<&Preview> = app.machines[cur]
        .model
        .previews
        .iter()
        .filter(|p| {
            p.status != PreviewStatus::Gone && p.pane.as_ref().is_some_and(|q| panes.contains(q))
        })
        .collect();
    ps.sort_by_key(|p| p.port);
    for p in ps.into_iter().take(4) {
        let name = p.label.clone().unwrap_or_else(|| "web".into());
        let label = if mirror_of(app, cur, p).is_some() {
            format!(" {} ", mirror_badge(p))
        } else if p.status == PreviewStatus::Suggested {
            format!(" ◉ {name} :{} open? ", p.port)
        } else {
            format!(" ◉ {name} :{} ", p.port)
        };
        let w = UnicodeWidthStr::width(label.as_str()) as u16;
        out.push((cur, p.clone(), label, x, x + w));
        x += w;
    }
    out
}

fn chip_hit(app: &App, x: u16) -> Option<(usize, Preview)> {
    chip_entries(app, crate::draw::tabs_end(app))
        .into_iter()
        .find(|(_, _, _, a, b)| x >= *a && x < *b)
        .map(|(mi, p, _, _, _)| (mi, p))
}

/// Draw the chips into the tab bar.
pub fn draw_chips(app: &App, g: &mut Grid, tabs_end: u16, limit: u16) {
    let t = &app.theme;
    for (mi, p, label, x0, x1) in chip_entries(app, tabs_end) {
        if x1 > limit {
            break;
        }
        let st = match p.status {
            _ if mirror_of(app, mi, &p).is_some() => t.bold(t.yellow),
            PreviewStatus::Up => t.bold(t.green),
            PreviewStatus::Suggested => t.dim(),
            PreviewStatus::Down => t.s(t.red),
            _ => t.s(t.yellow),
        };
        let Some(row) = crate::chrome::tab_row(app) else {
            return;
        };
        g.put_str(x0, row, &label, st, x1 - x0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Popup;
    use crate::app::test_app;
    use serde_json::Value;
    use vk_proto::model::*;
    use vk_proto::render::{MediaTile, PaneModes, Row, Span};

    fn browser_pane(id: &str, tab: &str, url: &str) -> Pane {
        serde_json::from_value(json!({
            "id": id, "handle": "w1:p2", "tab": tab, "workspace": "W", "title": null,
            "auto_title": "◉ localhost:5173", "cwd": null, "cols": 80, "rows": 24,
            "child_pid": null, "fg_cmdline": [], "exited": false, "exit_code": null,
            "unread": false, "marked_unread": false, "pinned": false, "created_by": "user",
            "recovered": null,
            "browser": {"url": url, "machine": "", "task": null, "preview": null,
                        "source_pane": "p1", "history": [url], "history_index": 0, "title": ""}
        }))
        .unwrap()
    }

    fn shell_pane(id: &str, tab: &str) -> Pane {
        let mut p = browser_pane(id, tab, "x");
        p.browser = None;
        p
    }

    /// Machine 0 (local) shows a tab with a shell pane p1 and a browser pane bp.
    fn setup(
        n: usize,
    ) -> (
        App,
        Vec<tokio::sync::mpsc::UnboundedReceiver<ClientFrame>>,
        usize,
    ) {
        let (mut app, rxs) = test_app(n);
        let mi = n - 1; // the machine owning the tab (remote when n > 1)
        app.cur = mi;
        let m = &mut app.machines[mi];
        m.model.workspaces = vec![Workspace {
            id: "W".into(),
            handle: "w1".into(),
            name: None,
            auto_name: "w".into(),
            root_path: "/".into(),
            task: None,
            order: 1.0,
            branch: None,
        }];
        m.model.tabs = vec![Tab {
            id: "T".into(),
            handle: "w1:t1".into(),
            workspace: "W".into(),
            title: None,
            number: 1,
            layout: LayoutNode::Split {
                dir: SplitDir::Horizontal,
                children: vec![
                    (LayoutNode::Leaf { pane: "p1".into() }, 0.5),
                    (LayoutNode::Leaf { pane: "bp".into() }, 0.5),
                ],
            },
            focused_pane: Some("bp".into()),
            zoomed_pane: None,
            order: 1.0,
            floating: Default::default(),
            floats_hidden: false,
        }];
        m.model.panes = vec![
            shell_pane("p1", "T"),
            browser_pane("bp", "T", "http://localhost:5173/"),
        ];
        m.focus = ClientFocus {
            workspace: Some("W".into()),
            tab: Some("T".into()),
            pane: Some("bp".into()),
        };
        app.caps.kitty_graphics = true;
        app.caps.truecolor = true;
        app.caps.cell_w = 16;
        app.caps.cell_h = 32;
        app.caps.dpr_x100 = 200;
        app.sidebar = false;
        app.size = (81, 25);
        (app, rxs, mi)
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>) -> Vec<ClientFrame> {
        let mut v = Vec::new();
        while let Ok(f) = rx.try_recv() {
            v.push(f);
        }
        v
    }

    #[test]
    fn a_vanished_browser_pane_forces_a_full_repaint() {
        // Images live outside the grid: without a full repaint the blank cells under the old
        // image never change and the picture stays on screen (user-reported in iTerm2).
        for iterm in [false, true] {
            let (mut app, _rxs, mi) = setup(1);
            if iterm {
                app.caps.kitty_graphics = false;
                app.caps.iterm2_images = true;
            }
            update_views(&mut app);
            app.prev = crate::screen::Grid::new(10, 10);
            app.machines[mi].model.tabs[0].zoomed_pane = Some("p1".into());
            update_views(&mut app);
            assert_eq!(app.prev.cols, 0, "full repaint requested (iterm={iterm})");
            // Nothing changes on the next pass: no repeated full repaints.
            app.prev = crate::screen::Grid::new(10, 10);
            update_views(&mut app);
            assert_eq!(app.prev.cols, 10, "only once (iterm={iterm})");
        }
    }

    #[test]
    fn views_go_to_the_local_media_host_with_geometry() {
        let (mut app, mut rxs, mi) = setup(2);
        assert_eq!(mi, 1);
        update_views(&mut app);
        // Pane rect: x 41..81 (40 cols) × 24 rows → content 40×23.
        let sent = drain(&mut rxs[0]);
        let mv = sent
            .iter()
            .find_map(|f| match f {
                ClientFrame::MediaView { panes, shm, .. } => Some((panes.clone(), *shm)),
                _ => None,
            })
            .expect("MediaView to the local machine");
        assert_eq!(mv.0.len(), 1);
        let p = &mv.0[0];
        assert_eq!(p.pane, "bp");
        assert_eq!(p.owner, "m1", "remote owner named");
        assert_eq!((p.cols, p.rows, p.cell_w, p.cell_h), (40, 23, 16, 32));
        assert!((p.dpr - 2.0).abs() < 1e-6);
        assert_eq!(p.spec.url, "http://localhost:5173/");
        assert!(!mv.1, "no shm without a kitty shm probe");
        assert!(
            drain(&mut rxs[1])
                .iter()
                .all(|f| !matches!(f, ClientFrame::MediaView { .. })),
            "the owner gets no view"
        );
        // Unchanged: nothing resent. Hidden: an empty view.
        update_views(&mut app);
        assert!(drain(&mut rxs[0]).is_empty());
        app.machines[1].model.tabs[0].zoomed_pane = Some("p1".into());
        update_views(&mut app);
        let sent = drain(&mut rxs[0]);
        assert!(
            sent.iter()
                .any(|f| matches!(f, ClientFrame::MediaView { panes, .. } if panes.is_empty())),
            "{sent:?}"
        );
    }

    fn frame(seq: u64, reset: bool, tiles: Vec<MediaTile>) -> MediaFrame {
        MediaFrame {
            pane: "bp".into(),
            seq,
            width: 640,
            height: 736,
            cell_w: 16,
            cell_h: 32,
            tile_cols: 4,
            tile_rows: 2,
            grid_cols: 10,
            grid_rows: 12,
            reset,
            tiles,
        }
    }

    fn tile(index: u32, col: u16, row: u16, data: TileData) -> MediaTile {
        MediaTile {
            index,
            col,
            row,
            cols: 4,
            rows: 2,
            w: 64,
            h: 64,
            data,
        }
    }

    /// Placeholders cover the content area and the kitty bytes are accepted by Vibeke's own VT
    /// engine (libghostty-vt): each tile acknowledged, placeholder cells carry the image id.
    #[test]
    fn placeholders_and_kitty_output_through_the_vt_engine() {
        // SAFETY: test-only env, read by zlib_ok() in this thread.
        unsafe { std::env::set_var("VIBEKE_KITTY_ZLIB", "0") };
        let (mut app, mut rxs, mi) = setup(1);
        app.size = (121, 25);
        update_views(&mut app);
        drain(&mut rxs[0]);
        let r = app.pane_rects()[1].1;
        assert_eq!((r.x, r.y), (61, 1));
        let px = vec![200u8; 64 * 64 * 4];
        on_state(
            &mut app,
            mi,
            "bp".into(),
            BrowserStatus {
                url: "http://localhost:5173/".into(),
                env: "laptop chromium → local".into(),
                can_back: true,
                ..Default::default()
            },
        );
        on_media(
            &mut app,
            mi,
            frame(
                1,
                true,
                vec![
                    tile(0, 0, 0, TileData::ZlibRgba(kitty::zlib(&px, 6))),
                    tile(11, 4, 2, TileData::Rgba(px.clone())),
                ],
            ),
        );
        let base = app.browser.panes["bp"].base;
        let mut g = Grid::new(121, 25);
        crate::draw::compose(&app, &mut g);
        // Chrome row on the pane's first row: buttons, URL, environment label.
        let row: String = g.row(1).iter().map(|c| c.text.as_str()).collect();
        assert!(row.contains("←") && row.contains("⟳"), "{row}");
        assert!(row.contains("localhost:5173/"), "{row}");
        assert!(row.contains("laptop chromium → local"), "{row}");
        // Content cell (0,0) is tile 0 row 0 col 0; cell (5,3) is tile 11 (row 1, col 1) at
        // (1,1) inside it.
        let c00 = g.get(r.x, 2).unwrap();
        assert_eq!(c00.text.as_str(), placeholder(base, 0, 0));
        assert_eq!(c00.style.fg, id_style(base).fg);
        let c53 = g.get(r.x + 5, 2 + 3).unwrap();
        assert_eq!(c53.text.as_str(), placeholder(base + 11, 1, 1));
        // Output into the engine: transmissions first, then the grid.
        let mut out = take_output(&mut app);
        let mut caps = app.caps;
        caps.sync_update = false;
        crate::screen::diff(&Grid::new(121, 25), &g, &caps, &mut out);
        let mut quiet0 = String::from_utf8(out.clone()).unwrap();
        quiet0 = quiet0.replace(",q=2", ",q=0");
        let mut eng = vk_term::Engine::new(121, 25, 100);
        let mut fx = Vec::new();
        eng.feed(quiet0.as_bytes(), &mut fx);
        let replies: Vec<String> = fx
            .iter()
            .filter_map(|e| match e {
                vk_term::Effect::Reply(b) => Some(String::from_utf8_lossy(b).into_owned()),
                _ => None,
            })
            .collect();
        let oks = replies.iter().filter(|r| r.contains(";OK")).count();
        assert_eq!(oks, 2, "{replies:?}");
        let text = eng.screen_text();
        assert!(
            text.contains(PLACEHOLDER),
            "placeholders on the engine screen"
        );
        // Acks go out after the write.
        let mut post = Vec::new();
        after_write(&mut app, &mut post);
        assert!(
            drain(&mut rxs[0])
                .iter()
                .any(|f| matches!(f, ClientFrame::MediaAck { seq: 1, .. }))
        );
        // A popup over the pane clips the image: its cells are no longer placeholders.
        app.mode = Mode::Popup(crate::app::Popup::Help);
        let mut g2 = Grid::new(121, 25);
        crate::draw::compose(&app, &mut g2);
        let covered = (0..121u16)
            .flat_map(|x| (2..25u16).map(move |y| (x, y)))
            .filter(|(x, y)| {
                g.get(*x, *y)
                    .unwrap()
                    .text
                    .as_str()
                    .starts_with(PLACEHOLDER)
                    && !g2
                        .get(*x, *y)
                        .unwrap()
                        .text
                        .as_str()
                        .starts_with(PLACEHOLDER)
            })
            .count();
        assert!(covered > 0);
    }

    #[test]
    fn placeholder_width_is_one_cell() {
        let p = placeholder(0x00ab_cdef, 1, 3);
        assert_eq!(UnicodeWidthStr::width(p.as_str()), 1);
        use unicode_segmentation::UnicodeSegmentation;
        assert_eq!(p.graphemes(true).count(), 1);
        assert_eq!(
            kitty::decode_placeholder(&p),
            Some((Some(1), Some(3), None))
        );
        assert_eq!(id_style(0x00ab_cdef).fg, Color::Rgb(0xab, 0xcd, 0xef));
    }

    #[test]
    fn keys_paste_mouse_and_prefix_table() {
        let (mut app, mut rxs, _) = setup(2);
        update_views(&mut app);
        drain(&mut rxs[0]);
        // Normal-mode key → Browser{Key} to the media host, not a PTY key to the owner.
        app.on_key(KeyEvent::ch('x'));
        let f = drain(&mut rxs[0]);
        assert!(
            f.iter().any(|f| matches!(f, ClientFrame::Browser { pane, cmd: BrowserCmd::Key(k), .. } if pane == "bp" && k.key == vk_proto::input::Key::Char('x'))),
            "{f:?}"
        );
        assert!(
            drain(&mut rxs[1])
                .iter()
                .all(|f| !matches!(f, ClientFrame::Key { .. }))
        );
        // Direct bindings (ctrl+v image paste) don't steal keys from the page.
        app.on_key(KeyEvent::new(vk_proto::input::Key::Char('v'), Mods::CTRL));
        assert!(drain(&mut rxs[0]).iter().any(|f| matches!(
            f,
            ClientFrame::Browser {
                cmd: BrowserCmd::Key(_),
                ..
            }
        )));
        // The prefix still goes to Vibeke; prefix+[ is "back" in a browser pane.
        app.on_key(app.keymap.prefix.clone());
        assert!(matches!(app.mode, Mode::Prefix(_)));
        app.on_key(KeyEvent::ch('['));
        assert!(drain(&mut rxs[0]).iter().any(|f| matches!(
            f,
            ClientFrame::Browser {
                cmd: BrowserCmd::Back,
                ..
            }
        )));
        assert!(matches!(app.mode, Mode::Normal));
        // prefix+e opens the address bar prefilled; enter navigates.
        app.on_key(app.keymap.prefix.clone());
        app.on_key(KeyEvent::ch('e'));
        assert!(matches!(&app.mode, Mode::Prompt(p) if p.input == "http://localhost:5173/"));
        for _ in 0.."http://localhost:5173/".len() {
            app.on_key(KeyEvent::named(vk_proto::input::NamedKey::Backspace));
        }
        for c in "localhost:3000/x".chars() {
            app.on_key(KeyEvent::ch(c));
        }
        app.on_key(KeyEvent::named(vk_proto::input::NamedKey::Enter));
        assert!(drain(&mut rxs[0]).iter().any(|f| matches!(f, ClientFrame::Browser { cmd: BrowserCmd::Navigate(u), .. } if u == "localhost:3000/x")));
        // Paste → insertText.
        assert!(on_paste(&mut app, "hello"));
        assert!(drain(&mut rxs[0]).iter().any(
            |f| matches!(f, ClientFrame::Browser { cmd: BrowserCmd::Text(t), .. } if t == "hello")
        ));
        // Mouse: cell-centre mapping into CSS px (content origin is the row under the chrome).
        let me = CtMouse {
            kind: MouseEventKind::Down(CtButton::Left),
            column: 41 + 3,
            row: 1 + 1 + 2,
            modifiers: KeyModifiers::NONE,
        };
        assert!(on_mouse(&mut app, &me, None));
        let f = drain(&mut rxs[0]);
        let (x, y) = f
            .iter()
            .find_map(|f| match f {
                ClientFrame::Browser {
                    cmd:
                        BrowserCmd::Mouse {
                            x,
                            y,
                            kind: MouseKind::Press,
                            clicks: 1,
                            ..
                        },
                    ..
                } => Some((*x, *y)),
                _ => None,
            })
            .expect("press");
        assert_eq!((x, y), (3.5 * 16.0 / 2.0, 2.5 * 32.0 / 2.0));
        // SGR-pixels: exact pixels.
        let r = Rect {
            x: 41,
            y: 1,
            w: 40,
            h: 24,
        };
        let (px, py) = css_pos(&app, r, (0, 0), Some((41 * 16 + 10, 2 * 32 + 7)));
        assert_eq!((px, py), (5.0, 3.5));
        // Wheel → pixel deltas.
        let wheel = CtMouse {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        on_mouse(&mut app, &wheel, None);
        assert!(drain(&mut rxs[0]).iter().any(|f| matches!(f, ClientFrame::Browser { cmd: BrowserCmd::Wheel { dy, .. }, .. } if *dy == WHEEL_PX)));
        // Chrome row: back button.
        let back = CtMouse {
            kind: MouseEventKind::Down(CtButton::Left),
            column: 41,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        on_mouse(&mut app, &back, None);
        assert!(drain(&mut rxs[0]).iter().any(|f| matches!(
            f,
            ClientFrame::Browser {
                cmd: BrowserCmd::Back,
                ..
            }
        )));
    }

    #[test]
    fn localhost_urls_and_previews_open_panes() {
        let (mut app, mut rxs, mi) = setup(2);
        app.machines[mi].panes.insert(
            "p1".into(),
            crate::app::PaneBuf {
                epoch: 1,
                rev: 1,
                cols: 40,
                rows: 24,
                lines: vec![Row {
                    spans: vec![Span {
                        style: Style::default(),
                        text: "  ➜  Local:   http://localhost:5173/app/".into(),
                        cols: 40,
                    }],
                    wrapped: false,
                }],
                cursor: Default::default(),
                modes: PaneModes::default(),
                title: String::new(),
            },
        );
        assert_eq!(
            url_at(&app, mi, "p1", 20, 0).as_deref(),
            Some("http://localhost:5173/app/")
        );
        assert_eq!(url_at(&app, mi, "p1", 3, 0), None);
        let click = CtMouse {
            kind: MouseEventKind::Down(CtButton::Left),
            column: 20,
            row: 1,
            modifiers: KeyModifiers::CONTROL,
        };
        assert!(on_mouse(&mut app, &click, None));
        let cmds: Vec<String> = drain(&mut rxs[mi])
            .into_iter()
            .filter_map(|f| match f {
                ClientFrame::Command { json, .. } => Some(json),
                _ => None,
            })
            .collect();
        assert!(
            cmds.iter().any(|j| j.contains("browser.pane.create")
                && j.contains("http://localhost:5173/app/")
                && j.contains("\"pane\":\"p1\"")),
            "{cmds:?}"
        );
        // prefix+o on a pane with a preview → browser pane on the preview's machine.
        app.machines[mi].model.previews = vec![
            serde_json::from_value(json!({
                "id": "PV", "handle": "v4", "machine": "m1", "pane": "p1", "task": null,
                "port": 5173, "path": "/", "label": "vite", "url": "http://localhost:5173/",
                "scheme": "http", "status": "up", "source": "banner", "pid": null,
                "first_seen_ms": 0, "last_seen_ms": 0
            }))
            .unwrap(),
        ];
        let cur = app.cur;
        app.focus_pane(cur, "p1");
        drain(&mut rxs[mi]);
        assert!(action_name(&mut app, "open_notification_target"));
        let cmds: Vec<String> = drain(&mut rxs[mi])
            .into_iter()
            .filter_map(|f| match f {
                ClientFrame::Command { json, .. } => Some(json),
                _ => None,
            })
            .collect();
        assert!(
            cmds.iter()
                .any(|j| j.contains("browser.pane.create") && j.contains("\"preview\":\"PV\"")),
            "{cmds:?}"
        );
        // Chips in the tab bar and the sidebar Previews section.
        let chips = chip_entries(&app, 10);
        assert_eq!(chips.len(), 1);
        assert!(chips[0].2.contains("◉ vite :5173"));
        app.sidebar = true;
        let rows = crate::draw::sidebar_rows(&app);
        let last: String = rows
            .last()
            .unwrap()
            .segs
            .iter()
            .map(|(s, _)| s.as_str())
            .collect();
        assert!(last.contains(":5173") && last.contains("vite"), "{last}");
        assert!(preview_hit(&app, rows.len() as u16).is_some());
        // No graphics: the same action opens a window through the local server.
        app.caps.kitty_graphics = false;
        drain(&mut rxs[0]);
        open_focused_preview(&mut app);
        let cmds: Vec<String> = drain(&mut rxs[0])
            .into_iter()
            .filter_map(|f| match f {
                ClientFrame::Command { json, .. } => Some(json),
                _ => None,
            })
            .collect();
        assert!(
            cmds.iter().any(|j| j.contains("preview.open")
                && j.contains("m1/v4")
                && j.contains("\"window\":true")),
            "{cmds:?}"
        );
    }

    // ---- preview actions: window, proxy, mirror (06 B4) -----------------------------------

    static OPENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    fn record_open(url: &str) -> std::io::Result<()> {
        OPENED.lock().unwrap().push(url.to_string());
        Ok(())
    }

    fn cmds_of(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>) -> Vec<Value> {
        drain(rx)
            .into_iter()
            .filter_map(|f| match f {
                ClientFrame::Command { json, .. } => serde_json::from_str::<Value>(&json).ok(),
                _ => None,
            })
            .collect()
    }

    /// A remote machine's preview `v4` (port 5173) on pane `p1`, focused.
    fn remote_preview_app() -> (
        App,
        Vec<tokio::sync::mpsc::UnboundedReceiver<ClientFrame>>,
        usize,
    ) {
        let (mut app, mut rxs, mi) = setup(2);
        app.machines[mi].model.previews = vec![
            serde_json::from_value(json!({
                "id": "PV", "handle": "v4", "machine": "m1", "pane": "p1", "task": null,
                "port": 5173, "path": "/", "label": "vite", "url": "http://localhost:5173/",
                "scheme": "http", "status": "up", "source": "banner", "pid": null,
                "first_seen_ms": 0, "last_seen_ms": 0
            }))
            .unwrap(),
        ];
        let cur = app.cur;
        app.focus_pane(cur, "p1");
        app.browser.opener = Some(record_open);
        for rx in &mut rxs {
            drain(rx);
        }
        (app, rxs, mi)
    }

    fn mirror_reply(app: &App, mi: usize) -> Value {
        json!({
            "machine": app.machines[mi].label, "preview": "PV", "preview_handle": "v4",
            "local_port": 5173, "warning": "unauthenticated"
        })
    }

    #[test]
    fn preview_window_and_proxy_actions_go_through_the_local_server() {
        let (mut app, mut rxs, mi) = remote_preview_app();
        let label = app.machines[mi].label.clone();
        assert!(action_name(&mut app, "preview_window"));
        let c = cmds_of(&mut rxs[0]);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0]["method"], "preview.open");
        assert_eq!(c[0]["params"]["preview"], format!("{label}/v4"));
        assert_eq!(c[0]["params"]["window"], true);
        assert!(cmds_of(&mut rxs[mi]).is_empty());

        // Proxy: the server is told not to open it; this client opens the one-time URL.
        assert!(action_name(&mut app, "preview_proxy"));
        let c = cmds_of(&mut rxs[0]);
        assert_eq!(c[0]["method"], "preview.open");
        assert_eq!(c[0]["params"]["proxy"], true);
        assert_eq!(c[0]["params"]["no_open"], true);
        assert!(
            app.machines[0]
                .pending
                .values()
                .any(|p| matches!(p, Pending::Preview(Reply::Proxy)))
        );
        OPENED.lock().unwrap().clear();
        on_reply(
            &mut app,
            0,
            Reply::Proxy,
            Ok(json!({
                "url": "http://m1-v4.vibeke.localhost:7000/",
                "open_url": "http://m1-v4.vibeke.localhost:7000/?vk_token=SECRET",
            })),
        );
        assert_eq!(
            *OPENED.lock().unwrap(),
            vec!["http://m1-v4.vibeke.localhost:7000/?vk_token=SECRET".to_string()]
        );
        // The token is never shown: the toast names the plain URL only.
        let shown: String = app.toasts.iter().map(|t| t.text.clone()).collect();
        assert!(
            shown.contains("vibeke.localhost:7000") && !shown.contains("SECRET"),
            "{shown}"
        );
        // A server that returns no URL (pane scope) opens nothing.
        OPENED.lock().unwrap().clear();
        on_reply(&mut app, 0, Reply::Proxy, Ok(json!({"url": "http://x/"})));
        assert!(OPENED.lock().unwrap().is_empty());
        // Over ssh nothing is sent or opened: the proxy is on the server's loopback.
        app.caps.host_remote = true;
        assert!(action_name(&mut app, "preview_proxy"));
        assert!(cmds_of(&mut rxs[0]).is_empty());
        assert!(OPENED.lock().unwrap().is_empty());
    }

    #[test]
    fn mirror_and_unmirror_track_state_and_show_the_badge() {
        let (mut app, mut rxs, mi) = remote_preview_app();
        let label = app.machines[mi].label.clone();
        app.sidebar = true;
        let text = |app: &App| -> String {
            let rows = crate::draw::sidebar_rows(app);
            rows.last()
                .unwrap()
                .segs
                .iter()
                .map(|(s, _)| s.as_str())
                .collect()
        };
        assert!(!text(&app).contains("mirrored"));
        assert!(!chip_entries(&app, 10)[0].2.contains("mirrored"));

        assert!(action_name(&mut app, "preview_mirror"));
        let c = cmds_of(&mut rxs[0]);
        assert_eq!(c[0]["method"], "preview.mirror");
        assert_eq!(c[0]["params"]["preview"], format!("{label}/v4"));
        // Unmirroring something that is not mirrored sends nothing.
        action_name(&mut app, "preview_unmirror");
        assert!(cmds_of(&mut rxs[0]).is_empty());

        let reply = mirror_reply(&app, mi);
        on_reply(&mut app, 0, Reply::Mirror, Ok(reply));
        assert_eq!(app.browser.mirrors.len(), 1);
        let row = text(&app);
        assert!(row.contains("⇄ :5173 mirrored"), "{row}");
        assert_eq!(chip_entries(&app, 10)[0].2, " ⇄ :5173 mirrored ");
        // The warning is styled, not just text.
        let rows = crate::draw::sidebar_rows(&app);
        let badge = rows
            .last()
            .unwrap()
            .segs
            .iter()
            .find(|(s, _)| s.contains("mirrored"))
            .unwrap();
        assert_eq!(badge.1, app.theme.bold(app.theme.yellow));

        // Draw: the chip is in the tab bar while the mirror is active.
        let mut g = crate::screen::Grid::new(80, 24);
        draw_chips(&app, &mut g, 10, 80);
        let drawn = crate::tasks::grid_text(&g);
        assert!(drawn.contains("⇄ :5173 mirrored"), "{drawn}");

        // Unmirror sends the port and drops the badge on success.
        assert!(action_name(&mut app, "preview_unmirror"));
        let c = cmds_of(&mut rxs[0]);
        assert_eq!(c[0]["method"], "preview.unmirror");
        assert_eq!(c[0]["params"]["port"], 5173);
        on_reply(
            &mut app,
            0,
            Reply::Unmirror,
            Ok(json!({"local_port": 5173, "machine": label, "preview": "v4"})),
        );
        assert!(app.browser.mirrors.is_empty());
        assert!(!text(&app).contains("mirrored"));
        assert!(!chip_entries(&app, 10)[0].2.contains("mirrored"));

        // A refused mirror (port busy) is reported and leaves no badge.
        on_reply(
            &mut app,
            0,
            Reply::Mirror,
            Err(crate::app::RpcErr {
                kind: "conflict".into(),
                message: "port 5173 is busy on this machine".into(),
                details: Value::Null,
            }),
        );
        assert!(app.browser.mirrors.is_empty());
        let shown: String = app.toasts.iter().map(|t| t.text.clone()).collect();
        assert!(shown.contains("busy"), "{shown}");
    }

    #[test]
    fn mirror_is_refused_for_a_local_preview_and_polled_state_replaces_the_list() {
        let (mut app, mut rxs, mi) = remote_preview_app();
        // Move the preview to the local machine.
        let pv = app.machines[mi].model.previews.clone();
        app.machines[mi].model.previews.clear();
        app.machines[0].model.previews = pv;
        app.cur = 0;
        let label = app.machines[0].label.clone();
        let panes: Vec<_> = app.machines[mi].model.panes.clone();
        app.machines[0].model.panes = panes;
        app.machines[0].model.tabs = app.machines[mi].model.tabs.clone();
        app.machines[0].model.workspaces = app.machines[mi].model.workspaces.clone();
        app.focus_pane(0, "p1");
        action_name(&mut app, "preview_mirror");
        assert!(cmds_of(&mut rxs[0]).is_empty());
        let shown: String = app.toasts.iter().map(|t| t.text.clone()).collect();
        assert!(shown.contains("remote previews"), "{shown}");

        // `preview.status` lists mirrors made elsewhere (CLI): the list is replaced.
        on_reply(
            &mut app,
            0,
            Reply::Mirrors,
            Ok(json!({"mirrors": [
                {"machine": label, "preview": "PV", "preview_handle": "v4", "local_port": 5173},
                {"machine": "other", "preview": "X", "preview_handle": "v9", "local_port": 3000},
            ]})),
        );
        assert_eq!(app.browser.mirrors.len(), 2);
        on_reply(&mut app, 0, Reply::Mirrors, Ok(json!({"mirrors": []})));
        assert!(app.browser.mirrors.is_empty());
        // An old server without the call: silently ignored.
        on_reply(
            &mut app,
            0,
            Reply::Mirrors,
            Err(crate::app::RpcErr {
                kind: "method_not_found".into(),
                message: "no".into(),
                details: Value::Null,
            }),
        );
        assert!(app.toasts.iter().all(|t| t.text != "✗ no"));
    }

    #[test]
    fn right_click_on_a_chip_or_row_targets_the_palette_actions() {
        let (mut app, mut rxs, mi) = remote_preview_app();
        app.sidebar = true;
        // A second preview on another pane: the right-clicked one wins over the focus.
        let mut other = app.machines[mi].model.previews[0].clone();
        other.id = "PV2".into();
        other.handle = "v5".into();
        other.port = 6006;
        other.pane = Some("bp".into());
        app.machines[mi].model.previews.push(other);
        let rows = crate::draw::sidebar_rows(&app).len() as u16;
        // Sidebar rows are ordered by port: 5173 then 6006 (last row).
        let click = |col: u16, row: u16| CtMouse {
            kind: MouseEventKind::Down(CtButton::Right),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let sx = crate::chrome::sidebar_x(&app).unwrap_or(0);
        assert!(on_mouse(&mut app, &click(sx + 3, rows), None));
        assert_eq!(app.browser.target, Some((mi, "PV2".to_string())));
        match &app.mode {
            Mode::Popup(Popup::Palette { filter, .. }) => assert_eq!(filter, "preview_"),
            _ => panic!("palette not open"),
        }
        for rx in &mut rxs {
            drain(rx);
        }
        // The palette lists the preview actions for that filter.
        let ids: Vec<String> = crate::nav::palette_ranked(&app, "preview_")
            .into_iter()
            .map(|(e, _)| e.id)
            .collect();
        for want in [
            "preview_window",
            "preview_proxy",
            "preview_mirror",
            "preview_unmirror",
        ] {
            assert!(ids.iter().any(|i| i == want), "{want} in {ids:?}");
        }
        app.action("preview_window", None);
        let c = cmds_of(&mut rxs[0]);
        assert_eq!(
            c[0]["params"]["preview"],
            format!("{}/v5", app.machines[mi].label)
        );
        // The target is consumed: the next action falls back to the focused pane's preview.
        assert!(app.browser.target.is_none());
        app.action("preview_window", None);
        let c = cmds_of(&mut rxs[0]);
        assert_eq!(
            c[0]["params"]["preview"],
            format!("{}/v4", app.machines[mi].label)
        );
    }
}

#[cfg(test)]
#[path = "browser_media_tests.rs"]
mod media_tests;
#[cfg(test)]
#[path = "browser_watch_tests.rs"]
mod watch_tests;
