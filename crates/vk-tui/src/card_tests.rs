//! Attach pop-up tests (spec 18 §11.1): the card appears for an agent-attached image in the
//! focused pane (kitty and iTerm2, local and fetched), and not for another pane, another
//! machine, `taken_by = user`, a captured screenshot, a plugin surface, an armed prefix, an
//! unfocused terminal, `ui.images.popup = false` or focus that moved before the bytes arrived.
//! Keys and pastes reach the pane and close it (releases, repeats, modifier-only presses and the
//! prefix do not); esc closes it without reaching the pane; untouched it hides after a while; `prefix+alt+i` and a click open the viewer; typing leaves the image unseen;
//! three images make one card; the corner away from the cursor; the iTerm2 repaint on close.

use super::*;
use crate::app::{App, Mode, PaneBuf, Popup};
use crate::drafts::now_ms;
use crate::drafts::tests::{ch, commands, ctl, fleet_n, named, only, reply, screen};
use crate::images::tests::{deleted, load, rec};
use base64::Engine as _;
use crossterm::event::{KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::Mods;
use vk_proto::render::{ClientFrame, PushedEvent};

type Rx = UnboundedReceiver<ClientFrame>;

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

/// `fleet_n` with room for the whole card: no sidebar, a wide screen.
fn setup(n: usize) -> (App, Vec<Rx>) {
    let (mut app, rxs) = fleet_n(n);
    app.sidebar = false;
    app.size = (200, 50);
    (app, rxs)
}

fn event(seq: i64, id: &str, pane: &str, env: &str, by: &str) -> PushedEvent {
    let handle = id.to_lowercase();
    PushedEvent {
        seq,
        kind: "screenshot.captured".into(),
        json: json!({"type": "screenshot.captured", "subject": {"screenshot": id, "pane": pane},
            "data": {"id": id, "handle": handle, "environment": env, "taken_by": by,
                     "pane": pane, "caption": format!("{handle} caption")}})
        .to_string(),
    }
}

/// An agent attaches image `id` in `pane` of machine `mi`.
fn attach(app: &mut App, mi: usize, seq: i64, id: &str, pane: &str) {
    crate::push::on_events(app, mi, vec![event(seq, id, pane, "agent", "agent")], false);
}

/// Key and paste frames sent to panes (and mouse events).
fn inputs(rx: &mut Rx) -> Vec<String> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        match f {
            ClientFrame::Key { pane, key, .. } => v.push(format!("key {pane} {:?}", key.key)),
            ClientFrame::Paste { pane, text, .. } => v.push(format!("paste {pane} {text}")),
            ClientFrame::Mouse { pane, .. } => v.push(format!("mouse {pane}")),
            _ => {}
        }
    }
    v
}

fn osc1337(out: &[u8]) -> usize {
    String::from_utf8_lossy(out)
        .matches("\x1b]1337;File=")
        .count()
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

fn b64(d: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(d)
}

/// A local image S5 attached in the focused pane p1, its PNG on disk; the card shows once the
/// recent list gives its path.
fn local_card(kitty: bool) -> (App, Vec<Rx>, tempfile::TempDir) {
    let (mut app, mut rxs) = setup(1);
    caps(&mut app, kitty);
    let dir = tempfile::tempdir().unwrap();
    app.imgview.gfx_dir = Some(dir.path().join("gfx"));
    let path = dir.path().join("s5.png");
    std::fs::write(&path, tiny_png()).unwrap();
    attach(&mut app, 0, 1, "S5", "p1");
    assert!(app.images_ui.card.is_some());
    assert!(
        showing(&app).is_none(),
        "waiting for the path and the bytes"
    );
    let mut r = rec("S5", "agent", "p1", now_ms() - 1_000);
    r["path_on_machine"] = json!(path.to_string_lossy().to_string());
    load(&mut app, &mut rxs[0], 0, vec![r], 1);
    assert!(showing(&app).is_some());
    (app, rxs, dir)
}

#[test]
fn appears_for_an_agent_attached_image_in_the_focused_pane() {
    for kitty in [true, false] {
        let (mut app, _rxs, _dir) = local_card(kitty);
        let out = app.draw_bytes();
        if kitty {
            let s = String::from_utf8_lossy(&out);
            assert!(s.contains("a=T,i="), "transmitted: {s}");
        } else {
            assert_eq!(osc1337(&out), 1, "sent after the frame");
            assert_eq!(osc1337(&app.draw_bytes()), 0, "and only once");
        }
        let s = screen(&app);
        assert!(s.contains("attached by claude"), "{s}");
        assert!(s.contains("s5 · s5 caption"), "{s}");
        assert!(s.contains("prefix+alt+i open · esc close"), "{s}");
        // At most half the pane, in the bottom-right corner (the cursor is top-left).
        let p = placements(&app).unwrap();
        let r = pane_rect(&app, "p1").unwrap();
        assert_eq!(p.rect.x + p.rect.w + 1, r.x + r.w, "right edge");
        assert_eq!(p.rect.y + p.rect.h + 2, r.y + r.h, "bottom edge");
        assert!(p.rect.w <= r.w / 2 && p.rect.h <= r.h / 2);
    }
}

#[test]
fn it_sits_in_the_corner_away_from_the_cursor() {
    let (mut app, _rxs) = setup(1);
    attach(&mut app, 0, 1, "A", "p1");
    let r = pane_rect(&app, "p1").unwrap();
    let (_, l) = layout(&app).unwrap();
    assert_eq!(
        (l.outer.x + l.outer.w, l.outer.y + l.outer.h),
        (r.x + r.w, r.y + r.h),
        "bottom-right"
    );
    assert!(l.outer.w <= r.w / 2 && l.outer.h <= r.h / 2);
    app.machines[0].panes.get_mut("p1").unwrap().cursor.row = r.h - 1;
    let (_, l) = layout(&app).unwrap();
    assert_eq!(
        (l.outer.x + l.outer.w, l.outer.y),
        (r.x + r.w, r.y),
        "top-right"
    );
    app.machines[0].panes.get_mut("p1").unwrap().cursor.col = r.w - 1;
    let (_, l) = layout(&app).unwrap();
    assert_eq!((l.outer.x, l.outer.y), (r.x, r.y), "top-left");
    // Without graphics the card still shows the caption.
    let s = screen(&app);
    assert!(s.contains("a · a caption"), "{s}");
}

#[test]
fn no_card_for_other_panes_machines_people_or_captured_screenshots() {
    let (mut app, _rxs) = setup(2);
    crate::push::on_events(
        &mut app,
        0,
        vec![event(1, "A", "p2", "agent", "agent")],
        false,
    );
    assert!(app.images_ui.card.is_none(), "another pane");
    crate::push::on_events(
        &mut app,
        1,
        vec![event(2, "B", "p1", "agent", "agent")],
        false,
    );
    assert!(app.images_ui.card.is_none(), "another machine");
    crate::push::on_events(
        &mut app,
        0,
        vec![event(3, "C", "p1", "agent", "user")],
        false,
    );
    assert!(app.images_ui.card.is_none(), "a person attached it");
    crate::push::on_events(
        &mut app,
        0,
        vec![event(4, "D", "p1", "local_pane", "agent")],
        false,
    );
    assert!(app.images_ui.card.is_none(), "a captured screenshot");
    attach(&mut app, 0, 5, "E", "p1");
    assert!(showing(&app).is_some(), "the focused pane does get one");
}

#[test]
fn no_card_with_a_plugin_surface_an_armed_prefix_an_unfocused_terminal_or_the_setting_off() {
    let (mut app, _rxs) = setup(1);
    let mut p = crate::drafts::tests::pane("p9", "T1", "W1");
    p.created_by = "plugin-surface:popup:60%:10:acme.demo/picker".into();
    let m = &mut app.machines[0];
    m.model.panes.push(p);
    m.model.tabs[0]
        .floating
        .push(vk_proto::model::FloatingPane::centred("p9", 2));
    m.panes.insert("p9".into(), PaneBuf::blank());
    assert!(!crate::plugins::surfaces(&app).is_empty());
    attach(&mut app, 0, 1, "A", "p1");
    assert!(app.images_ui.card.is_none(), "a plugin surface");

    let (mut app, _rxs) = setup(1);
    app.on_key(ctl('b'));
    assert!(matches!(app.mode, Mode::Prefix(_)));
    attach(&mut app, 0, 1, "A", "p1");
    assert!(app.images_ui.card.is_none(), "an armed prefix");

    let (mut app, _rxs) = setup(1);
    app.caps.focus_events = true;
    app.host_focused = false;
    attach(&mut app, 0, 1, "A", "p1");
    assert!(app.images_ui.card.is_none(), "an unfocused host terminal");
    app.host_focused = true;
    attach(&mut app, 0, 2, "B", "p1");
    assert!(showing(&app).is_some());

    let (mut app, _rxs) = setup(1);
    app.config.ui.images.popup = false;
    attach(&mut app, 0, 1, "A", "p1");
    assert!(app.images_ui.card.is_none(), "ui.images.popup = false");
}

#[test]
fn a_remote_image_is_fetched_at_the_card_width_and_moved_focus_drops_it() {
    for kitty in [true, false] {
        let (mut app, mut rxs) = setup(2);
        caps(&mut app, kitty);
        let dir = tempfile::tempdir().unwrap();
        app.imgview.gfx_dir = Some(dir.path().join("gfx"));
        app.cur = 1;
        attach(&mut app, 1, 1, "R1", "p1");
        let (req, p) = only(&commands(&mut rxs[1]), "screenshot.get");
        assert_eq!(p["id"], "R1");
        assert_eq!(p["inline"], true);
        let px = crate::image_view::thumb_px(&app, max_image_cols(&app, "p1"));
        assert_eq!(p["thumb"], json!(px));
        assert!(showing(&app).is_none(), "waiting for the bytes");
        reply(&mut app, 1, req, json!({"data_b64": b64(&tiny_png())}));
        assert!(showing(&app).is_some());
        if !kitty {
            assert_eq!(osc1337(&app.draw_bytes()), 1);
        }
        // The focus moves before the next image's bytes arrive: no card.
        close(&mut app);
        attach(&mut app, 1, 2, "R2", "p1");
        let (req, _) = only(&commands(&mut rxs[1]), "screenshot.get");
        app.machines[1].focus.pane = Some("p2".into());
        reply(&mut app, 1, req, json!({"data_b64": b64(&tiny_png())}));
        assert!(app.images_ui.card.is_none());
    }
}

#[test]
fn typing_reaches_the_pane_closes_the_card_and_leaves_the_image_unseen() {
    for kitty in [true, false] {
        let (mut app, mut rxs, _dir) = local_card(kitty);
        app.draw_bytes();
        let p = placements(&app).unwrap();
        inputs(&mut rxs[0]);
        // Releases, repeats and modifier-only presses reach the pane but keep the card.
        let mut rel = ch('x');
        rel.kind = KeyKind::Release;
        app.on_key(rel);
        let mut rep = ch('x');
        rep.kind = KeyKind::Repeat;
        app.on_key(rep);
        app.on_key(named(NamedKey::LeftShift));
        assert!(showing(&app).is_some());
        inputs(&mut rxs[0]);
        app.on_key(ch('x'));
        assert!(app.images_ui.card.is_none());
        assert_eq!(inputs(&mut rxs[0]), vec!["key p1 Char('x')".to_string()]);
        assert!(crate::images::is_unseen(&app, 0, "S5"), "still unseen");
        if !kitty {
            // The cells under the image are repainted.
            let mut grid = Grid::new(app.size.0, app.size.1);
            crate::draw::compose(&app, &mut grid);
            crate::image_view::after_compose(&mut app, &grid);
            assert!(marked(&app, p.rect));
        }
    }
}

#[test]
fn every_key_and_a_paste_reach_the_pane_and_close_the_card() {
    let (mut app, mut rxs) = setup(1);
    let keys = [
        ch('x'),
        ch('o'),
        ch('g'),
        named(NamedKey::Left),
        named(NamedKey::Right),
        named(NamedKey::Up),
        named(NamedKey::Down),
    ];
    for (i, k) in keys.into_iter().enumerate() {
        attach(&mut app, 0, i as i64 + 1, &format!("S{i}"), "p1");
        assert!(showing(&app).is_some(), "{k:?}");
        let want = format!("key p1 {:?}", k.key);
        app.on_key(k);
        assert!(app.images_ui.card.is_none(), "{want}");
        assert_eq!(inputs(&mut rxs[0]), vec![want]);
    }
    attach(&mut app, 0, 50, "S50", "p1");
    app.on_paste("hello".into());
    assert!(app.images_ui.card.is_none());
    assert_eq!(inputs(&mut rxs[0]), vec!["paste p1 hello".to_string()]);
    assert!(crate::images::is_unseen(&app, 0, "S50"));
}

#[test]
fn prefix_alt_i_opens_the_card_image_in_the_viewer() {
    for kitty in [true, false] {
        let (mut app, mut rxs, _dir) = local_card(kitty);
        app.draw_bytes();
        inputs(&mut rxs[0]);
        app.on_key(ctl('b'));
        assert!(matches!(app.mode, Mode::Prefix(_)));
        assert!(showing(&app).is_some(), "the prefix key does not close it");
        app.on_key(KeyEvent::new(Key::Char('i'), Mods::ALT));
        assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
        let v = app.images_ui.viewer.as_ref().unwrap();
        assert_eq!(v.cur, (0, "S5".to_string()));
        assert!(app.images_ui.card.is_none());
        assert!(inputs(&mut rxs[0]).is_empty(), "nothing reached the pane");
        assert!(!crate::images::is_unseen(&app, 0, "S5"));
    }
}

#[test]
fn a_click_on_the_card_opens_the_viewer_and_other_mouse_events_pass() {
    let (mut app, mut rxs, _dir) = local_card(true);
    app.machines[0].panes.get_mut("p1").unwrap().modes.mouse = true;
    app.draw_bytes();
    inputs(&mut rxs[0]);
    let (_, l) = layout(&app).unwrap();
    let at = |kind: MouseEventKind| MouseEvent {
        kind,
        column: l.outer.x + 2,
        row: l.outer.y + 2,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(at(MouseEventKind::ScrollDown));
    assert!(showing(&app).is_some());
    assert_eq!(inputs(&mut rxs[0]), vec!["mouse p1".to_string()]);
    app.on_mouse(at(MouseEventKind::Down(CtButton::Left)));
    assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    assert!(
        inputs(&mut rxs[0]).is_empty(),
        "the click stays with the card"
    );
}

#[test]
fn three_images_make_one_card_with_a_count() {
    let (mut app, mut rxs) = setup(1);
    for (i, id) in ["S1", "S2", "S3"].iter().enumerate() {
        attach(&mut app, 0, i as i64 + 1, id, "p1");
    }
    let now = now_ms();
    load(
        &mut app,
        &mut rxs[0],
        0,
        vec![
            rec("S3", "agent", "p1", now - 1_000),
            rec("S2", "agent", "p1", now - 2_000),
            rec("S1", "agent", "p1", now - 3_000),
        ],
        3,
    );
    let c = showing(&app).unwrap();
    assert_eq!((c.id.as_str(), c.count), ("S3", 3));
    let s = screen(&app);
    assert!(s.contains("3 new images"), "{s}");
    assert!(s.contains("s3 · s3 caption"), "{s}");
    app.action("open_images", None);
    let v = app.images_ui.viewer.as_ref().unwrap();
    assert_eq!(v.cur, (0, "S3".to_string()));
    let ids: Vec<&str> = v.order.iter().map(|t| t.1.as_str()).collect();
    assert_eq!(ids, ["S3", "S2", "S1"]);
    app.on_key(named(NamedKey::Left));
    assert_eq!(
        app.images_ui.viewer.as_ref().unwrap().cur,
        (0, "S2".to_string())
    );
}

#[test]
fn it_closes_when_the_focus_moves_a_popup_opens_or_the_image_goes() {
    let (mut app, _rxs) = setup(1);
    attach(&mut app, 0, 1, "A", "p1");
    app.machines[0].focus.pane = Some("p2".into());
    app.draw_bytes();
    app.machines[0].focus.pane = Some("p1".into());
    assert!(app.images_ui.card.is_none(), "the focus moved");
    attach(&mut app, 0, 2, "B", "p1");
    app.action("screenshots", None);
    app.draw_bytes();
    assert!(app.images_ui.card.is_none(), "a popup opened");
    app.mode = Mode::Normal;
    attach(&mut app, 0, 3, "C", "p1");
    assert!(showing(&app).is_some());
    crate::push::on_events(&mut app, 0, vec![deleted(4, &["C"])], false);
    assert!(app.images_ui.card.is_none(), "the image was deleted");
}

#[test]
fn a_replacement_image_is_checked_again_when_its_bytes_arrive() {
    let (mut app, mut rxs) = setup(2);
    caps(&mut app, false);
    app.cur = 1;
    attach(&mut app, 1, 1, "R1", "p1");
    let (req, _) = only(&commands(&mut rxs[1]), "screenshot.get");
    reply(&mut app, 1, req, json!({"data_b64": b64(&tiny_png())}));
    assert!(showing(&app).is_some());
    // A second image while the first card shows; the terminal loses focus during its fetch.
    attach(&mut app, 1, 2, "R2", "p1");
    let (req, _) = only(&commands(&mut rxs[1]), "screenshot.get");
    app.caps.focus_events = true;
    app.host_focused = false;
    reply(&mut app, 1, req, json!({"data_b64": b64(&tiny_png())}));
    assert!(app.images_ui.card.is_none());
}

#[test]
fn esc_closes_the_card_without_reaching_the_pane() {
    let (mut app, mut rxs) = setup(1);
    attach(&mut app, 0, 1, "S1", "p1");
    assert!(showing(&app).is_some());
    inputs(&mut rxs[0]);
    app.on_key(named(NamedKey::Escape));
    assert!(app.images_ui.card.is_none());
    assert!(
        inputs(&mut rxs[0]).is_empty(),
        "esc did not interrupt the agent"
    );
    assert!(crate::images::is_unseen(&app, 0, "S1"));
    // Without a card, esc reaches the pane as usual.
    app.on_key(named(NamedKey::Escape));
    assert_eq!(
        inputs(&mut rxs[0]),
        vec![format!("key p1 {:?}", Key::Named(NamedKey::Escape))]
    );
}

#[test]
fn an_untouched_card_hides_itself() {
    let (mut app, _rxs) = setup(1);
    attach(&mut app, 0, 1, "S1", "p1");
    let shown_at = app.images_ui.card.as_ref().unwrap().shown_at.unwrap();
    assert_eq!(
        app.deadlines(Instant::now()).get("card.hide"),
        Some(shown_at + AUTO_HIDE)
    );
    app.draw_bytes();
    assert!(showing(&app).is_some());
    app.images_ui.card.as_mut().unwrap().shown_at = Some(Instant::now() - AUTO_HIDE);
    app.draw_bytes();
    assert!(app.images_ui.card.is_none());
    assert!(crate::images::is_unseen(&app, 0, "S1"));
}
