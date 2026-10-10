//! Inbound kitty graphics on terminal panes (03 §9): images a pane's program placed, as the
//! server forwards them (`ServerFrame::Image` once per hash, `ServerFrame::PaneImages` per
//! pane), re-placed on the host.
//!
//! With kitty graphics on the host, each image's pixels are transmitted once (`a=t`, zlib when
//! the host takes it) under an id from [`ID_BASE`] up, and sized with one virtual placement
//! (`a=p,U=1,p=1,c=,r=`, re-sent without pixels when the size changes); the pane cells it
//! covers are drawn as unicode placeholders (`U+10EEEE` + row/column diacritics, the id in the
//! foreground colour), like browser panes; so images clip at pane edges, scroll with the text
//! and stay under popups. The placeholder cells carry no placement id, so an image has one
//! size on the host at a time: the first visible placement's; placements of it at another size
//! that frame get the label. Without kitty graphics the first visible row of the image shows a
//! `[image W×H]` label instead. Pixels are kept (zlib) up to [`CACHE_MAX`]; images no pane
//! places are evicted first, and one evicted that way is fetched again (a `Resync` of the
//! pane) when a pane places it later. Transmissions are bounded per frame by
//! [`FRAME_OUT_MAX`] (at least one image goes); the rest wait for the next frame.
//!
//! **Placements the program makes itself** (`ImagePlace.virt`, a virtual placement whose
//! unicode placeholder cells the program writes, with its own image id in their foreground
//! colour): the image is transmitted and sized like the others, and the program's placeholder
//! cells are rewritten to the host id (row/column diacritics kept, or inferred from the cell
//! to the left as kitty does). Without kitty graphics those cells are blanked and labelled.
//!
//! **Z-order**: a placement below text (`z < 0`) leaves cells that hold text alone, so the text
//! stays readable over the image.

use crate::app::App;
use crate::screen::Grid;
use std::collections::{HashMap, HashSet};
use vk_browser::kitty::{self, Header, PixelFormat};
use vk_proto::layout::Rect;
use vk_proto::render::{ClientFrame, ImagePlace};

/// Host image ids for pane images: above every browser tile, gallery and thumbnail id (the
/// fourth placeholder diacritic carries the top byte).
pub const ID_BASE: u32 = 1 << 24;
/// Compressed pixels kept on the client.
pub const CACHE_MAX: usize = 256 << 20;
/// Kitty output queued per frame before further transmissions wait for the next frame (one
/// transmission always goes, so a frame carries at most this plus one image).
pub const FRAME_OUT_MAX: usize = 16 << 20;

struct Img {
    width: u32,
    height: u32,
    rgba_z: Vec<u8>,
}

/// One image on the host: its id and the cell size of its virtual placement.
struct HostImg {
    id: u32,
    cells: (u16, u16),
}

#[derive(Default)]
pub struct State {
    images: HashMap<(usize, String), Img>,
    places: HashMap<(usize, String), Vec<ImagePlace>>,
    /// Images transmitted to the host, per (machine, hash).
    host: HashMap<(usize, String), HostImg>,
    /// Images evicted while no pane placed them: fetched again when one does.
    evicted: HashSet<(usize, String)>,
    next_id: u32,
    bytes: usize,
    /// `o=z` transmissions (None: not decided yet; see [`zlib_ok`]).
    zlib: Option<bool>,
    /// Kitty commands for the host, written before the frame's grid diff.
    pub out: Vec<u8>,
}

/// The host takes zlib (`o=z`) payloads. Vibeke's own engine (libghostty-vt) rejects
/// dynamic-Huffman streams, so nested Vibeke and `VIBEKE_KITTY_ZLIB=0` get raw RGBA (as browser
/// tiles do).
fn zlib_ok() -> bool {
    std::env::var("VIBEKE_KITTY_ZLIB").map_or(true, |v| v != "0")
        // Nested inside a Vibeke pane (the terminal identity is inherited, so check `VIBEKE`).
        && std::env::var("VIBEKE").map_or(true, |v| v != "1")
}

/// `ServerFrame::Image`.
pub fn on_image(app: &mut App, mi: usize, hash: String, width: u32, height: u32, rgba_z: Vec<u8>) {
    let st = &mut app.images;
    let key = (mi, hash);
    st.evicted.remove(&key);
    if let Some(old) = st.images.remove(&key) {
        st.bytes -= old.rgba_z.len();
    }
    st.bytes += rgba_z.len();
    st.images.insert(
        key,
        Img {
            width,
            height,
            rgba_z,
        },
    );
    evict(st);
}

/// Drop unplaced images (then any) until the cache fits.
fn evict(st: &mut State) {
    while st.bytes > CACHE_MAX {
        let placed = |k: &(usize, String)| {
            st.places
                .iter()
                .any(|((m, _), v)| *m == k.0 && v.iter().any(|p| p.hash == k.1))
        };
        let unplaced = st.images.keys().find(|k| !placed(k)).cloned();
        let was_unplaced = unplaced.is_some();
        let Some(k) = unplaced.or_else(|| st.images.keys().next().cloned()) else {
            break;
        };
        if let Some(img) = st.images.remove(&k) {
            st.bytes -= img.rgba_z.len();
        }
        // Only an image evicted unplaced is fetched again: placed images over the budget
        // would evict each other forever.
        if was_unplaced {
            st.evicted.insert(k.clone());
        }
        if let Some(h) = st.host.remove(&k) {
            kitty::delete_image(&mut st.out, h.id);
        }
    }
}

/// `ServerFrame::PaneImages`.
pub fn on_places(app: &mut App, mi: usize, pane: String, places: Vec<ImagePlace>) {
    // Pixels evicted earlier: a resync of the pane makes the server send them again.
    let refetch = app.caps.kitty_graphics
        && places.iter().any(|p| {
            let k = (mi, p.hash.clone());
            !app.images.images.contains_key(&k) && app.images.evicted.contains(&k)
        });
    if refetch {
        for p in &places {
            app.images.evicted.remove(&(mi, p.hash.clone()));
        }
        if let Some(m) = app.machines.get(mi) {
            m.send(ClientFrame::Resync { pane: pane.clone() });
        }
    }
    if places.is_empty() {
        app.images.places.remove(&(mi, pane));
    } else {
        app.images.places.insert((mi, pane), places);
    }
}

/// A keyframe for the pane: its placements come again if it still has any.
pub fn on_pane_full(app: &mut App, mi: usize, pane: &str) {
    app.images.places.remove(&(mi, pane.to_string()));
}

/// The largest image accepted (`graphics.max_image_bytes`, 03 §9).
fn max_image(app: &App) -> usize {
    (app.config.graphics.max_image_bytes.0 as usize)
        .clamp(64 << 10, vk_term::limits::MAX_IMAGE_BYTES_CAP)
}

/// Before composing: transmit (once per image, within the frame budget) the images visible
/// panes place, and size their virtual placements.
pub fn before_draw(app: &mut App) {
    if !app.caps.kitty_graphics || app.images.places.is_empty() {
        return;
    }
    let max_img = max_image(app);
    let mi = app.cur;
    let visible: Vec<String> = app.pane_rects().into_iter().map(|(p, _)| p).collect();
    let st = &mut app.images;
    let zlib = *st.zlib.get_or_insert_with(zlib_ok);
    // The size each image is shown at this frame: its first visible placement's.
    let mut want: Vec<(String, (u16, u16))> = Vec::new();
    for pane in &visible {
        if let Some(ps) = st.places.get(&(mi, pane.clone())) {
            for p in ps {
                if !want.iter().any(|(h, _)| *h == p.hash) {
                    want.push((p.hash.clone(), (p.cols.max(1), p.rows.max(1))));
                }
            }
        }
    }
    let start = st.out.len();
    let mut sent = 0usize;
    let mut deferred = false;
    for (hash, cells) in want {
        let key = (mi, hash);
        if let Some(h) = st.host.get_mut(&key) {
            if h.cells != cells {
                h.cells = cells;
                place(&mut st.out, h.id, cells);
            }
            continue;
        }
        let Some(img) = st.images.get(&key) else {
            continue;
        };
        if sent > 0 && st.out.len() - start >= FRAME_OUT_MAX {
            deferred = true;
            break;
        }
        let len = img.width as usize * img.height as usize * 4;
        if len == 0 || len > max_img {
            continue;
        }
        let Ok(px) = kitty::unzlib_limited(&img.rgba_z, len) else {
            continue;
        };
        if px.len() != len {
            continue;
        }
        let id = ID_BASE + st.next_id % 0x00ff_0000;
        st.next_id += 1;
        let mut h = Header::new(id, PixelFormat::Rgba, img.width, img.height);
        h.action = b't';
        h.quiet = 2;
        if zlib {
            // The cached stream is the payload: no deflate round trip, and the host gets the
            // compressed size.
            h.zlib = true;
            kitty::write_chunked(&mut st.out, &h.control('d', None), &img.rgba_z, h.quiet);
        } else {
            kitty::transmit_direct(&mut st.out, &h, &px);
        }
        place(&mut st.out, id, cells);
        st.host.insert(key, HostImg { id, cells });
        sent += 1;
    }
    if deferred {
        // The rest go next frame.
        app.dirty = true;
    }
}

/// (Re)size an image's virtual placement: `p=1` replaces the previous one.
fn place(out: &mut Vec<u8>, id: u32, (c, r): (u16, u16)) {
    out.extend_from_slice(format!("\x1b_Ga=p,U=1,i={id},p=1,c={c},r={r},q=2\x1b\\").as_bytes());
}

/// Kitty commands queued for the host.
pub fn take_output(app: &mut App) -> Vec<u8> {
    std::mem::take(&mut app.images.out)
}

/// Draw the placements of `pane` (machine `mi`) into its rect `r`, clipped.
pub fn draw(app: &App, g: &mut Grid, mi: usize, pane: &str, r: Rect) {
    let Some(places) = app.images.places.get(&(mi, pane.to_string())) else {
        return;
    };
    let t = app.theme;
    for p in places.iter().filter(|p| p.virt.is_none()) {
        let (c0, r0) = (p.col, p.row);
        // Only the part inside the rect is iterated (a placement may claim 65535² cells).
        let ys = (-r0).max(0)..(r.h as i32 - r0).min(p.rows as i32);
        let xs = (-c0).max(0)..(r.w as i32 - c0).min(p.cols as i32);
        if ys.is_empty() || xs.is_empty() {
            continue;
        }
        let id = app
            .images
            .host
            .get(&(mi, p.hash.clone()))
            .filter(|h| h.cells == (p.cols.max(1), p.rows.max(1)))
            .map(|h| h.id);
        match id {
            Some(id) if app.caps.kitty_graphics => {
                for cy in ys {
                    for cx in xs.clone() {
                        let (x, y) = (r.x + (c0 + cx) as u16, r.y + (r0 + cy) as u16);
                        // Below text: a cell that holds text keeps it.
                        if p.z < 0
                            && g.get(x, y).is_some_and(|c| {
                                let s = c.text.as_str();
                                !s.trim().is_empty() && !s.starts_with(kitty::PLACEHOLDER)
                            })
                        {
                            continue;
                        }
                        g.put_grapheme(
                            r.x + (c0 + cx) as u16,
                            r.y + (r0 + cy) as u16,
                            &crate::browser::placeholder(id, cy as u16, cx as u16),
                            crate::browser::id_style(id),
                        );
                    }
                }
            }
            _ => {
                // No kitty graphics on the host (or pixels not here yet, or the image is shown
                // at another size): a label on the first visible row.
                let label = format!("[image {}×{}]", p.width, p.height);
                let x = (c0 + xs.start) as u16;
                let w = (xs.end - xs.start) as u16;
                g.put_str(r.x + x, r.y + (r0 + ys.start) as u16, &label, t.dim(), w);
            }
        }
    }
    draw_program_placeholders(app, g, mi, places, r);
}

/// Index of a placeholder diacritic (kitty's row/column/id-byte encoding).
fn diacritic(c: char) -> Option<u32> {
    static MAP: std::sync::OnceLock<HashMap<char, u32>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        kitty::DIACRITICS
            .iter()
            .enumerate()
            .map(|(i, d)| (*d, i as u32))
            .collect()
    })
    .get(&c)
    .copied()
}

/// A program's placeholder cell: (image id, row, column), row/column `None` when omitted.
pub fn parse_placeholder(
    text: &str,
    fg: vk_proto::render::Color,
) -> Option<(u32, Option<u32>, Option<u32>)> {
    use vk_proto::render::Color;
    let mut it = text.chars();
    if it.next()? != kitty::PLACEHOLDER {
        return None;
    }
    let low = match fg {
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        Color::Indexed(n) => u32::from(n),
        Color::Default => return None,
    };
    let row = it.next().and_then(diacritic);
    let col = it.next().and_then(diacritic);
    let msb = it.next().and_then(diacritic).unwrap_or(0);
    Some((low | (msb << 24), row, col))
}

/// Rewrite the placeholder cells the program wrote for its virtual placements (03 §9) to the
/// host's image ids; blank them (with a label) when the host cannot show the image.
fn draw_program_placeholders(app: &App, g: &mut Grid, mi: usize, places: &[ImagePlace], r: Rect) {
    let virts: Vec<&ImagePlace> = places.iter().filter(|p| p.virt.is_some()).collect();
    if virts.is_empty() {
        return;
    }
    let find = |id: u32| {
        virts
            .iter()
            .find(|p| p.virt == Some(id))
            .or_else(|| {
                virts
                    .iter()
                    .find(|p| p.virt.map(|v| v & 0x00ff_ffff) == Some(id))
            })
            .copied()
    };
    let mut labels: Vec<(u16, u16, String)> = Vec::new();
    for y in r.y..r.y.saturating_add(r.h).min(g.rows) {
        let mut prev: Option<(u32, u32, u32)> = None;
        for x in r.x..r.x.saturating_add(r.w).min(g.cols) {
            let Some(cell) = g.get(x, y) else { continue };
            let Some((id, row, col)) = parse_placeholder(cell.text.as_str(), cell.style.fg) else {
                prev = None;
                continue;
            };
            // Omitted diacritics continue the cell to the left (kitty's inference rules).
            let (row, col) = match (row, col, prev) {
                (Some(r), Some(c), _) => (r, c),
                (Some(r), None, Some((pid, pr, pc))) if pid == id && pr == r => (r, pc + 1),
                (None, None, Some((pid, pr, pc))) if pid == id => (pr, pc + 1),
                (r, c, _) => (r.unwrap_or(0), c.unwrap_or(0)),
            };
            prev = Some((id, row, col));
            let Some(p) = find(id) else { continue };
            let host = app
                .images
                .host
                .get(&(mi, p.hash.clone()))
                .filter(|h| h.cells == (p.cols.max(1), p.rows.max(1)))
                .map(|h| h.id);
            match host {
                Some(hid) if app.caps.kitty_graphics => g.put_grapheme(
                    x,
                    y,
                    &crate::browser::placeholder(hid, row as u16, col as u16),
                    crate::browser::id_style(hid),
                ),
                _ => {
                    g.put_grapheme(x, y, " ", vk_proto::render::Style::default());
                    if !labels.iter().any(|(_, _, h)| *h == p.hash) {
                        labels.push((x, y, p.hash.clone()));
                    }
                }
            }
        }
    }
    let t = app.theme;
    for (x, y, hash) in labels {
        if let Some(p) = virts.iter().find(|p| p.hash == hash) {
            let label = format!("[image {}×{}]", p.width, p.height);
            let w = (r.x + r.w).saturating_sub(x);
            g.put_str(x, y, &label, t.dim(), w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{PaneBuf, test_app};

    fn place(hash: &str, col: i32, row: i32, cols: u16, rows: u16) -> ImagePlace {
        ImagePlace {
            hash: hash.into(),
            width: 4,
            height: 2,
            col,
            row,
            cols,
            rows,
            z: 0,
            virt: None,
        }
    }

    /// The program's own placeholder cells (image id 42 in the foreground, row/column
    /// diacritics, the second cell inferring its column) become the host image's placeholders;
    /// without kitty graphics they are blanked and labelled.
    #[test]
    fn program_placeholders_are_rewritten_to_host_ids() {
        use vk_proto::render::{Color, Style};
        let mut app = setup();
        app.caps.kitty_graphics = true;
        app.images.zlib = Some(false);
        on_image(&mut app, 0, "v".into(), 4, 2, kitty::zlib(&[5u8; 32], 1));
        let mut vp = place("v", 0, 0, 2, 1);
        vp.virt = Some(42);
        on_places(&mut app, 0, "p1".into(), vec![vp.clone()]);
        before_draw(&mut app);
        let out = String::from_utf8_lossy(&take_output(&mut app)).into_owned();
        assert!(out.contains("a=p,U=1,i=16777216,p=1,c=2,r=1"), "{out}");
        let r = rect(&app);
        let prog = Style {
            fg: Color::Rgb(0, 0, 42),
            ..Style::default()
        };
        let mut g = Grid::new(app.size.0, app.size.1);
        let first = format!(
            "{}{}{}",
            kitty::PLACEHOLDER,
            kitty::DIACRITICS[0],
            kitty::DIACRITICS[0]
        );
        g.put_grapheme(r.x + 3, r.y + 1, &first, prog);
        g.put_grapheme(r.x + 4, r.y + 1, &kitty::PLACEHOLDER.to_string(), prog);
        draw(&app, &mut g, 0, "p1", r);
        assert_eq!(
            g.get(r.x + 3, r.y + 1).unwrap().text.as_str(),
            crate::browser::placeholder(ID_BASE, 0, 0)
        );
        assert_eq!(
            g.get(r.x + 4, r.y + 1).unwrap().text.as_str(),
            crate::browser::placeholder(ID_BASE, 0, 1),
            "column inferred from the left cell"
        );
        assert_eq!(
            g.get(r.x + 4, r.y + 1).unwrap().style.fg,
            crate::browser::id_style(ID_BASE).fg
        );
        assert_eq!(
            parse_placeholder(&first, Color::Indexed(7)),
            Some((7, Some(0), Some(0)))
        );
        // No kitty graphics: blank + label.
        app.caps.kitty_graphics = false;
        let mut g = Grid::new(app.size.0, app.size.1);
        g.put_grapheme(r.x + 3, r.y + 1, &first, prog);
        draw(&app, &mut g, 0, "p1", r);
        assert!(crate::tasks::grid_text(&g).contains("[image 4×2]"));
    }

    /// A placement below text (`z < 0`) leaves the cells that hold text alone.
    #[test]
    fn placements_below_text_keep_text_cells() {
        let mut app = setup();
        app.caps.kitty_graphics = true;
        app.images.zlib = Some(false);
        on_image(&mut app, 0, "bg".into(), 4, 2, kitty::zlib(&[1u8; 32], 1));
        let mut p = place("bg", 0, 0, 3, 1);
        p.z = -1;
        on_places(&mut app, 0, "p1".into(), vec![p]);
        before_draw(&mut app);
        let r = rect(&app);
        let mut g = Grid::new(app.size.0, app.size.1);
        g.put_str(r.x + 1, r.y, "A", vk_proto::render::Style::default(), 1);
        draw(&app, &mut g, 0, "p1", r);
        assert_eq!(g.get(r.x + 1, r.y).unwrap().text.as_str(), "A");
        assert!(
            g.get(r.x, r.y)
                .unwrap()
                .text
                .as_str()
                .starts_with(kitty::PLACEHOLDER)
        );
        assert!(
            g.get(r.x + 2, r.y)
                .unwrap()
                .text
                .as_str()
                .starts_with(kitty::PLACEHOLDER)
        );
    }

    fn setup() -> App {
        setup_rx().0
    }

    fn setup_rx() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>,
    ) {
        let (mut app, mut rxs) = test_app(1);
        let m = &mut app.machines[0];
        m.model.workspaces = vec![
            serde_json::from_value(serde_json::json!({
                "id": "w1", "handle": "w1", "name": "a", "auto_name": "a",
                "root_path": "/tmp", "task": null, "order": 1.0, "branch": null
            }))
            .unwrap(),
        ];
        m.model.tabs = vec![
            serde_json::from_value(serde_json::json!({
                "id": "t1", "handle": "w1:t1", "workspace": "w1", "title": null, "number": 1,
                "layout": {"Leaf": {"pane": "p1"}}, "focused_pane": "p1", "zoomed_pane": null,
                "order": 1.0
            }))
            .unwrap(),
        ];
        m.model.panes = vec![
            serde_json::from_value(serde_json::json!({
                "id": "p1", "handle": "w1:p1", "tab": "t1", "workspace": "w1", "title": null,
                "auto_title": "sh", "cwd": null, "cols": 80, "rows": 24, "child_pid": null,
                "fg_cmdline": [], "exited": false, "exit_code": null, "unread": false,
                "marked_unread": false, "pinned": false, "created_by": "user", "recovered": null
            }))
            .unwrap(),
        ];
        m.focus.workspace = Some("w1".into());
        m.focus.tab = Some("t1".into());
        m.focus.pane = Some("p1".into());
        m.panes.insert("p1".into(), PaneBuf::blank());
        (app, rxs.remove(0))
    }

    fn rect(app: &App) -> Rect {
        app.pane_rects()[0].1
    }

    #[test]
    fn kitty_hosts_get_one_transmission_and_placeholders() {
        let mut app = setup();
        app.caps.kitty_graphics = true;
        app.images.zlib = Some(false);
        let px = vec![9u8; 4 * 2 * 4];
        on_image(&mut app, 0, "h1".into(), 4, 2, kitty::zlib(&px, 1));
        on_places(&mut app, 0, "p1".into(), vec![place("h1", 2, -1, 3, 2)]);
        before_draw(&mut app);
        let out = String::from_utf8_lossy(&take_output(&mut app)).into_owned();
        assert!(out.contains("a=t,i=16777216,f=32"), "{out}");
        assert!(out.contains("a=p,U=1,i=16777216,p=1,c=3,r=2"), "{out}");
        // Second frame: nothing new to send.
        before_draw(&mut app);
        assert!(take_output(&mut app).is_empty());
        let mut g = Grid::new(app.size.0, app.size.1);
        crate::draw::compose(&app, &mut g);
        let r = rect(&app);
        // Row -1 is clipped; row 0 shows the image's second row.
        let c = g.get(r.x + 2, r.y).unwrap();
        assert_eq!(
            c.text.as_str(),
            crate::browser::placeholder(ID_BASE, 1, 0),
            "placeholder for image row 1, col 0"
        );
        assert_eq!(c.style.fg, crate::browser::id_style(ID_BASE).fg);
        assert_eq!(
            g.get(r.x + 4, r.y).unwrap().text.as_str(),
            crate::browser::placeholder(ID_BASE, 1, 2)
        );
        assert!(
            !g.get(r.x + 5, r.y)
                .unwrap()
                .text
                .as_str()
                .starts_with(kitty::PLACEHOLDER)
        );
        assert!(
            !g.get(r.x + 2, r.y + 1)
                .unwrap()
                .text
                .as_str()
                .starts_with(kitty::PLACEHOLDER),
            "only 2 rows, one scrolled off"
        );
        // Placements cleared: plain cells again.
        on_places(&mut app, 0, "p1".into(), vec![]);
        let mut g = Grid::new(app.size.0, app.size.1);
        crate::draw::compose(&app, &mut g);
        assert!(
            !g.get(r.x + 2, r.y)
                .unwrap()
                .text
                .as_str()
                .starts_with(kitty::PLACEHOLDER)
        );
    }

    #[test]
    fn hosts_without_graphics_get_a_label() {
        let mut app = setup();
        app.caps.kitty_graphics = false;
        on_image(&mut app, 0, "h1".into(), 4, 2, kitty::zlib(&[0u8; 32], 1));
        on_places(&mut app, 0, "p1".into(), vec![place("h1", 1, 0, 20, 2)]);
        before_draw(&mut app);
        assert!(take_output(&mut app).is_empty(), "no kitty commands");
        let mut g = Grid::new(app.size.0, app.size.1);
        crate::draw::compose(&app, &mut g);
        let text = crate::tasks::grid_text(&g);
        assert!(text.contains("[image 4×2]"), "{text}");
    }

    #[test]
    fn cache_is_bounded_and_evicts_unplaced_images_first() {
        let mut app = setup();
        app.caps.kitty_graphics = true;
        let big = vec![1u8; CACHE_MAX / 2 + 1];
        on_image(&mut app, 0, "placed".into(), 1, 1, big.clone());
        on_places(&mut app, 0, "p1".into(), vec![place("placed", 0, 0, 1, 1)]);
        on_image(&mut app, 0, "spare".into(), 1, 1, big.clone());
        assert!(app.images.bytes <= CACHE_MAX);
        assert!(app.images.images.contains_key(&(0, "placed".to_string())));
        assert!(!app.images.images.contains_key(&(0, "spare".to_string())));
    }

    /// A maximum-size image (32 MiB RGBA, highly compressible).
    fn max_image(app: &mut App, hash: &str, fill: u8) {
        let (w, h) = (2048u32, 4096u32);
        assert_eq!(
            w as usize * h as usize * 4,
            vk_term::limits::MAX_IMAGE_BYTES
        );
        let px = vec![fill; vk_term::limits::MAX_IMAGE_BYTES];
        on_image(app, 0, hash.into(), w, h, kitty::zlib(&px, 1));
    }

    fn count(out: &[u8], pat: &str) -> usize {
        String::from_utf8_lossy(out).matches(pat).count()
    }

    #[test]
    fn many_sizes_of_one_max_image_obey_the_output_budget() {
        for zlib in [true, false] {
            let mut app = setup();
            app.caps.kitty_graphics = true;
            app.images.zlib = Some(zlib);
            max_image(&mut app, "big", 7);
            // 100 overlapping placements of the same image, all different sizes.
            let places: Vec<_> = (0..100)
                .map(|i| place("big", (i % 7) as i32, (i % 5) as i32, 10 + i, 3 + i))
                .collect();
            on_places(&mut app, 0, "p1".into(), places);
            before_draw(&mut app);
            let out = take_output(&mut app);
            // The pixels go once, with one virtual placement for the first size.
            assert_eq!(count(&out, "a=t,"), 1, "zlib={zlib}");
            assert_eq!(count(&out, "a=p,"), 1, "zlib={zlib}");
            assert!(out.contains_sub(b"a=p,U=1,i=16777216,p=1,c=10,r=3"));
            let bound = if zlib {
                // The cached zlib stream is the payload.
                1 << 20
            } else {
                vk_term::limits::MAX_IMAGE_BYTES / 3 * 4 + (1 << 20)
            };
            assert!(out.len() <= bound, "zlib={zlib}: {} bytes", out.len());
            // The next frames send nothing: no retransmission per size.
            for _ in 0..3 {
                before_draw(&mut app);
                assert!(take_output(&mut app).is_empty());
            }
            // The other sizes show the label.
            let mut g = Grid::new(app.size.0, app.size.1);
            crate::draw::compose(&app, &mut g);
            assert!(crate::tasks::grid_text(&g).contains("[image"));
            // The first placement goes away: the image is resized, not resent.
            let places: Vec<_> = (1..100)
                .map(|i| place("big", (i % 7) as i32, (i % 5) as i32, 10 + i, 3 + i))
                .collect();
            on_places(&mut app, 0, "p1".into(), places);
            before_draw(&mut app);
            let out = take_output(&mut app);
            assert_eq!(count(&out, "a=t,"), 0);
            assert_eq!(
                String::from_utf8_lossy(&out),
                "\x1b_Ga=p,U=1,i=16777216,p=1,c=11,r=4,q=2\x1b\\"
            );
        }
    }

    #[test]
    fn transmissions_past_the_frame_budget_wait_for_the_next_frame() {
        let mut app = setup();
        app.caps.kitty_graphics = true;
        app.images.zlib = Some(false);
        max_image(&mut app, "a", 1);
        max_image(&mut app, "b", 2);
        on_places(
            &mut app,
            0,
            "p1".into(),
            vec![place("a", 0, 0, 2, 2), place("b", 4, 0, 2, 2)],
        );
        app.dirty = false;
        before_draw(&mut app);
        let first = take_output(&mut app);
        assert_eq!(count(&first, "a=t,"), 1);
        assert!(
            first.len() <= FRAME_OUT_MAX + vk_term::limits::MAX_IMAGE_BYTES / 3 * 4 + (1 << 20)
        );
        assert!(app.dirty, "another frame is asked for");
        app.dirty = false;
        before_draw(&mut app);
        let second = take_output(&mut app);
        assert_eq!(count(&second, "a=t,"), 1);
        assert!(!app.dirty);
        before_draw(&mut app);
        assert!(take_output(&mut app).is_empty());
    }

    #[test]
    fn huge_placements_only_iterate_the_visible_cells() {
        let mut app = setup();
        app.caps.kitty_graphics = true;
        app.images.zlib = Some(false);
        on_image(&mut app, 0, "h".into(), 4, 2, kitty::zlib(&[3u8; 32], 1));
        // 65535² cells, starting far above and left of the pane: drawing must not walk the
        // whole placement (4 billion cells) to clip it.
        on_places(
            &mut app,
            0,
            "p1".into(),
            vec![place("h", -30_000, -30_000, u16::MAX, u16::MAX)],
        );
        before_draw(&mut app);
        let t = crate::time::Instant::now();
        let mut g = Grid::new(app.size.0, app.size.1);
        crate::draw::compose(&app, &mut g);
        let r = rect(&app);
        assert_eq!(
            g.get(r.x, r.y).unwrap().text.as_str(),
            crate::browser::placeholder(ID_BASE, 30_000, 30_000)
        );
        let last = g.get(r.x + r.w - 1, r.y + r.h - 1).unwrap();
        assert!(last.text.as_str().starts_with(kitty::PLACEHOLDER));
        assert!(t.elapsed() < crate::time::Duration::from_secs(5));
        // Entirely outside: nothing drawn, also for the label path.
        app.caps.kitty_graphics = false;
        on_places(
            &mut app,
            0,
            "p1".into(),
            vec![place("h", -70_000, 0, u16::MAX, u16::MAX)],
        );
        let mut g = Grid::new(app.size.0, app.size.1);
        crate::draw::compose(&app, &mut g);
        assert!(!crate::tasks::grid_text(&g).contains("[image"));
    }

    #[test]
    fn images_evicted_unplaced_are_fetched_again_when_placed() {
        let (mut app, mut rx) = setup_rx();
        app.caps.kitty_graphics = true;
        let big = vec![1u8; CACHE_MAX / 2 + 1];
        on_places(&mut app, 0, "p2".into(), vec![place("new", 0, 0, 1, 1)]);
        on_image(&mut app, 0, "old".into(), 1, 1, big.clone());
        on_image(&mut app, 0, "new".into(), 1, 1, big.clone());
        assert!(!app.images.images.contains_key(&(0, "old".to_string())));
        while rx.try_recv().is_ok() {}
        // The program places the evicted image again: the pane is resynced once, and the
        // server answers with the pixels.
        on_places(&mut app, 0, "p1".into(), vec![place("old", 0, 0, 1, 1)]);
        match rx.try_recv() {
            Ok(vk_proto::render::ClientFrame::Resync { pane }) => assert_eq!(pane, "p1"),
            other => panic!("{other:?}"),
        }
        on_places(&mut app, 0, "p1".into(), vec![place("old", 0, 1, 1, 1)]);
        assert!(rx.try_recv().is_err(), "asked once");
        // The pixels arrive; both images are placed and over the budget, so one is evicted,
        // and it is not refetched (placed images would evict each other forever).
        on_image(&mut app, 0, "old".into(), 1, 1, big.clone());
        let gone: Vec<_> = ["old", "new"]
            .into_iter()
            .filter(|h| !app.images.images.contains_key(&(0, h.to_string())))
            .collect();
        assert_eq!(gone.len(), 1);
        on_places(&mut app, 0, "p3".into(), vec![place(gone[0], 0, 0, 1, 1)]);
        assert!(rx.try_recv().is_err());
    }

    trait ContainsSub {
        fn contains_sub(&self, needle: &[u8]) -> bool;
    }
    impl ContainsSub for Vec<u8> {
        fn contains_sub(&self, needle: &[u8]) -> bool {
            self.windows(needle.len()).any(|w| w == needle)
        }
    }
}
