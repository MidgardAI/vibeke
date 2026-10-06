//! Inbound kitty graphics on terminal panes (03 §9): images a pane's program placed, as the
//! server forwards them (`ServerFrame::Image` once per hash, `ServerFrame::PaneImages` per
//! pane), re-placed on the host.
//!
//! With kitty graphics on the host, each (image, cell size) is transmitted once as a virtual
//! placement (`a=T,U=1,c=,r=`) under an id from [`ID_BASE`] up, and the pane cells it covers
//! are drawn as unicode placeholders (`U+10EEEE` + row/column diacritics, the id in the
//! foreground colour), like browser panes; so images clip at pane edges, scroll with the text
//! and stay under popups. Without kitty graphics the first visible row of the image shows a
//! `[image W×H]` label instead. Pixels are kept (zlib) up to [`CACHE_MAX`]; images no pane
//! places are evicted first.

use crate::app::App;
use crate::screen::Grid;
use std::collections::HashMap;
use vk_browser::kitty::{self, Header, PixelFormat};
use vk_proto::layout::Rect;
use vk_proto::render::ImagePlace;

/// Host image ids for pane images: above every browser tile, gallery and thumbnail id (the
/// fourth placeholder diacritic carries the top byte).
pub const ID_BASE: u32 = 1 << 24;
/// Compressed pixels kept on the client.
pub const CACHE_MAX: usize = 256 << 20;

struct Img {
    width: u32,
    height: u32,
    rgba_z: Vec<u8>,
}

#[derive(Default)]
pub struct State {
    images: HashMap<(usize, String), Img>,
    places: HashMap<(usize, String), Vec<ImagePlace>>,
    /// Host id per (machine, hash, cols, rows) already transmitted.
    host_ids: HashMap<(usize, String, u16, u16), u32>,
    next_id: u32,
    bytes: usize,
    /// Kitty commands for the host, written before the frame's grid diff.
    pub out: Vec<u8>,
}

/// `ServerFrame::Image`.
pub fn on_image(app: &mut App, mi: usize, hash: String, width: u32, height: u32, rgba_z: Vec<u8>) {
    let st = &mut app.images;
    if let Some(old) = st.images.remove(&(mi, hash.clone())) {
        st.bytes -= old.rgba_z.len();
    }
    st.bytes += rgba_z.len();
    st.images.insert(
        (mi, hash),
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
        let victim = st
            .images
            .keys()
            .find(|k| !placed(k))
            .or_else(|| st.images.keys().next())
            .cloned();
        let Some(k) = victim else { break };
        if let Some(img) = st.images.remove(&k) {
            st.bytes -= img.rgba_z.len();
        }
        let gone: Vec<_> = st
            .host_ids
            .keys()
            .filter(|(m, h, _, _)| *m == k.0 && *h == k.1)
            .cloned()
            .collect();
        for g in gone {
            if let Some(id) = st.host_ids.remove(&g) {
                kitty::delete_image(&mut st.out, id);
            }
        }
    }
}

/// `ServerFrame::PaneImages`.
pub fn on_places(app: &mut App, mi: usize, pane: String, places: Vec<ImagePlace>) {
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

/// Before composing: transmit (once) the images visible panes place.
pub fn before_draw(app: &mut App) {
    if !app.caps.kitty_graphics || app.images.places.is_empty() {
        return;
    }
    let mi = app.cur;
    let visible: Vec<String> = app.pane_rects().into_iter().map(|(p, _)| p).collect();
    let st = &mut app.images;
    let mut want: Vec<(String, u16, u16)> = Vec::new();
    for pane in &visible {
        if let Some(ps) = st.places.get(&(mi, pane.clone())) {
            for p in ps {
                want.push((p.hash.clone(), p.cols.max(1), p.rows.max(1)));
            }
        }
    }
    for (hash, cols, rows) in want {
        let key = (mi, hash.clone(), cols, rows);
        if st.host_ids.contains_key(&key) {
            continue;
        }
        let Some(img) = st.images.get(&(mi, hash)) else {
            continue;
        };
        let len = img.width as usize * img.height as usize * 4;
        let Ok(px) = kitty::unzlib_limited(&img.rgba_z, len) else {
            continue;
        };
        if px.len() != len {
            continue;
        }
        let id = ID_BASE + st.next_id % 0x00ff_0000;
        st.next_id += 1;
        let mut h = Header::new(id, PixelFormat::Rgba, img.width, img.height);
        h.virtual_cells = Some((cols, rows));
        h.quiet = 2;
        kitty::transmit_direct(&mut st.out, &h, &px);
        st.host_ids.insert(key, id);
    }
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
    for p in places {
        let (c0, r0) = (p.col, p.row);
        let visible = |x: i32, y: i32| x >= 0 && y >= 0 && (x as u16) < r.w && (y as u16) < r.h;
        let id = app
            .images
            .host_ids
            .get(&(mi, p.hash.clone(), p.cols.max(1), p.rows.max(1)))
            .copied();
        match id {
            Some(id) if app.caps.kitty_graphics => {
                for cy in 0..p.rows {
                    for cx in 0..p.cols {
                        let (x, y) = (c0 + cx as i32, r0 + cy as i32);
                        if visible(x, y) {
                            g.put_grapheme(
                                r.x + x as u16,
                                r.y + y as u16,
                                &crate::browser::placeholder(id, cy, cx),
                                crate::browser::id_style(id),
                            );
                        }
                    }
                }
            }
            _ => {
                // No kitty graphics on the host (or pixels not here yet): a label on the
                // first visible row.
                let label = format!("[image {}×{}]", p.width, p.height);
                if let Some(cy) = (0..p.rows as i32).find(|cy| visible(c0.max(0), r0 + cy)) {
                    let x = c0.max(0) as u16;
                    let w = (p.cols as i32 - (x as i32 - c0)).max(0) as u16;
                    g.put_str(
                        r.x + x,
                        r.y + (r0 + cy) as u16,
                        &label,
                        t.dim(),
                        w.min(r.w - x),
                    );
                }
            }
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
        }
    }

    fn setup() -> App {
        let (mut app, _rx) = test_app(1);
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
        app
    }

    fn rect(app: &App) -> Rect {
        app.pane_rects()[0].1
    }

    #[test]
    fn kitty_hosts_get_one_transmission_and_placeholders() {
        let mut app = setup();
        app.caps.kitty_graphics = true;
        let px = vec![9u8; 4 * 2 * 4];
        on_image(&mut app, 0, "h1".into(), 4, 2, kitty::zlib(&px, 1));
        on_places(&mut app, 0, "p1".into(), vec![place("h1", 2, -1, 3, 2)]);
        before_draw(&mut app);
        let out = String::from_utf8_lossy(&take_output(&mut app)).into_owned();
        assert!(out.contains("a=T,i=16777216,f=32"), "{out}");
        assert!(out.contains("U=1,c=3,r=2"), "{out}");
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
}
