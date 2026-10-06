//! `!N` console-error badges (sidebar row and tab-bar chip, cleared by opening the preview) and
//! sidebar thumbnails (off by default; kitty only; local machines only; reserved image ids;
//! re-sent only for a newer screenshot; deleted when switched off).

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

fn thumb_app() -> (
    crate::app::App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
    tempfile::TempDir,
    String,
) {
    let (mut app, rxs) = fleet_n(2);
    app.sidebar = true;
    app.sidebar_w = 30;
    app.caps.kitty_graphics = true;
    app.caps.cell_w = 10;
    app.caps.cell_h = 20;
    app.machines[0].model.previews = vec![preview("v4", 5173)];
    let dir = tempfile::tempdir().unwrap();
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

#[test]
fn thumbnails_are_off_by_default_and_need_kitty() {
    let (mut app, mut rxs, _dir, _) = thumb_app();
    assert!(!app.config.ui.sidebar.preview_thumbnails);
    crate::preview_ui::before_draw(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(out(&mut app).is_empty());
    // On, but the terminal has no kitty graphics (iTerm2 images don't do placeholders).
    app.config.ui.sidebar.preview_thumbnails = true;
    app.caps.kitty_graphics = false;
    app.caps.iterm2_images = true;
    crate::preview_ui::before_draw(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn thumbnail_lifecycle() {
    let (mut app, mut rxs, _dir, path) = thumb_app();
    app.config.ui.sidebar.preview_thumbnails = true;
    // The latest screenshot is asked for once.
    crate::preview_ui::before_draw(&mut app);
    crate::preview_ui::before_draw(&mut app);
    let (req, p) = only(&commands(&mut rxs[0]), "screenshot.list");
    assert_eq!(p, json!({"preview": "v4", "limit": 1}));
    assert!(out(&mut app).is_empty(), "nothing to draw before the reply");
    reply(&mut app, 0, req, list_reply("S1", &path));
    // Transmitted as raw RGBA, a virtual 4×1-cell placement, in the reserved id range.
    assert_eq!(app.previews_ui.thumbs.len(), 1, "decoded {path}");
    crate::preview_ui::before_draw(&mut app);
    let o = out(&mut app);
    let id = THUMB_ID_BASE;
    assert!(
        o.contains(&format!("a=T,i={id},f=32,t=d,q=2,s=40,v=20")),
        "{o}"
    );
    assert!(o.contains("U=1,c=4,r=1"), "{o}");
    // The placeholders land at the right end of the preview row, with the id as colour.
    let mut g = crate::screen::Grid::new(app.size.0, app.size.1);
    crate::draw::compose(&app, &mut g);
    let text = crate::tasks::grid_text(&g);
    assert!(text.contains('\u{10EEEE}'), "{text}");
    let rows = crate::draw::sidebar_rows(&app);
    let y = rows.len() as u16; // last row = the preview row (row i is drawn at y = i + 1)
    let x_end = app.sidebar_w - 1; // sidebar border column
    let cell = g.get(x_end - 1, y).unwrap();
    assert!(
        cell.text.as_str().starts_with('\u{10EEEE}'),
        "thumbnail cell at ({}, {y}): {:?}",
        x_end - 1,
        cell.text.as_str()
    );
    // Unchanged: nothing re-sent.
    crate::preview_ui::before_draw(&mut app);
    assert!(out(&mut app).is_empty());
    // A newer screenshot of that preview: asked again, old image replaced.
    crate::push::on_events(&mut app, 0, vec![captured(1, "v4")], false);
    crate::preview_ui::before_draw(&mut app);
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    reply(&mut app, 0, req, list_reply("S2", &path));
    crate::preview_ui::before_draw(&mut app);
    let o = out(&mut app);
    assert!(o.contains(&format!("a=d,d=I,i={id}")), "{o}");
    assert!(o.contains(&format!("a=T,i={id},f=32")), "{o}");
    // A capture of another preview doesn't re-ask.
    crate::push::on_events(&mut app, 0, vec![captured(2, "v7")], false);
    crate::preview_ui::before_draw(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
    // Switched off: the image goes away.
    app.config.ui.sidebar.preview_thumbnails = false;
    crate::preview_ui::before_draw(&mut app);
    assert!(out(&mut app).contains(&format!("a=d,d=I,i={id}")));
    let mut g = crate::screen::Grid::new(app.size.0, app.size.1);
    crate::draw::compose(&app, &mut g);
    assert!(!crate::tasks::grid_text(&g).contains('\u{10EEEE}'));
}

#[test]
fn remote_machines_get_no_thumbnails() {
    let (mut app, mut rxs, _dir, _) = thumb_app();
    app.config.ui.sidebar.preview_thumbnails = true;
    // The preview lives on machine 1, which isn't this host: its blobs are never fetched.
    app.machines[0].model.previews.clear();
    app.machines[1].model.previews = vec![preview("v4", 5173)];
    assert!(!app.machines[1].local);
    crate::preview_ui::before_draw(&mut app);
    assert!(commands(&mut rxs[1]).is_empty() && commands(&mut rxs[0]).is_empty());
}
