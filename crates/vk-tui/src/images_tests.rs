//! Images section and unseen state tests (spec 18 §11.1): placement after the needs-you
//! section, the height cap and the order, unseen and seen styles, the header count, per-machine
//! `+N more` from `total`, `ui.images.sidebar = false`, the `/` filter, image targets winning
//! over pane actions, the selection after a removal, the 📷N badge click; images from before
//! the connection count as seen, reconnects keep the unseen set, out-of-order replies are
//! dropped, and deletions by another client or retention (`data.ids`) clear everything.

use super::*;
use crate::app::{Popup, test_interaction};
use crate::drafts::now_ms;
use crate::drafts::tests::{ch, commands, fleet, fleet_n, named, only, reply, reply_err, screen};
use crossterm::event::{KeyModifiers, MouseButton as CtButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::{ClientFrame, PushedEvent};

/// A screenshot record: `agent` images carry a caption, captured ones a page title.
pub(crate) fn rec(id: &str, kind: &str, pane: &str, ts: i64) -> Value {
    let handle = id.to_lowercase();
    let caption = if kind == "agent" {
        json!(format!("{handle} caption"))
    } else {
        Value::Null
    };
    let title = if kind == "agent" { "" } else { "Login" };
    json!({
        "id": id, "handle": handle, "environment": {"kind": kind}, "pane": pane,
        "created_at_ms": ts, "caption": caption, "title": title,
        "url": if kind == "agent" { "" } else { "http://localhost:5173/login" },
        "label": "your browser pane · profile devbox", "binding": "bound",
        "path_on_machine": format!("/nonexistent/{id}.png"), "exists": true,
        "width": 100, "height": 50, "bytes": 1000
    })
}

/// Connect machine `mi` and answer its recent-images request with `shots` (`total` on the
/// server).
pub(crate) fn load(
    app: &mut App,
    rx: &mut UnboundedReceiver<ClientFrame>,
    mi: usize,
    shots: Vec<Value>,
    total: u64,
) {
    on_connected(app, mi);
    tick(app);
    let (req, p) = only(&commands(rx), "screenshot.list");
    assert_eq!(p, json!({"limit": app.config.ui.images.recent}));
    let n = shots.len();
    reply(
        app,
        mi,
        req,
        json!({"screenshots": shots, "count": n, "total": total}),
    );
}

pub(crate) fn captured(seq: i64, id: &str, pane: &str, env: &str) -> PushedEvent {
    PushedEvent {
        seq,
        kind: "screenshot.captured".into(),
        json: json!({"type": "screenshot.captured", "subject": {"screenshot": id, "pane": pane},
            "data": {"id": id, "handle": id.to_lowercase(), "environment": env,
                     "taken_by": "agent", "pane": pane}})
        .to_string(),
    }
}

pub(crate) fn deleted(seq: i64, ids: &[&str]) -> PushedEvent {
    PushedEvent {
        seq,
        kind: "screenshot.deleted".into(),
        json: json!({"type": "screenshot.deleted", "subject": {},
            "data": {"ids": ids, "reason": "retention"}})
        .to_string(),
    }
}

fn texts(app: &App) -> Vec<String> {
    crate::draw::sidebar_rows(app)
        .iter()
        .map(|r| r.segs.iter().map(|(s, _)| s.as_str()).collect())
        .collect()
}

fn pos(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|r| r.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not in {rows:#?}"))
}

/// The navigate-mode index of the image row `(mi, id)`.
fn nav_index(app: &App, mi: usize, id: &str) -> usize {
    crate::draw::sidebar_rows(app)
        .iter()
        .filter(|r| r.selectable())
        .position(|r| r.image == Some((mi, id.to_string())))
        .expect("image row")
}

fn three(now: i64) -> Vec<Value> {
    vec![
        rec("S5", "agent", "p1", now - 120_000),
        rec("S4", "local_pane", "p2", now - 180_000),
        rec("S3", "agent", "p1", now - 240_000),
    ]
}

#[test]
fn section_follows_needs_you_with_order_styles_and_count() {
    let (mut app, mut rxs) = fleet();
    app.machines[0]
        .model
        .interactions
        .push(test_interaction("i1", "p1", "run tests?", 1));
    let now = now_ms();
    // S3 is attached after the connection (unseen); S5 and S4 existed before.
    crate::push::on_events(&mut app, 0, vec![captured(1, "S3", "p1", "agent")], false);
    load(&mut app, &mut rxs[0], 0, three(now), 3);
    let rows = texts(&app);
    let need = pos(&rows, "─ needs you ─");
    let head = pos(&rows, "─ images · 1 new ─");
    // Header, the agent row, the blank line: then the images.
    assert_eq!(head, need + 3, "{rows:#?}");
    assert!(head < pos(&rows, "api"), "before the workspaces: {rows:#?}");
    // Unseen first, then newest first.
    let s3 = pos(&rows, "s3 s3 caption");
    let s5 = pos(&rows, "s5 s5 caption");
    let s4 = pos(&rows, "s4 Login");
    assert!(s3 < s5 && s5 < s4, "{rows:#?}");
    assert_eq!(rows[s5], " 📷 s5 s5 caption · claude · 2m");
    // The run of the shell pane is gone: the label only.
    assert_eq!(rows[s4], " 📷 s4 Login · 3m");
    let side = crate::draw::sidebar_rows(&app);
    let t = app.theme;
    assert_eq!(side[s3].segs[0].1, t.bold(t.accent), "unseen: bright");
    assert_eq!(side[s5].segs[0].1, t.dim(), "seen: dimmed");
    // Looking at it: the count goes, the row dims.
    mark_seen(&mut app, 0, "S3");
    let rows = texts(&app);
    assert!(rows.iter().any(|r| r == "─ images ─"), "{rows:#?}");
    assert_eq!(crate::draw::sidebar_rows(&app)[s3].segs[0].1, t.dim());
}

#[test]
fn height_cap_and_more_per_machine() {
    let (mut app, mut rxs) = fleet_n(2);
    let now = now_ms();
    load(
        &mut app,
        &mut rxs[0],
        0,
        vec![
            rec("A1", "agent", "p1", now - 10_000),
            rec("A2", "agent", "p1", now - 50_000),
            rec("A3", "agent", "p1", now - 60_000),
        ],
        5,
    );
    load(
        &mut app,
        &mut rxs[1],
        1,
        vec![
            rec("B1", "agent", "p1", now - 20_000),
            rec("B2", "agent", "p1", now - 30_000),
        ],
        12,
    );
    // 13 rows: a third is 4, the header included.
    app.size.1 = 13;
    let rows = texts(&app);
    let images: Vec<&String> = rows.iter().filter(|r| r.starts_with(" 📷")).collect();
    assert_eq!(images.len(), 3, "{rows:#?}");
    assert!(images[0].contains("a1") && images[1].contains("b1") && images[2].contains("b2"));
    // A remote machine's rows carry its label.
    assert!(images[1].contains(" · m1 · "), "{}", images[1]);
    assert!(!images[0].contains("m0"), "{}", images[0]);
    // `+N more` per machine from the server's total.
    assert!(rows.iter().any(|r| r == "  +4 more on m0"), "{rows:#?}");
    assert!(rows.iter().any(|r| r == "  +10 more on m1"), "{rows:#?}");
    // At most `ui.images.recent` rows when there is room.
    app.size.1 = 60;
    app.config.ui.images.recent = 2;
    let n = texts(&app).iter().filter(|r| r.starts_with(" 📷")).count();
    assert_eq!(n, 2);
    // enter on `+10 more on m1` opens that machine's gallery.
    let sel = crate::draw::sidebar_rows(&app)
        .iter()
        .filter(|r| r.selectable())
        .position(|r| r.image_more == Some(1))
        .unwrap();
    app.mode = Mode::Navigate { sel };
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Popup(Popup::Gallery)));
    assert_eq!(app.gallery.view.as_ref().unwrap().machine, 1);
    let (_, p) = only(&commands(&mut rxs[1]), "screenshot.list");
    assert_eq!(p["limit"], 100);
}

#[test]
fn hidden_without_images_or_when_off() {
    let (mut app, mut rxs) = fleet();
    assert!(!texts(&app).iter().any(|r| r.contains("─ images")));
    load(&mut app, &mut rxs[0], 0, three(now_ms()), 3);
    assert!(texts(&app).iter().any(|r| r.contains("─ images")));
    app.config.ui.images.sidebar = false;
    let rows = texts(&app);
    assert!(
        !rows
            .iter()
            .any(|r| r.contains("─ images") || r.contains("📷 s"))
    );
}

#[test]
fn the_filter_matches_image_rows() {
    let (mut app, mut rxs) = fleet();
    load(&mut app, &mut rxs[0], 0, three(now_ms()), 3);
    app.mode = Mode::Navigate { sel: 0 };
    app.ux.nav.filter = Some("s4 login".into());
    let rows = crate::draw::sidebar_rows(&app);
    assert_eq!(rows.len(), 1, "{:?}", texts(&app));
    assert_eq!(rows[0].image, Some((0, "S4".to_string())));
}

#[test]
fn image_rows_never_act_on_a_pane() {
    let (mut app, mut rxs) = fleet();
    load(&mut app, &mut rxs[0], 0, three(now_ms()), 3);
    let sel = nav_index(&app, 0, "S5");
    // Pane-only keys are swallowed on an image row: no peek, no close-pane dialog.
    app.mode = Mode::Navigate { sel };
    app.on_key(ch(' '));
    assert!(matches!(app.mode, Mode::Navigate { sel: s } if s == sel));
    app.on_key(ch('u'));
    assert!(commands(&mut rxs[0]).is_empty(), "no pane.mark_unread");
    // enter opens the viewer on that image.
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    assert_eq!(
        app.images_ui.viewer.as_ref().unwrap().cur,
        (0, "S5".to_string())
    );
    assert_eq!(app.m().focus.pane.as_deref(), Some("p1"));
}

#[test]
fn x_removes_and_the_selection_moves_to_the_next_image() {
    let (mut app, mut rxs) = fleet();
    load(&mut app, &mut rxs[0], 0, three(now_ms()), 3);
    let sel = nav_index(&app, 0, "S5");
    app.mode = Mode::Navigate { sel };
    // An attached image: a second x, shown on the row.
    app.on_key(ch('x'));
    assert!(!matches!(app.mode, Mode::Popup(Popup::Confirm { .. })));
    assert!(texts(&app).iter().any(|r| r.contains("x again to remove")));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('x'));
    let (req, p) = only(&commands(&mut rxs[0]), "screenshot.delete");
    assert_eq!(p, json!({"id": "S5"}));
    reply(&mut app, 0, req, json!({"deleted": 1}));
    assert!(!texts(&app).iter().any(|r| r.contains("s5 ")));
    let row = |app: &App| {
        let Mode::Navigate { sel } = app.mode else {
            panic!("navigate mode")
        };
        crate::draw::sidebar_rows(app)
            .into_iter()
            .filter(|r| r.selectable())
            .nth(sel)
            .and_then(|r| r.image)
    };
    assert_eq!(row(&app), Some((0, "S4".to_string())));
    // A captured screenshot: y confirms; one kept for a review acceptance needs !.
    app.on_key(ch('x'));
    assert!(texts(&app).iter().any(|r| r.contains("remove? y")));
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
    app.on_key(ch('!'));
    let (req, p) = only(&commands(&mut rxs[0]), "screenshot.delete");
    assert_eq!(p, json!({"id": "S4", "force": true}));
    reply(&mut app, 0, req, json!({"deleted": 1}));
    assert_eq!(row(&app), Some((0, "S3".to_string())));
    // The last image: the section goes and the selection leaves it.
    app.on_key(ch('x'));
    app.on_key(ch('x'));
    let (req, _) = only(&commands(&mut rxs[0]), "screenshot.delete");
    reply(&mut app, 0, req, json!({"deleted": 1}));
    assert!(!texts(&app).iter().any(|r| r.contains("─ images")));
    assert_eq!(row(&app), None);
}

fn click(app: &mut App, column: u16, row: u16) {
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(CtButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn clicks_open_the_viewer_and_the_badge_opens_the_newest_unseen() {
    let (mut app, mut rxs) = fleet();
    app.sidebar = true;
    app.sidebar_w = 48;
    let now = now_ms();
    crate::push::on_events(
        &mut app,
        0,
        vec![
            captured(1, "S3", "p1", "agent"),
            captured(2, "S5", "p1", "agent"),
        ],
        false,
    );
    load(&mut app, &mut rxs[0], 0, three(now), 3);
    let sx = crate::chrome::sidebar_x(&app).unwrap();
    // A click on an image row: the viewer, never the pane.
    let rows = crate::draw::sidebar_rows(&app);
    let y = rows
        .iter()
        .position(|r| r.image == Some((0, "S4".to_string())))
        .unwrap() as u16
        + 1;
    click(&mut app, sx + 2, y);
    assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    assert_eq!(
        app.images_ui.viewer.as_ref().unwrap().cur,
        (0, "S4".to_string())
    );
    app.on_key(named(NamedKey::Escape));
    // The agent row's 📷2 badge: the newest unseen image of that agent.
    let rows = crate::draw::sidebar_rows(&app);
    let (i, r) = rows
        .iter()
        .enumerate()
        .find(|(_, r)| {
            r.target == Some((0, "p1".to_string()))
                && r.segs.iter().any(|(s, _)| s.starts_with(" 📷"))
        })
        .expect("agent row with a badge");
    let mut x = 0u16;
    for (s, _) in &r.segs {
        if s.starts_with(" 📷") {
            break;
        }
        x += unicode_width::UnicodeWidthStr::width(s.as_str()) as u16;
    }
    click(&mut app, sx + x + 2, i as u16 + 1);
    assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    assert_eq!(
        app.images_ui.viewer.as_ref().unwrap().cur,
        (0, "S5".to_string())
    );
    assert_eq!(crate::gallery::badge(&app, 0, "p1"), Some(1));
}

#[test]
fn images_from_before_the_connection_are_seen() {
    let (mut app, mut rxs) = fleet();
    load(&mut app, &mut rxs[0], 0, three(now_ms()), 3);
    assert!(unseen(&app).is_empty());
    assert_eq!(crate::gallery::badge(&app, 0, "p1"), None);
    assert!(texts(&app).iter().any(|r| r == "─ images ─"));
    crate::push::on_events(&mut app, 0, vec![captured(1, "S6", "p1", "agent")], false);
    assert_eq!(unseen(&app), vec![(0, "S6".to_string())]);
    assert_eq!(crate::gallery::badge(&app, 0, "p1"), Some(1));
    // The recent list is asked for again at the next tick (once for several events).
    crate::push::on_events(&mut app, 0, vec![captured(2, "S7", "p1", "agent")], false);
    tick(&mut app);
    only(&commands(&mut rxs[0]), "screenshot.list");
}

#[test]
fn a_reconnect_keeps_the_unseen_images() {
    let (mut app, mut rxs) = fleet();
    crate::push::on_events(&mut app, 0, vec![captured(1, "S6", "p1", "agent")], false);
    app.on_disconnected(0);
    on_connected(&mut app, 0);
    tick(&mut app);
    only(&commands(&mut rxs[0]), "screenshot.list");
    assert_eq!(unseen(&app), vec![(0, "S6".to_string())]);
    crate::push::on_events(&mut app, 0, vec![captured(2, "S7", "p2", "agent")], false);
    assert_eq!(unseen(&app).len(), 2);
}

#[test]
fn an_older_reply_is_dropped() {
    let (mut app, mut rxs) = fleet();
    let now = now_ms();
    on_connected(&mut app, 0);
    tick(&mut app);
    let (old, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    crate::push::on_events(&mut app, 0, vec![captured(1, "S6", "p1", "agent")], false);
    tick(&mut app);
    let (new, _) = only(&commands(&mut rxs[0]), "screenshot.list");
    reply(
        &mut app,
        0,
        new,
        json!({"screenshots": [rec("S6", "agent", "p1", now), rec("S5", "agent", "p1", now - 1)], "total": 2}),
    );
    reply(
        &mut app,
        0,
        old,
        json!({"screenshots": [rec("S5", "agent", "p1", now - 1)], "total": 1}),
    );
    let r = &app.images_ui.recent[&0];
    assert_eq!(r.shots.len(), 2);
    assert_eq!(r.total, 2);
}

#[test]
fn deletions_by_another_client_or_retention_clear_everything() {
    let (mut app, mut rxs) = fleet();
    crate::push::on_events(
        &mut app,
        0,
        vec![
            captured(1, "S3", "p1", "agent"),
            captured(2, "S5", "p1", "agent"),
        ],
        false,
    );
    load(&mut app, &mut rxs[0], 0, three(now_ms()), 3);
    app.imgview.put("0:S5".into(), vec![1, 2, 3]);
    // A removal of S4 waits.
    ask_remove(&mut app, 0, "S4".into());
    crate::push::on_events(&mut app, 0, vec![deleted(3, &["S5", "S4"])], false);
    assert_eq!(unseen(&app), vec![(0, "S3".to_string())]);
    let r = &app.images_ui.recent[&0];
    assert_eq!(r.shots.len(), 1);
    assert_eq!(r.total, 1);
    assert!(app.imgview.image("0:S5").is_none());
    assert!(app.images_ui.confirm.is_none());
    // The viewer on the last image closes when another client deletes it.
    crate::viewer::open(&mut app, (0, "S3".into()), false);
    assert!(matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    crate::push::on_events(&mut app, 0, vec![deleted(4, &["S3"])], false);
    assert!(app.images_ui.viewer.is_none());
    assert!(matches!(app.mode, Mode::Normal));
    assert!(unseen(&app).is_empty());
    assert!(!texts(&app).iter().any(|r| r.contains("─ images")));
    // The gallery's pending delete of a deleted screenshot is off.
    crate::gallery::open(&mut app, 0, Scope::All, View::List, false);
    if let Some(g) = &mut app.gallery.view {
        g.confirm_delete = Some("S1".into());
    }
    crate::push::on_events(&mut app, 0, vec![deleted(5, &["S1"])], false);
    assert!(app.gallery.view.as_ref().unwrap().confirm_delete.is_none());
    assert!(!screen(&app).contains("Delete this screenshot?"));
    // The recent list is asked for again.
    tick(&mut app);
    assert!(
        commands(&mut rxs[0])
            .iter()
            .any(|c| c.1 == "screenshot.list" && c.2["limit"] == 5)
    );
}

#[test]
fn digits_count_panes_and_never_the_image_rows() {
    let (mut app, mut rxs) = fleet();
    let before: Vec<_> = crate::draw::sidebar_digit_rows(&app)
        .into_iter()
        .map(|r| r.target.map(|t| t.1))
        .collect();
    load(&mut app, &mut rxs[0], 0, three(now_ms()), 3);
    let after: Vec<_> = crate::draw::sidebar_digit_rows(&app)
        .into_iter()
        .map(|r| r.target.map(|t| t.1))
        .collect();
    assert_eq!(before, after, "pictures do not shift the numbers");
    // `1` with an image row selected still goes to the first pane row, not the viewer.
    let sel = nav_index(&app, 0, "S5");
    app.mode = Mode::Navigate { sel };
    app.on_key(ch('1'));
    assert!(!matches!(app.mode, Mode::Popup(Popup::ImageViewer)));
    if let Some(Some(first)) = before.first() {
        assert_eq!(app.m().focus.pane.as_ref(), Some(first));
    }
}

#[test]
fn unseen_is_capped_and_browser_automation_is_not_new() {
    let (mut app, _rxs) = fleet();
    // An agent's headless-browser capture shows in lists but is not a "new image".
    crate::push::on_events(
        &mut app,
        0,
        vec![captured(1, "S1", "p1", "remote_headless")],
        false,
    );
    assert!(app.images_ui.unseen.is_empty());
    // An attached image counts.
    crate::push::on_events(&mut app, 0, vec![captured(2, "S2", "p1", "agent")], false);
    assert_eq!(app.images_ui.unseen.len(), 1);
    // A flood keeps the newest MAX_UNSEEN.
    for i in 0..(MAX_UNSEEN + 20) {
        let id = format!("X{i}");
        crate::push::on_events(
            &mut app,
            0,
            vec![captured(10 + i as i64, &id, "p1", "agent")],
            false,
        );
    }
    assert_eq!(app.images_ui.unseen.len(), MAX_UNSEEN);
    let last = format!("X{}", MAX_UNSEEN + 19);
    assert_eq!(app.images_ui.unseen.last().unwrap().id, last);
    assert!(!is_unseen(&app, 0, "S2"), "the oldest fell off");
}
