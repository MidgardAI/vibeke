//! `!N` console-error badges (sidebar row and tab-bar chip, cleared by opening the preview) and
//! sidebar thumbnails on the shared image layer (off by default; kitty and iTerm2; read from disk
//! on this host, fetched with `thumb` from other machines; re-sent only for a newer screenshot;
//! none inside tmux).

use super::*;
use crate::drafts::tests::{commands, fleet, fleet_n, only, reply, screen};
use vk_proto::render::PushedEvent;

fn preview(handle: &str, port: u16) -> Preview {
    serde_json::from_value(json!({
        "id": format!("PV-{handle}"), "handle": handle, "machine": "m1", "pane": "p1", "task": null,
        "port": port, "path": "/", "label": "vite", "url": format!("http://localhost:{port}/"),
        "scheme": "http", "status": "up", "source": "banner", "pid": null,
        "first_seen_ms": 0, "last_seen_ms": 0
    }))
    .unwrap()
}

fn console_error(seq: i64, preview: &str, count: u64) -> PushedEvent {
    PushedEvent {
        seq,
        kind: "preview.console_error".into(),
        json: json!({"type": "preview.console_error",
            "subject": {"preview": preview, "pane": "p1", "task": null},
            "data": {"count": count, "text": "TypeError: x is undefined", "source": "exception"}})
        .to_string(),
    }
}

fn captured(seq: i64, preview: &str) -> PushedEvent {
    PushedEvent {
        seq,
        kind: "screenshot.captured".into(),
        json: json!({"type": "screenshot.captured",
            "subject": {"screenshot": "S9", "pane": "p1"}, "data": {"handle": "s9", "preview": preview}})
        .to_string(),
    }
}

#[test]
fn console_errors_badge_chip_and_row_until_opened() {
    let (mut app, mut rxs) = fleet();
    app.sidebar = true;
    app.machines[0].model.previews = vec![preview("v4", 5173)];
    assert!(!screen(&app).contains('!'));
    crate::push::on_events(
        &mut app,
        0,
        vec![console_error(1, "v4", 2), console_error(2, "v4", 1)],
        false,
    );
    assert_eq!(badge(&app, 0, "v4"), Some(3));
    let rows = crate::draw::sidebar_rows(&app);
    let last: String = rows
        .last()
        .unwrap()
        .segs
        .iter()
        .map(|(s, _)| s.as_str())
        .collect();
    assert!(last.contains("!3"), "{last}");
    let chips = crate::browser::chip_entries(&app, 10);
    assert!(chips[0].2.contains("◉ vite :5173 !3"), "{:?}", chips[0].2);
    assert!(screen(&app).contains("!3"));
    // Other previews and unrelated errors don't count.
    crate::push::on_events(&mut app, 0, vec![console_error(3, "v9", 1)], false);
    assert_eq!(badge(&app, 0, "v4"), Some(3));
    // Opening the preview (here: as a pane) clears it.
    let p = app.machines[0].model.previews[0].clone();
    crate::browser::open_preview(&mut app, 0, &p, Some("p1".into()));
    assert_eq!(badge(&app, 0, "v4"), None);
    assert!(!crate::browser::chip_entries(&app, 10)[0].2.contains('!'));
    let _ = commands(&mut rxs[0]);
    // Large counts are capped in the text.
    crate::push::on_events(&mut app, 0, vec![console_error(4, "v4", 500)], false);
    assert_eq!(badge_text(&app, 0, "v4"), " !99+");
}

fn thumb_app(
    kitty: bool,
) -> (
    crate::app::App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
    tempfile::TempDir,
    String,
) {
    let (mut app, rxs) = fleet_n(2);
    app.sidebar = true;
    app.sidebar_w = 30;
    app.caps.kitty_graphics = kitty;
    app.caps.iterm2_images = !kitty;
    app.imgview.tmux = false;
    app.caps.cell_w = 10;
    app.caps.cell_h = 20;
    app.machines[0].model.previews = vec![preview("v4", 5173)];
    let dir = tempfile::tempdir().unwrap();
    app.imgview.gfx_dir = Some(dir.path().join("gfx"));
    let png = dir.path().join("s1.png");
    // 80×40 px, so it is scaled down to fit 4 cells × 1 row of 10×20 px: 40×20.
    let img = vec![200u8; 80 * 40 * 4];
    std::fs::write(
        &png,
        vk_browser::frame::encode_png(80, 40, &img, true).unwrap(),
    )
    .unwrap();
    let path = png.to_string_lossy().into_owned();
    (app, rxs, dir, path)
}

fn list_reply(id: &str, path: &str) -> Value {
    json!({"screenshots": [{"id": id, "handle": "s1", "path_on_machine": path, "preview": "v4"}], "count": 1})
}

fn out(app: &mut crate::app::App) -> String {
    let s = String::from_utf8_lossy(&app.browser.out).to_string();
    app.browser.out.clear();
    s
}

fn osc1337(out: &[u8]) -> usize {
    String::from_utf8_lossy(out)
        .matches("\x1b]1337;File=")
        .count()
}

#[test]
fn thumbnails_are_off_by_default_and_need_graphics() {
    for kitty in [true, false] {
        let (mut app, mut rxs, _dir, _) = thumb_app(kitty);
        assert!(!app.config.ui.sidebar.preview_thumbnails);
        observe(&mut app);
        assert!(commands(&mut rxs[0]).is_empty());
        // On, but the terminal has no graphics at all.
        app.config.ui.sidebar.preview_thumbnails = true;
        app.caps.kitty_graphics = false;
        app.caps.iterm2_images = false;
        observe(&mut app);
        assert!(commands(&mut rxs[0]).is_empty());
        // On, with graphics, but inside tmux.
        app.caps.kitty_graphics = kitty;
        app.caps.iterm2_images = !kitty;
        app.imgview.tmux = true;
        observe(&mut app);
        assert!(commands(&mut rxs[0]).is_empty());
        assert!(placements(&app).is_empty());
    }
}

#[test]
fn local_thumbnail_is_read_scaled_and_drawn() {
    for kitty in [true, false] {
        let (mut app, mut rxs, _dir, path) = thumb_app(kitty);
        app.config.ui.sidebar.preview_thumbnails = true;
        // The latest screenshot is asked for once.
        observe(&mut app);
        observe(&mut app);
        let (req, p) = only(&commands(&mut rxs[0]), "screenshot.list");
        assert_eq!(p, json!({"preview": "v4", "limit": 1}));
        assert!(
            placements(&app).is_empty(),
            "nothing to draw before the reply"
        );
        reply(&mut app, 0, req, list_reply("S1", &path));
        // Scaled down to the thumbnail size under a key of its own; the full-size key is free.
        let png = app.imgview.image("thumb:0:S1").expect("thumbnail cached");
        assert_eq!(crate::image_view::png_size(png), Some((40, 20)));
        assert!(app.imgview.image("0:S1").is_none());
        // At the right end of the preview row.
        let rows = crate::draw::sidebar_rows(&app);
        let y = rows.len() as u16; // last row = the preview row (row i is drawn at y = i + 1)
        let ps = placements(&app);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].slot, "thumb:0:v4");
        assert_eq!(ps[0].key, "thumb:0:S1");
        assert_eq!(
            ps[0].rect,
            Rect {
                x: app.sidebar_w - 5,
                y,
                w: THUMB_COLS,
                h: 1
            }
        );
        assert_eq!(crate::image_view::placements(&app), ps);
        if kitty {
            crate::image_view::before_draw(&mut app);
            let o = out(&mut app);
            let id = crate::image_view::ID_BASE;
            assert!(o.contains(&format!("a=T,i={id},f=100")), "{o}");
            assert!(o.contains("U=1,c=4,r=1"), "{o}");
            // Placeholder cells, with the id as colour.
            let mut g = crate::screen::Grid::new(app.size.0, app.size.1);
            crate::draw::compose(&app, &mut g);
            let cell = g.get(app.sidebar_w - 2, y).unwrap();
            assert!(
                cell.text.as_str().starts_with('\u{10EEEE}'),
                "{:?}",
                cell.text.as_str()
            );
            // Unchanged: nothing re-sent.
            crate::image_view::before_draw(&mut app);
            assert!(out(&mut app).is_empty());
        } else {
            assert_eq!(osc1337(&app.draw_bytes()), 1);
            assert_eq!(osc1337(&app.draw_bytes()), 0, "nothing again");
        }
        // A newer screenshot of that preview: asked again, the old image replaced.
        crate::push::on_events(&mut app, 0, vec![captured(1, "v4")], false);
        observe(&mut app);
        let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
        reply(&mut app, 0, req, list_reply("S2", &path));
        assert!(app.imgview.image("thumb:0:S2").is_some());
        assert_eq!(placements(&app)[0].key, "thumb:0:S2");
        if kitty {
            crate::image_view::before_draw(&mut app);
            let o = out(&mut app);
            let id = crate::image_view::ID_BASE;
            assert!(o.contains(&format!("a=d,d=I,i={id}")), "{o}");
            assert!(o.contains(&format!("a=T,i={id},f=100")), "{o}");
        } else {
            assert_eq!(osc1337(&app.draw_bytes()), 1);
        }
        // A capture of another preview doesn't re-ask.
        crate::push::on_events(&mut app, 0, vec![captured(2, "v7")], false);
        observe(&mut app);
        assert!(commands(&mut rxs[0]).is_empty());
        // Switched off: the image goes away.
        app.config.ui.sidebar.preview_thumbnails = false;
        assert!(placements(&app).is_empty());
        if kitty {
            crate::image_view::before_draw(&mut app);
            assert!(out(&mut app).contains(&format!("a=d,d=I,i={}", crate::image_view::ID_BASE)));
        }
        let mut g = crate::screen::Grid::new(app.size.0, app.size.1);
        crate::draw::compose(&app, &mut g);
        assert!(!crate::tasks::grid_text(&g).contains('\u{10EEEE}'));
    }
}

#[test]
fn iterm_sends_a_thumbnail_after_the_frame() {
    let (mut app, mut rxs, _dir, path) = thumb_app(false);
    app.config.ui.sidebar.preview_thumbnails = true;
    observe(&mut app);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    reply(&mut app, 0, req, list_reply("S1", &path));
    let o = String::from_utf8_lossy(&app.draw_bytes()).to_string();
    // After the frame: drawing fits the sidebar first, which can move the row.
    let p = placements(&app).remove(0);
    assert_eq!(o.matches("\x1b]1337;File=").count(), 1, "{o:?}");
    assert!(
        o.contains(&format!(
            "\x1b[{};{}H\x1b]1337;File=inline=1;",
            p.rect.y + 1,
            p.rect.x + 1
        )),
        "{o:?}"
    );
    assert!(o.contains("width=4;height=1;"), "{o:?}");
}

#[test]
fn kitty_gives_each_thumbnail_its_own_id() {
    let (mut app, mut rxs, _dir, path) = thumb_app(true);
    app.config.ui.sidebar.preview_thumbnails = true;
    app.machines[0].model.previews = vec![preview("v4", 5173), preview("v5", 5174)];
    observe(&mut app);
    let cmds = commands(&mut rxs[0]);
    assert_eq!(cmds.iter().filter(|c| c.1 == "screenshot.list").count(), 2);
    for (req, method, params) in cmds {
        assert_eq!(method, "screenshot.list");
        let (id, handle) = match params["preview"].as_str().unwrap() {
            "v4" => ("S1", "v4"),
            _ => ("S2", "v5"),
        };
        let list = json!({"screenshots": [{"id": id, "handle": "s", "path_on_machine": path, "preview": handle}], "count": 1});
        reply(&mut app, 0, req, list);
    }
    let ps = placements(&app);
    assert_eq!(ps.len(), 2, "{ps:?}");
    assert_ne!(ps[0].slot, ps[1].slot);
    crate::image_view::before_draw(&mut app);
    let o = out(&mut app);
    let base = crate::image_view::ID_BASE;
    assert!(o.contains(&format!("a=T,i={base},f=100")), "{o}");
    assert!(o.contains(&format!("a=T,i={},f=100", base + 1)), "{o}");
}

#[test]
fn a_screenshot_is_a_thumbnail_and_in_the_gallery_at_once() {
    for kitty in [true, false] {
        let (mut app, mut rxs, _dir, path) = thumb_app(kitty);
        app.config.ui.sidebar.preview_thumbnails = true;
        observe(&mut app);
        let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
        reply(&mut app, 0, req, list_reply("S1", &path));
        // The gallery's full view of the same screenshot (full-size key `0:S1`).
        crate::gallery::open(
            &mut app,
            0,
            crate::gallery::Scope::All,
            crate::gallery::View::Full,
            false,
        );
        let shot = json!({"id": "S1", "handle": "s1", "path_on_machine": path, "exists": true,
            "environment": {"kind": "agent"}, "created_at_ms": 1, "width": 80, "height": 40});
        if let Some(g) = &mut app.gallery.view {
            g.shots = vec![crate::gallery::Shot::parse(&shot).unwrap()];
            g.loading = false;
        }
        crate::gallery::ensure_local_image(&mut app);
        let small = app.imgview.image("thumb:0:S1").unwrap().to_vec();
        let full = app.imgview.image("0:S1").unwrap().to_vec();
        assert_ne!(small, full, "the tiny image never replaces the full one");
        let ps = crate::image_view::placements(&app);
        let slots: Vec<&str> = ps.iter().map(|p| p.slot.as_str()).collect();
        assert_eq!(slots, ["gallery", "thumb:0:v4"], "{ps:?}");
        assert_eq!(
            (ps[0].key.as_str(), ps[1].key.as_str()),
            ("0:S1", "thumb:0:S1")
        );
        if kitty {
            crate::image_view::before_draw(&mut app);
            let o = out(&mut app);
            let base = crate::image_view::ID_BASE;
            assert!(o.contains(&format!("a=T,i={base},f=100")), "{o}");
            assert!(o.contains(&format!("a=T,i={},f=100", base + 1)), "{o}");
        }
    }
}

#[test]
fn remote_machines_fetch_a_thumbnail() {
    for kitty in [true, false] {
        let (mut app, mut rxs, _dir, _) = thumb_app(kitty);
        app.config.ui.sidebar.preview_thumbnails = true;
        // The preview lives on machine 1, which isn't this host: its blobs are fetched.
        app.machines[0].model.previews.clear();
        app.machines[1].model.previews = vec![preview("v4", 5173)];
        assert!(!app.machines[1].local);
        observe(&mut app);
        assert!(commands(&mut rxs[0]).is_empty());
        let (req, _) = only(&commands(&mut rxs[1]), "screenshot.list");
        reply(&mut app, 1, req, list_reply("S1", "/remote/s1.png"));
        // `thumb` is clamped to 64 px wide (4 cells × 10 px is less).
        let (req, p) = only(&commands(&mut rxs[1]), "screenshot.get");
        assert_eq!(p, json!({"id": "S1", "inline": true, "thumb": 64}));
        assert!(placements(&app).is_empty(), "still on its way");
        // Asking again while it is on its way sends nothing.
        observe(&mut app);
        assert!(commands(&mut rxs[1]).is_empty());
        let png = vk_browser::frame::encode_png(64, 32, &vec![200u8; 64 * 32 * 4], true).unwrap();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
        reply(&mut app, 1, req, json!({"data_b64": b64}));
        assert!(app.imgview.image("1:S1@64").is_some());
        let ps = placements(&app);
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(
            (ps[0].slot.as_str(), ps[0].key.as_str()),
            ("thumb:1:v4", "1:S1@64")
        );
        observe(&mut app);
        assert!(commands(&mut rxs[1]).is_empty(), "cached");
    }
}

#[test]
fn a_deleted_screenshot_leaves_its_thumbnail_and_the_preview_looks_again() {
    let (mut app, mut rxs, _dir, path) = thumb_app(false);
    app.config.ui.sidebar.preview_thumbnails = true;
    observe(&mut app);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    reply(&mut app, 0, req, list_reply("S1", &path));
    observe(&mut app);
    assert!(app.imgview.image("thumb:0:S1").is_some());
    assert_eq!(placements(&app).len(), 1);
    crate::images::forget(&mut app, 0, &["S1".to_string()]);
    assert!(app.imgview.image("thumb:0:S1").is_none());
    assert!(placements(&app).is_empty());
    observe(&mut app);
    only(&commands(&mut rxs[0]), "screenshot.list");
}

#[test]
fn a_disconnect_lets_the_previews_be_asked_again() {
    let (mut app, mut rxs, _dir, _path) = thumb_app(false);
    app.config.ui.sidebar.preview_thumbnails = true;
    observe(&mut app);
    only(&commands(&mut rxs[0]), "screenshot.list");
    observe(&mut app);
    assert!(commands(&mut rxs[0]).is_empty(), "asked once");
    on_disconnected(&mut app, 0);
    observe(&mut app);
    only(&commands(&mut rxs[0]), "screenshot.list");
    // Offline: not marked as asked, so the preview is asked for once the machine is back.
    on_disconnected(&mut app, 0);
    app.machines[0].tx = None;
    observe(&mut app);
    assert!(app.previews_ui.asked.is_empty());
}
