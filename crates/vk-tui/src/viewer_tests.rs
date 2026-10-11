//! Image viewer tests (spec 18 §11.1): the header and action line, every key of §5.2 (esc,
//! o, y, g, ←/→), the two-step `x` for attached images, the confirm step for captured
//! screenshots and `!` for review evidence, stepping across machines with the unseen ones first,
//! closing after the last removal, the WASM `o download` label, `open_images` with and without
//! new images, the toast naming the key, and the image on kitty and iTerm2 (local and fetched).

use super::*;
use crate::drafts::now_ms;
use crate::drafts::tests::{ch, commands, fleet, fleet_n, named, only, reply, reply_err, screen};
use crate::images::tests::{captured, load, rec};
use base64::Engine as _;
use serde_json::{Value, json};

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

fn cur(app: &App) -> (usize, String) {
    app.images_ui
        .viewer
        .as_ref()
        .expect("viewer open")
        .cur
        .clone()
}

fn two_attached(now: i64) -> Vec<Value> {
    let mut a = rec("S5", "agent", "p1", now - 120_000);
    a["caption"] = json!("login page");
    vec![a, rec("S3", "agent", "p1", now - 240_000)]
}

#[test]
fn open_images_shows_the_newest_unseen_and_esc_dismisses() {
    let (mut app, mut rxs) = fleet();
    // The whole action line fits; no tmux hint.
    app.sidebar = false;
    app.imgview.tmux = false;
    crate::push::on_events(&mut app, 0, vec![captured(1, "S5", "p1", "agent")], false);
    load(&mut app, &mut rxs[0], 0, two_attached(now_ms()), 2);
    assert_eq!(crate::gallery::badge(&app, 0, "p1"), Some(1));
    app.action("open_images", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    assert_eq!(cur(&app), (0, "S5".to_string()));
    let s = screen(&app);
    assert!(
        s.contains("s5 · login page · attached by claude · 2m ago"),
        "{s}"
    );
    assert!(
        s.contains(
            "esc dismiss · o open on this machine · x remove · ←/→ older/newer · g gallery · y copy path"
        ),
        "{s}"
    );
    // No graphics here: the hint names what still works.
    assert!(
        s.contains("no graphics — [o] open it on this machine"),
        "{s}"
    );
    // Seen once shown: the badge goes.
    assert_eq!(crate::gallery::badge(&app, 0, "p1"), None);
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.images_ui.viewer.is_none());
    assert!(crate::images::unseen(&app).is_empty());
}

#[test]
fn open_images_without_new_images_opens_the_gallery() {
    let (mut app, mut rxs) = fleet();
    load(&mut app, &mut rxs[0], 0, two_attached(now_ms()), 2);
    app.action("open_images", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Gallery)));
    let (_, p) = only(&commands(&mut rxs[0]), "screenshot.list");
    assert_eq!(p["limit"], 100);
    // The palette lists it.
    assert_eq!(crate::nav::describe("open_images"), "View new images");
}

#[test]
fn o_opens_y_copies_and_g_opens_the_gallery_on_the_image() {
    let (mut app, mut rxs) = fleet();
    load(&mut app, &mut rxs[0], 0, two_attached(now_ms()), 2);
    open(&mut app, (0, "S3".into()), false);
    app.on_key(ch('o'));
    assert_eq!(app.gallery.opened, vec!["/nonexistent/S3.png".to_string()]);
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('y'));
    let (data, _) = app.clipboard_sink.as_ref().unwrap().last().unwrap().clone();
    assert_eq!(String::from_utf8(data).unwrap(), "s3 /nonexistent/S3.png");
    app.on_key(ch('g'));
    assert!(app.images_ui.viewer.is_none());
    assert!(matches!(app.mode, Mode::Popup(Popup::Gallery)));
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    reply(
        &mut app,
        0,
        req,
        json!({"screenshots": two_attached(now_ms()), "total": 2}),
    );
    let g = app.gallery.view.as_ref().unwrap();
    assert_eq!(g.cur().unwrap().id, "S3");
}

#[test]
fn remote_o_transfers_unless_too_large() {
    let (mut app, mut rxs) = fleet_n(2);
    let now = now_ms();
    let mut big = rec("S7", "agent", "p1", now - 1_000);
    big["bytes"] = json!(9u64 << 20);
    load(
        &mut app,
        &mut rxs[1],
        1,
        vec![big, rec("S6", "agent", "p1", now - 2_000)],
        2,
    );
    open(&mut app, (1, "S7".into()), false);
    let s = screen(&app);
    assert!(!s.contains("o open on this machine"), "{s}");
    assert!(s.contains("too large to transfer (9 MiB)"), "{s}");
    assert!(s.contains("/nonexistent/S7.png"), "{s}");
    assert!(s.contains(" · m1 · "), "the machine label: {s}");
    app.on_key(ch('o'));
    assert!(commands(&mut rxs[1]).is_empty(), "no screenshot.open");
    // A smaller one is fetched with screenshot.open.
    app.on_key(named(NamedKey::Left));
    assert_eq!(cur(&app), (1, "S6".to_string()));
    app.on_key(ch('o'));
    only(&commands(&mut rxs[1]), "screenshot.open");
}

#[test]
fn an_attached_image_takes_a_second_x_then_the_next_shows_then_it_closes() {
    let (mut app, mut rxs) = fleet();
    load(&mut app, &mut rxs[0], 0, two_attached(now_ms()), 2);
    open(&mut app, (0, "S5".into()), false);
    app.on_key(ch('x'));
    assert!(commands(&mut rxs[0]).is_empty());
    let s = screen(&app);
    assert!(s.contains("x again to remove"), "{s}");
    // Another key: the removal is off and the key acts.
    app.on_key(ch('o'));
    assert_eq!(app.gallery.opened.len(), 1);
    assert!(!screen(&app).contains("x again to remove"));
    app.on_key(ch('x'));
    app.on_key(ch('x'));
    let (req, p) = only(&commands(&mut rxs[0]), "screenshot.delete");
    assert_eq!(p, json!({"id": "S5"}));
    reply(&mut app, 0, req, json!({"deleted": 1}));
    assert_eq!(cur(&app), (0, "S3".to_string()));
    app.on_key(ch('x'));
    app.on_key(ch('x'));
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.delete");
    reply(&mut app, 0, req, json!({"deleted": 1}));
    assert!(app.images_ui.viewer.is_none());
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn a_captured_screenshot_confirms_and_review_evidence_needs_bang() {
    let (mut app, mut rxs) = fleet();
    load(
        &mut app,
        &mut rxs[0],
        0,
        vec![rec("S4", "local_pane", "p2", now_ms() - 1_000)],
        1,
    );
    open(&mut app, (0, "S4".into()), false);
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Remove this screenshot? [y] remove"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    app.on_key(ch('x'));
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rxs[0]), "screenshot.delete");
    assert_eq!(p, json!({"id": "S4"}));
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "referenced_by_acceptance"}),
    );
    assert!(screen(&app).contains("[!] remove anyway"));
    app.on_key(ch('!'));
    let (req, p) = only(&commands(&mut rxs[0]), "screenshot.delete");
    assert_eq!(p, json!({"id": "S4", "force": true}));
    reply(&mut app, 0, req, json!({"deleted": 1}));
    assert!(app.images_ui.viewer.is_none());
}

#[test]
fn arrows_step_across_machines_with_the_unseen_first() {
    let (mut app, mut rxs) = fleet_n(2);
    let now = now_ms();
    load(
        &mut app,
        &mut rxs[0],
        0,
        vec![
            rec("B", "agent", "p1", now - 30_000),
            rec("A", "agent", "p1", now - 50_000),
        ],
        2,
    );
    load(
        &mut app,
        &mut rxs[1],
        1,
        vec![
            rec("D", "agent", "p1", now - 10_000),
            rec("C", "agent", "p1", now - 40_000),
        ],
        2,
    );
    // C (m1) and then B (m0) arrive after the connection.
    crate::push::on_events(&mut app, 1, vec![captured(1, "C", "p1", "agent")], false);
    crate::push::on_events(&mut app, 0, vec![captured(1, "B", "p1", "agent")], false);
    assert!(open_newest_unseen(&mut app));
    // The unseen ones by time (B, C), then the rest (D, A).
    assert_eq!(cur(&app), (0, "B".to_string()));
    app.on_key(named(NamedKey::Left));
    assert_eq!(cur(&app), (1, "C".to_string()));
    app.on_key(named(NamedKey::Left));
    assert_eq!(cur(&app), (1, "D".to_string()));
    app.on_key(named(NamedKey::Left));
    assert_eq!(cur(&app), (0, "A".to_string()));
    app.on_key(named(NamedKey::Left));
    assert_eq!(cur(&app), (0, "A".to_string()));
    assert!(screen(&app).contains("No older image"));
    app.on_key(named(NamedKey::Right));
    assert_eq!(cur(&app), (1, "D".to_string()));
    assert!(crate::images::unseen(&app).is_empty());
    // Opened on one image: plain time order.
    app.on_key(named(NamedKey::Escape));
    open(&mut app, (0, "A".into()), false);
    app.on_key(named(NamedKey::Right));
    assert_eq!(cur(&app), (1, "C".to_string()));
    app.on_key(named(NamedKey::Right));
    assert_eq!(cur(&app), (0, "B".to_string()));
    app.on_key(named(NamedKey::Right));
    assert_eq!(cur(&app), (1, "D".to_string()));
}

#[test]
fn the_wasm_label_is_download() {
    assert_eq!(open_text(true), "o download");
    assert_eq!(open_text(false), "o open on this machine");
    assert!(action_line(true, true).contains(" · o download · "));
    assert!(!action_line(false, false).contains(" o "));
}

#[test]
fn the_toast_names_the_key() {
    let (mut app, _rxs) = fleet();
    let key = app.keymap.binding_for("open_images").expect("bound");
    assert!(key.starts_with("prefix+"), "{key}");
    crate::push::on_events(&mut app, 0, vec![captured(1, "S5", "p1", "agent")], false);
    assert_eq!(
        app.toasts.last().unwrap().text,
        format!("📷 s5 attached by claude · {key} to view")
    );
    let ev = vk_proto::render::PushedEvent {
        seq: 2,
        kind: "screenshot.captured".into(),
        json: json!({"type": "screenshot.captured", "subject": {"screenshot": "S6", "pane": "p2"},
            "data": {"id": "S6", "handle": "s6", "environment": "local_pane", "url": "http://localhost:5173/"}})
        .to_string(),
    };
    crate::push::on_events(&mut app, 0, vec![ev], false);
    assert_eq!(
        app.toasts.last().unwrap().text,
        format!("📷 screenshot captured http://localhost:5173/ · {key} to view")
    );
}

#[test]
fn local_image_on_kitty_and_iterm() {
    for kitty in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s5.png");
        std::fs::write(&path, tiny_png()).unwrap();
        let (mut app, mut rxs) = fleet();
        caps(&mut app, kitty);
        app.imgview.gfx_dir = Some(dir.path().join("gfx"));
        let mut shot = rec("S5", "agent", "p1", now_ms() - 1_000);
        shot["path_on_machine"] = json!(path.to_string_lossy());
        load(&mut app, &mut rxs[0], 0, vec![shot], 1);
        open(&mut app, (0, "S5".into()), false);
        // Read from disk when it opens: nothing is fetched.
        assert!(commands(&mut rxs[0]).is_empty());
        assert!(app.imgview.image("0:S5").is_some());
        let p = placements(&app).expect("placement");
        assert_eq!((p.slot.as_str(), p.key.as_str()), ("viewer", "0:S5"));
        let out = String::from_utf8_lossy(&app.draw_bytes()).to_string();
        if kitty {
            assert!(out.contains("a=T"), "kitty transmits");
            assert!(screen(&app).contains('\u{10EEEE}'), "placeholders");
        } else {
            assert_eq!(out.matches("\x1b]1337;File=").count(), 1);
        }
        let s = screen(&app);
        assert!(
            !s.contains("loading image") && !s.contains("no graphics"),
            "{s}"
        );
        // Closing removes the placement.
        app.on_key(named(NamedKey::Escape));
        assert!(placements(&app).is_none());
    }
}

#[test]
fn remote_image_is_fetched_at_the_view_width_on_kitty_and_iterm() {
    for kitty in [true, false] {
        let (mut app, mut rxs) = fleet_n(2);
        caps(&mut app, kitty);
        let dir = tempfile::tempdir().unwrap();
        app.imgview.gfx_dir = Some(dir.path().join("gfx"));
        load(
            &mut app,
            &mut rxs[1],
            1,
            vec![rec("S7", "agent", "p1", now_ms() - 1_000)],
            1,
        );
        open(&mut app, (1, "S7".into()), false);
        let px = crate::image_view::thumb_px(&app, app.pane_area().w - 2);
        let (req, p) = only(&commands(&mut rxs[1]), "screenshot.get");
        assert_eq!(p, json!({"id": "S7", "inline": true, "thumb": px}));
        assert!(screen(&app).contains("loading image"));
        let b64 = base64::engine::general_purpose::STANDARD.encode(tiny_png());
        reply(&mut app, 1, req, json!({"id": "S7", "data_b64": b64}));
        assert!(app.imgview.image(&format!("1:S7@{px}")).is_some());
        let p = placements(&app).expect("placement");
        assert_eq!(p.slot, "viewer");
        let out = String::from_utf8_lossy(&app.draw_bytes()).to_string();
        if kitty {
            assert!(out.contains("a=T"), "kitty transmits");
        } else {
            assert_eq!(out.matches("\x1b]1337;File=").count(), 1);
        }
    }
}
