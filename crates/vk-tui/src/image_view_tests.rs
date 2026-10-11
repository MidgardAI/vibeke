//! Image view tests (spec 18 §11.1): per-slot kitty ids, iTerm2 sends after the frame (once,
//! again after a resize or when uncovered), occlusion, repaint of the cells under a hidden,
//! moved or removed image, browser canvases restored when a popup closes, and the hints.

use super::*;
use crate::app::{Mode, Popup};
use crate::drafts::tests::{fleet, fleet_n, named};
use crate::gallery::{Scope, View};
use vk_proto::input::NamedKey;

fn tiny_png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend_from_slice(&100u32.to_be_bytes());
    v.extend_from_slice(&50u32.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}

fn caps(app: &mut App, kitty: bool) {
    app.caps.kitty_graphics = kitty;
    app.caps.iterm2_images = !kitty;
    app.caps.cell_w = 10;
    app.caps.cell_h = 20;
    app.imgview.tmux = false;
}

/// The gallery's full view of a local screenshot whose PNG is at `path`.
fn gallery_full(app: &mut App, path: &str) {
    crate::gallery::open(app, 0, Scope::All, View::Full, false);
    let shot = json!({"id": "S1", "handle": "s1", "path_on_machine": path, "exists": true,
        "environment": {"kind": "agent"}, "created_at_ms": 1, "width": 100, "height": 50});
    if let Some(g) = &mut app.gallery.view {
        g.shots = vec![crate::gallery::Shot::parse(&shot).unwrap()];
        g.loading = false;
    }
    crate::gallery::ensure_local_image(app);
}

fn osc1337(out: &[u8]) -> usize {
    String::from_utf8_lossy(out)
        .matches("\x1b]1337;File=")
        .count()
}

fn with_png() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("s1.png");
    std::fs::write(&p, tiny_png()).unwrap();
    let s = p.to_string_lossy().into_owned();
    (dir, s)
}

#[test]
fn kitty_gives_each_slot_its_own_id() {
    let (mut app, _rxs) = fleet();
    caps(&mut app, true);
    let dir = tempfile::tempdir().unwrap();
    app.imgview.gfx_dir = Some(dir.path().join("gfx"));
    app.imgview.put("0:A".into(), tiny_png());
    let st = &mut app.imgview;
    let a = alloc_id(st, "viewer").unwrap();
    let b = alloc_id(st, "thumb:0:v1").unwrap();
    let c = alloc_id(st, "thumb:0:v2").unwrap();
    assert!(a != b && b != c && a != c);
    assert_eq!(alloc_id(st, "viewer"), Some(a));
    assert!((ID_BASE..ID_BASE + ID_SPAN).contains(&c));
}

#[test]
fn kitty_gallery_transmits_once_and_deletes_when_closed() {
    let (dir, path) = with_png();
    let (mut app, _rxs) = fleet();
    caps(&mut app, true);
    app.imgview.gfx_dir = Some(dir.path().join("gfx"));
    gallery_full(&mut app, &path);
    before_draw(&mut app);
    let out = String::from_utf8_lossy(&std::mem::take(&mut app.browser.out)).to_string();
    assert!(out.contains(&format!("a=T,i={ID_BASE},f=100")), "{out}");
    before_draw(&mut app);
    assert!(
        app.browser.out.is_empty(),
        "nothing again when nothing changed"
    );
    // The same image at another size is transmitted again under the same id.
    app.size.0 -= 30;
    before_draw(&mut app);
    let out = String::from_utf8_lossy(&std::mem::take(&mut app.browser.out)).to_string();
    assert!(out.contains(&format!("a=d,d=I,i={ID_BASE}")), "{out}");
    assert!(out.contains(&format!("a=T,i={ID_BASE}")), "{out}");
    app.on_key(named(NamedKey::Escape));
    before_draw(&mut app);
    let out = String::from_utf8_lossy(&app.browser.out).to_string();
    assert!(out.contains(&format!("a=d,d=I,i={ID_BASE}")), "{out}");
    assert!(app.imgview.kitty.is_empty() && app.imgview.ids.is_empty());
}

#[test]
fn iterm_sends_after_the_frame_once() {
    let (_dir, path) = with_png();
    let (mut app, _rxs) = fleet();
    caps(&mut app, false);
    gallery_full(&mut app, &path);
    let out = app.draw_bytes();
    assert_eq!(osc1337(&out), 1);
    let s = String::from_utf8_lossy(&out);
    let i = s.find("\x1b]1337;").unwrap();
    // After the grid diff, never before it; aspect kept, cursor not moved.
    assert!(s[..i].contains("screenshots"), "the frame comes first");
    assert!(s[i..].contains("preserveAspectRatio=1;doNotMoveCursor=1"));
    let p = crate::gallery::placements(&app).unwrap();
    assert!(s.contains(&format!(
        "\x1b[{};{}H\x1b]1337;File=inline=1;size={};width={};height={}",
        p.rect.y + 1,
        p.rect.x + 1,
        tiny_png().len(),
        p.rect.w,
        p.rect.h
    )));
    assert_eq!(
        osc1337(&app.draw_bytes()),
        0,
        "nothing again when nothing changed"
    );
    // A resize repaints everything: the image goes again.
    app.size.1 -= 1;
    assert_eq!(osc1337(&app.draw_bytes()), 1);
}

#[test]
fn iterm_hides_a_covered_image_and_sends_it_when_uncovered() {
    let (_dir, path) = with_png();
    let (mut app, _rxs) = fleet();
    caps(&mut app, false);
    gallery_full(&mut app, &path);
    app.draw_bytes();
    let p = crate::gallery::placements(&app).unwrap();
    // Something drawn over one cell of the image (a toast, a menu): hidden, cells repainted.
    let mut grid = Grid::new(app.size.0, app.size.1);
    crate::draw::compose(&app, &mut grid);
    grid.put_str(p.rect.x + 1, p.rect.y, "toast", app.theme.text(), 5);
    after_compose(&mut app, &grid);
    assert!(app.imgview.iterm.is_empty());
    assert!(app.imgview.queued.is_empty());
    let cell = app.prev.get(p.rect.x, p.rect.y).unwrap().clone();
    assert_eq!(
        cell.text.as_str(),
        "\u{1}",
        "the old image's cells are marked for a repaint"
    );
    app.prev = grid;
    // Uncovered again: sent again.
    assert_eq!(osc1337(&app.draw_bytes()), 1);
}

/// Compose a frame and run [`after_compose`] (no diff yet): what is marked for a repaint.
fn compose_only(app: &mut App) -> Grid {
    let mut grid = Grid::new(app.size.0, app.size.1);
    crate::draw::compose(app, &mut grid);
    after_compose(app, &grid);
    grid
}

fn marked(app: &App, r: Rect) -> bool {
    (r.y..r.y + r.h).all(|y| {
        (r.x..r.x + r.w).all(|x| {
            app.prev
                .get(x, y)
                .is_some_and(|c| c.text.as_str() == "\u{1}")
        })
    })
}

#[test]
fn iterm_repaints_cells_when_an_image_moves_or_goes() {
    let (_dir, path) = with_png();
    let (mut app, _rxs) = fleet();
    caps(&mut app, false);
    gallery_full(&mut app, &path);
    app.draw_bytes();
    let p = crate::gallery::placements(&app).unwrap();
    // A notice line pushes the image down: the old rectangle is repainted, the image re-sent.
    if let Some(g) = &mut app.gallery.view {
        g.notice = Some("hello".into());
    }
    let grid = compose_only(&mut app);
    let q = crate::gallery::placements(&app).unwrap();
    assert_eq!(q.rect.y, p.rect.y + 1);
    assert!(marked(&app, p.rect));
    assert_eq!(app.imgview.queued, vec![("0:S1".to_string(), q.rect)]);
    app.prev = grid;
    app.imgview.queued.clear();
    // Image → blank: leaving the full view repaints the whole rectangle and sends nothing.
    app.on_key(named(NamedKey::Escape));
    compose_only(&mut app);
    assert!(marked(&app, q.rect));
    assert!(app.imgview.iterm.is_empty() && app.imgview.queued.is_empty());
}

#[test]
fn iterm_repaint_after_hiding_the_sidebar() {
    let (_dir, path) = with_png();
    let (mut app, _rxs) = fleet();
    caps(&mut app, false);
    app.sidebar = true;
    gallery_full(&mut app, &path);
    app.draw_bytes();
    let p = crate::gallery::placements(&app).unwrap();
    app.sidebar = false;
    let out = app.draw_bytes();
    let q = crate::gallery::placements(&app).unwrap();
    assert_ne!(p.rect.x, q.rect.x, "the pane area moved");
    assert_eq!(osc1337(&out), 1, "sent again at its new place");
    assert!(app.imgview.iterm.values().all(|(_, r)| *r == q.rect));
}

#[test]
fn browser_canvas_restored_when_a_popup_closes() {
    let (mut app, _rxs) = fleet();
    caps(&mut app, false);
    app.browser.panes.insert(
        "p1".into(),
        crate::browser::PaneMedia {
            canvas: Some(vk_browser::frame::Rgba {
                width: 2,
                height: 2,
                data: vec![0; 16],
            }),
            ..Default::default()
        },
    );
    app.mode = Mode::Popup(Popup::Gallery);
    let mut out = Vec::new();
    crate::browser::after_write(&mut app, &mut out);
    assert!(!app.browser.panes["p1"].canvas_dirty);
    app.mode = Mode::Normal;
    crate::browser::after_write(&mut app, &mut out);
    // Marked (and sent, when the pane is on screen) without a new media frame.
    let pm = &app.browser.panes["p1"];
    assert!(pm.canvas_dirty || pm.last_inline.is_some());
}

#[test]
fn hints_name_the_reason() {
    let (mut app, _rxs) = fleet();
    caps(&mut app, false);
    let open = "[o] open image locally";
    assert_eq!(hint(&app, "0:S1", open), None);
    load_local(&mut app, "0:S1", "/nonexistent/s1.png", true);
    assert!(hint(&app, "0:S1", open).unwrap().contains("gone"));
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.png");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(MAX_IMAGE as u64 + 1)
        .unwrap();
    load_local(&mut app, "0:S2", &big.to_string_lossy(), true);
    let h = hint(&app, "0:S2", open).unwrap();
    assert!(h.contains("too large") && h.contains(open), "{h}");
    let err = Err(RpcErr {
        kind: "internal".into(),
        message: "link down".into(),
        details: Value::Null,
    });
    assert!(on_fetched(&mut app, "1:S3@400", &err).is_some());
    let h = hint(&app, "1:S3@400", open).unwrap();
    assert!(h.contains("link down"), "{h}");
    // No graphics, and tmux.
    app.caps.iterm2_images = false;
    assert!(hint(&app, "0:S9", open).unwrap().contains("no graphics"));
    caps(&mut app, true);
    app.imgview.tmux = true;
    assert!(hint(&app, "0:S9", open).unwrap().contains("tmux"));
    assert!(placements(&app).is_empty());
}

#[test]
fn remote_keys_carry_the_size_and_deletions_forget_them() {
    let (mut app, _rxs) = fleet_n(2);
    caps(&mut app, false);
    assert_eq!(key(&app, 0, "S1", 300), "0:S1");
    assert_eq!(key(&app, 1, "S1", 300), "1:S1@300");
    app.imgview.put("1:S1@300".into(), tiny_png());
    app.imgview.put("1:S1@600".into(), tiny_png());
    app.imgview.put("1:S10@600".into(), tiny_png());
    app.imgview.forget(1, "S1");
    assert!(app.imgview.image("1:S1@300").is_none() && app.imgview.image("1:S1@600").is_none());
    assert!(app.imgview.image("1:S10@600").is_some());
    assert_eq!(thumb_px(&app, 1), 64);
    assert_eq!(thumb_px(&app, 9), 128, "rounded up to 64 px");
    assert_eq!(thumb_px(&app, 500), 1024);
}
