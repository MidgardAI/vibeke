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
    assert!(s.contains("Page note:"), "{s}");
    assert!(s.contains("Looks good[2J overall"), "{s}");
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

#[test]
fn connecting_restores_held_drafts_and_drops_settled_ones() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    // A held draft the client saw before; it was settled while the client was away.
    on_held(&mut app, 0, &held_event());
    assert_eq!(app.ux.held.items.len(), 1);
    on_connected(&mut app, 0);
    let (req, _) = only(&commands(&mut rxs[0]), "preview.list");
    reply(
        &mut app,
        0,
        req,
        json!({"previews": [{"id": "pv-4", "handle": "v4"}]}),
    );
    let (req, p) = only(&commands(&mut rxs[0]), "browser.annotations");
    assert_eq!(p, json!({"preview": "pv-4"}));
    reply(
        &mut app,
        0,
        req,
        json!({"drafts": [
            {"id": "D1", "state": "delivered", "notes": [{"n": 1}]},
            {"id": "D9", "state": "held", "notes": [{"n": 1}, {"n": 2}],
             "author": {"kind": "github", "id": "7", "label": "bob"}},
        ]}),
    );
    let items: Vec<_> = app
        .ux
        .held
        .items
        .iter()
        .map(|h| (h.draft.clone(), h.count))
        .collect();
    assert_eq!(items, vec![("D9".to_string(), 2)]);
    assert_eq!(app.ux.held.items[0].author, "@bob");
}

/// Open the overlay for D1 with its notes loaded and press Send; returns the release request.
fn send_held(
    app: &mut App,
    rxs: &mut [tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>],
) -> u64 {
    commands(&mut rxs[0]);
    on_held(app, 0, &held_event());
    open(app, 0, "D1");
    let (req, _) = only(&commands(&mut rxs[0]), "browser.annotations");
    reply(app, 0, req, drafts_answer());
    app.on_key(ch('s'));
    let (req, _) = only(&commands(&mut rxs[0]), "browser.annotations_release");
    req
}

#[test]
fn a_queued_release_keeps_the_overlay_and_shows_why() {
    let (mut app, mut rxs) = fleet();
    let req = send_held(&mut app, &mut rxs);
    reply(
        &mut app,
        0,
        req,
        json!({"state": "queued", "agent_pane": "p3", "text": "x",
               "draft": {"id": "D1", "state": "queued",
                         "detail": "waiting: someone is typing in that pane"}}),
    );
    assert!(matches!(app.mode, Mode::Popup(Popup::HeldNotes)));
    assert!(app.ux.held.items.is_empty(), "no longer held");
    assert!(commands(&mut rxs[0]).is_empty(), "the reply had the detail");
    let s = screen(&app);
    assert!(
        s.contains("Waiting for the agent: someone is typing in that pane"),
        "{s}"
    );
    assert!(s.contains("S send now"), "{s}");
    // Send and discard no longer apply.
    app.on_key(ch('s'));
    app.on_key(ch('d'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(!screen(&app).contains("Discard these notes?"));
}

#[test]
fn a_queued_release_without_detail_reads_the_draft() {
    let (mut app, mut rxs) = fleet();
    let req = send_held(&mut app, &mut rxs);
    reply(&mut app, 0, req, json!({"state": "queued"}));
    let (req, p) = only(&commands(&mut rxs[0]), "browser.annotations");
    assert_eq!(p, json!({"draft": "D1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"draft": {"id": "D1", "state": "queued",
                         "detail": "waiting: the agent is working; it gets the notes at its next step"}}),
    );
    let s = screen(&app);
    assert!(
        s.contains(
            "Waiting for the agent: the agent is working; it gets the notes at its next step"
        ),
        "{s}"
    );
}

#[test]
fn shift_s_sends_a_waiting_draft_now() {
    let (mut app, mut rxs) = fleet();
    let req = send_held(&mut app, &mut rxs);
    reply(
        &mut app,
        0,
        req,
        json!({"state": "queued", "draft": {"id": "D1", "detail": "waiting: busy"}}),
    );
    app.on_key(ch('S'));
    let (req, p) = only(&commands(&mut rxs[0]), "browser.annotations_send");
    assert_eq!(p, json!({"draft": "D1", "force": true}));
    reply(
        &mut app,
        0,
        req,
        json!({"state": "delivered", "via": "hook"}),
    );
    assert!(app.ux.held.view.is_none());
    assert!(!matches!(app.mode, Mode::Popup(Popup::HeldNotes)));
}

#[test]
fn send_now_is_off_while_the_draft_is_held() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    on_held(&mut app, 0, &held_event());
    open(&mut app, 0, "D1");
    commands(&mut rxs[0]);
    app.on_key(ch('S'));
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn a_failed_send_now_shows_the_reason() {
    let (mut app, mut rxs) = fleet();
    let req = send_held(&mut app, &mut rxs);
    reply(
        &mut app,
        0,
        req,
        json!({"state": "queued", "draft": {"detail": "waiting: busy"}}),
    );
    app.on_key(ch('S'));
    let (req, _) = only(&commands(&mut rxs[0]), "browser.annotations_send");
    crate::drafts::tests::reply_err(&mut app, 0, req, "run_ended", Value::Null);
    assert!(screen(&app).contains("✗ run_ended!"));
    assert!(app.ux.held.view.is_some());
}

#[test]
fn events_keep_a_waiting_overlay_until_the_draft_settles() {
    let (mut app, mut rxs) = fleet();
    let req = send_held(&mut app, &mut rxs);
    reply(
        &mut app,
        0,
        req,
        json!({"state": "queued", "draft": {"detail": "waiting: busy"}}),
    );
    on_settled(
        &mut app,
        0,
        &json!({"data": {"draft": "D1", "state": "queued", "detail": "waiting: input box has text"}}),
    );
    assert!(screen(&app).contains("Waiting for the agent: input box has text"));
    on_settled(
        &mut app,
        0,
        &json!({"data": {"draft": "D1", "state": "delivered"}}),
    );
    assert!(app.ux.held.view.is_none());
}

#[test]
fn wait_details_lose_the_prefix_and_control_characters() {
    assert_eq!(detail_text("waiting: a\u{1b}[2Jb"), "a[2Jb");
    assert_eq!(detail_text("plain"), "plain");
}
