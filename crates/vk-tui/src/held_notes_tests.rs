//! Held review notes: the event raises a toast and an inbox item, the overlay shows the notes
//! with their author, Send and Discard call the release and discard methods, and a draft that is
//! settled elsewhere disappears.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, named, only, reply, screen};

fn held_event() -> Value {
    json!({"type": "browser.annotations_held", "subject": {},
           "data": {"draft": "D1", "preview": "v4", "count": 2,
                    "author": {"kind": "github", "id": "7", "label": "alice"}}})
}

fn drafts_answer() -> Value {
    json!({"drafts": [
        {"id": "D0", "state": "open", "notes": []},
        {"id": "D1", "state": "held", "author": {"kind": "github", "id": "7", "label": "alice"},
         "page_note": "Looks good\u{1b}[2J overall",
         "notes": [
            {"n": 1, "kind": "element", "selector": "button.buy", "note": "Make this bigger"},
            {"n": 2, "kind": "page", "note": "Colors"}]}
    ]})
}

#[test]
fn authors_read_as_logins_or_labels() {
    assert_eq!(
        author_text(&json!({"kind": "github", "id": "7", "login": "alice"})),
        "@alice"
    );
    assert_eq!(
        author_text(&json!({"kind": "github", "label": "alice"})),
        "@alice"
    );
    assert_eq!(
        author_text(&json!({"kind": "share", "label": "Kim's phone"})),
        "Kim's phone"
    );
    assert_eq!(author_text(&Value::Null), "a reviewer");
}

#[test]
fn a_held_event_raises_a_toast_and_an_inbox_item() {
    let (mut app, _rxs) = fleet();
    on_held(&mut app, 0, &held_event());
    assert_eq!(app.ux.held.items.len(), 1);
    let items = inbox_items(&app);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key.kind, "held_notes");
    assert_eq!(items[0].key.id, "D1");
    assert_eq!(items[0].title, "2 held notes from @alice");
    assert_eq!(items[0].subtitle, "design review of v4");
    // The same draft again replaces the entry.
    on_held(&mut app, 0, &held_event());
    assert_eq!(app.ux.held.items.len(), 1);
}

#[test]
fn the_pushed_event_is_routed_here() {
    let (mut app, _rxs) = fleet();
    let e = vk_proto::render::PushedEvent {
        seq: 1,
        kind: "browser.annotations_held".into(),
        json: held_event().to_string(),
    };
    crate::push::on_events(&mut app, 0, vec![e], false);
    assert_eq!(app.ux.held.items.len(), 1);
}

#[test]
fn send_releases_the_draft() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    on_held(&mut app, 0, &held_event());
    open(&mut app, 0, "D1");
    assert!(matches!(app.mode, Mode::Popup(Popup::HeldNotes)));
    let (req, p) = only(&commands(&mut rxs[0]), "browser.annotations");
    assert_eq!(p, json!({"preview": "v4"}));
    reply(&mut app, 0, req, drafts_answer());
    let s = screen(&app);
    assert!(s.contains("@alice left notes on v4"), "{s}");
    assert!(s.contains("1. [element] button.buy"), "{s}");
    assert!(s.contains("Make this bigger"), "{s}");
    assert!(s.contains("Page note: Looks good[2J overall"), "{s}");
    assert!(!s.contains('\u{1b}'));
    app.on_key(ch('s'));
    let (req, p) = only(&commands(&mut rxs[0]), "browser.annotations_release");
    assert_eq!(p, json!({"draft": "D1"}));
    reply(&mut app, 0, req, json!({"delivered": true}));
    assert!(app.ux.held.items.is_empty());
    assert!(app.ux.held.view.is_none());
    assert!(!matches!(app.mode, Mode::Popup(Popup::HeldNotes)));
}

#[test]
fn discard_asks_first() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    on_held(&mut app, 0, &held_event());
    open(&mut app, 0, "D1");
    commands(&mut rxs[0]);
    app.on_key(ch('d'));
    assert!(screen(&app).contains("Discard these notes?"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('d'));
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rxs[0]), "browser.annotations_discard");
    assert_eq!(p, json!({"draft": "D1"}));
    reply(&mut app, 0, req, json!({}));
    assert!(app.ux.held.items.is_empty());
}

#[test]
fn a_failed_send_stays_open_with_the_reason() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    on_held(&mut app, 0, &held_event());
    open(&mut app, 0, "D1");
    commands(&mut rxs[0]);
    app.on_key(named(NamedKey::Enter));
    let (req, _) = only(&commands(&mut rxs[0]), "browser.annotations_release");
    crate::drafts::tests::reply_err(&mut app, 0, req, "not_found", Value::Null);
    assert_eq!(app.ux.held.items.len(), 1);
    assert!(screen(&app).contains("✗ not_found!"));
}

#[test]
fn a_draft_settled_elsewhere_is_dropped() {
    let (mut app, _rxs) = fleet();
    on_held(&mut app, 0, &held_event());
    on_settled(
        &mut app,
        0,
        &json!({"data": {"draft": "D1", "state": "held"}}),
    );
    assert_eq!(app.ux.held.items.len(), 1);
    on_settled(
        &mut app,
        0,
        &json!({"data": {"draft": "D1", "state": "open"}}),
    );
    assert!(app.ux.held.items.is_empty());
}
