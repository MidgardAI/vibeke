//! `pane.scroll_requested`: the focused pane's view moves into copy mode at the offset (history
//! loaded first when needed), offset 0 returns to the live screen, other clients' requests are
//! ignored, and requests for unfocused panes wait until the pane is focused.

use super::*;
use crate::app::Popup;
use crate::drafts::tests::{fleet, named, screen};
use serde_json::json;
use vk_proto::input::NamedKey;
use vk_proto::render::{ClientFrame, PushedEvent, Row, ServerFrame, Span};

fn row(s: &str) -> Row {
    Row {
        spans: vec![Span {
            style: Default::default(),
            text: s.into(),
            cols: s.chars().count() as u16,
        }],
        ..Default::default()
    }
}

fn scroll_event(app: &mut App, pane: &str, offset: u64, client: Value) {
    let v = json!({"seq": 3, "type": "pane.scroll_requested", "subject": {"pane": pane},
                   "data": {"offset": offset, "total": 100, "client": client}});
    app.on_frame(
        0,
        ServerFrame::Events {
            events: vec![PushedEvent {
                seq: 3,
                kind: "pane.scroll_requested".into(),
                json: v.to_string(),
            }],
            lagged: false,
        },
    );
}

/// The history requests sent: (req, start, count).
fn fetches(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>) -> Vec<(u64, u32, u32)> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        if let ClientFrame::FetchHistory {
            req, start, count, ..
        } = f
        {
            v.push((req, start, count));
        }
    }
    v
}

fn history(app: &mut App, req: u64, start: u32, total: u32, n: usize) {
    let lines = (0..n).map(|i| row(&format!("old {i}"))).collect();
    app.on_frame(
        0,
        ServerFrame::History {
            pane: "p1".into(),
            req,
            start,
            total,
            lines,
        },
    );
}

#[test]
fn focused_pane_scrolls_into_copy_mode_once_history_is_in() {
    let (mut app, mut rx) = fleet();
    app.machines[0].panes.get_mut("p1").unwrap().lines = vec![row("live 0"), row("live 1")];
    scroll_event(&mut app, "p1", 5, Value::Null);
    let Mode::Copy(cm) = &app.mode else {
        panic!("copy mode expected, got {:?}", app.mode)
    };
    assert_eq!(cm.pane, "p1");
    // Size first, then the rows; the offset applies once they are in.
    let f = fetches(&mut rx[0]);
    let (req, _, count) = *f.last().unwrap();
    assert_eq!(count, 0);
    history(&mut app, req, 0, 10, 0);
    let (req, start, count) = *fetches(&mut rx[0]).last().unwrap();
    assert_eq!((start, count), (0, 10));
    history(&mut app, req, 0, 10, 10);
    let Mode::Copy(cm) = &app.mode else {
        panic!("copy mode")
    };
    // Top of the view 5 rows above the live screen.
    assert_eq!(cm.scroll_offset().0, 5);
    assert!(screen(&app).contains("old 5"), "{}", screen(&app));
    // A later request moves the open view; offset 0 returns to the live screen.
    scroll_event(&mut app, "p1", 2, json!("tui-test"));
    let Mode::Copy(cm) = &app.mode else {
        panic!("copy mode")
    };
    assert_eq!(cm.scroll_offset().0, 2);
    scroll_event(&mut app, "p1", 0, Value::Null);
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn other_clients_requests_are_ignored() {
    let (mut app, _rx) = fleet();
    scroll_event(&mut app, "p1", 5, json!("tui-someone-else"));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.ux.scroll.pending.is_empty());
}

#[test]
fn unfocused_pane_waits_until_focused_and_popups_hold_it() {
    let (mut app, mut rx) = fleet();
    scroll_event(&mut app, "p2", 4, Value::Null);
    assert!(matches!(app.mode, Mode::Normal), "focus never moves");
    assert_eq!(app.ux.scroll.pending.get(&(0, "p2".into())), Some(&4));
    // A newer request replaces it; offset 0 drops it.
    scroll_event(&mut app, "p2", 0, Value::Null);
    assert!(app.ux.scroll.pending.is_empty());
    scroll_event(&mut app, "p2", 7, Value::Null);
    // While a popup owns the keyboard, even the focused pane waits.
    app.mode = Mode::Popup(Popup::Message {
        title: "t".into(),
        body: String::new(),
    });
    scroll_event(&mut app, "p1", 3, Value::Null);
    assert!(matches!(app.mode, Mode::Popup(Popup::Message { .. })));
    assert_eq!(app.ux.scroll.pending.get(&(0, "p1".into())), Some(&3));
    app.on_key(named(NamedKey::Escape));
    app.on_tick();
    assert!(matches!(app.mode, Mode::Copy(_)), "applied after the popup");
    assert!(!app.ux.scroll.pending.contains_key(&(0, "p1".into())));
    app.mode = Mode::Normal;
    fetches(&mut rx[0]);
    // Focusing p2 applies its request.
    app.focus_pane(0, "p2");
    app.on_tick();
    let Mode::Copy(cm) = &app.mode else {
        panic!("copy mode on p2")
    };
    assert_eq!(cm.pane, "p2");
    assert!(app.ux.scroll.pending.is_empty());
}
