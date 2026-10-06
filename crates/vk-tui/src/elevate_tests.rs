//! Elevation approval view: requests from pushed events and `auth.list`, the chrome-only
//! notice, the view replacing the pane area (the requesting pane's content is not on screen),
//! the arm delay, explicit y/n through `auth.elevate.decide`, never auto-approving, scoped
//! clients, and errors.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, fleet_n, named, only, reply, reply_err, screen};
use vk_proto::render::{PushedEvent, ServerFrame};

fn event(app: &mut App, mi: usize, kind: &str, v: Value) {
    app.on_frame(
        mi,
        ServerFrame::Events {
            events: vec![PushedEvent {
                seq: 1,
                kind: kind.into(),
                json: v.to_string(),
            }],
            lagged: false,
        },
    );
}

fn requested(app: &mut App, mi: usize, id: &str, pane: &str, reason: &str) {
    event(
        app,
        mi,
        "auth.elevate_requested",
        json!({"seq": 1, "ts": now_ms(), "type": "auth.elevate_requested",
               "subject": {"pane": pane, "request": id}, "data": {"reason": reason}}),
    );
}

/// Open the view and let the arm delay pass.
fn open_armed(app: &mut App) {
    app.action("elevation_requests", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Elevate)));
    app.ux.elevate.view.as_mut().unwrap().opened_at = Instant::now() - ARM_DELAY * 2;
}

#[test]
fn request_shows_a_chrome_notice_and_never_opens_by_itself() {
    let (mut app, mut rx) = fleet();
    app.machines[0].panes.get_mut("p1").unwrap().lines = vec![row_of("PANE-CONTENT")];
    requested(&mut app, 0, "el-1", "p1", "deploy the staging stack");
    assert_eq!(app.ux.elevate.requests.len(), 1);
    // Nothing pops up and nothing is sent: only the notice in the tab bar.
    assert!(matches!(app.mode, Mode::Normal));
    assert!(commands(&mut rx[0]).is_empty());
    let s = screen(&app);
    let top = s.lines().next().unwrap();
    assert!(top.contains("asks for elevated access"), "{top}");
    assert!(top.contains("prefix+shift+e"), "{top}");
    assert!(s.contains("PANE-CONTENT"), "the pane is untouched: {s}");
    // Granted elsewhere (CLI): the notice goes.
    event(
        &mut app,
        0,
        "auth.elevate_granted",
        json!({"subject": {"pane": "p1", "request": "el-1"}, "data": {}}),
    );
    assert!(app.ux.elevate.requests.is_empty());
    assert!(!screen(&app).contains("asks for elevated access"));
}

fn row_of(s: &str) -> vk_proto::render::Row {
    vk_proto::render::Row {
        spans: vec![vk_proto::render::Span {
            style: Default::default(),
            text: s.into(),
            cols: s.chars().count() as u16,
        }],
        ..Default::default()
    }
}

#[test]
fn view_replaces_the_pane_area_and_shows_reason_pane_and_expiry() {
    let (mut app, _rx) = fleet();
    app.machines[0].panes.get_mut("p1").unwrap().lines = vec![row_of("PANE-CONTENT")];
    requested(
        &mut app,
        0,
        "el-1",
        "p1",
        "deploy\x1b[31m the staging stack",
    );
    open_armed(&mut app);
    let s = screen(&app);
    assert!(s.contains("drawn by Vibeke, not by any pane"), "{s}");
    assert!(
        s.contains("Pane w1:p1 (claude · api) asks for elevated access"),
        "{s}"
    );
    assert!(s.contains("written by the pane; unverified"), "{s}");
    // Control characters from the pane never reach the screen.
    assert!(s.contains("deploy[31m the staging stack"), "{s}");
    assert!(!s.contains('\x1b'));
    assert!(s.contains("Expiry: the request stays open until"), "{s}");
    assert!(s.contains("full API access for 10 minutes"), "{s}");
    assert!(s.contains("[y] approve   [n] deny"), "{s}");
    // The requesting pane's content is not on screen while the prompt is.
    assert!(!s.contains("PANE-CONTENT"), "{s}");
}

#[test]
fn keys_within_the_arm_delay_and_modified_keys_never_decide() {
    let (mut app, mut rx) = fleet();
    requested(&mut app, 0, "el-1", "p1", "");
    app.action("elevation_requests", None);
    // Typed for the pane a moment ago: ignored, the view stays.
    for c in "yyyn".chars() {
        app.on_key(ch(c));
    }
    assert!(matches!(app.mode, Mode::Popup(Popup::Elevate)));
    assert!(commands(&mut rx[0]).is_empty(), "never auto-approved");
    app.ux.elevate.view.as_mut().unwrap().opened_at = Instant::now() - ARM_DELAY * 2;
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
    assert!(commands(&mut rx[0]).is_empty());
    // Esc closes without deciding; the request stays.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(commands(&mut rx[0]).is_empty());
    assert_eq!(app.ux.elevate.requests.len(), 1);
    app.on_tick();
    assert!(app.ux.elevate.view.is_none());
}

#[test]
fn y_approves_through_auth_elevate_decide_and_n_denies() {
    let (mut app, mut rx) = fleet();
    requested(&mut app, 0, "el-1", "p1", "a");
    requested(&mut app, 0, "el-2", "p2", "b");
    open_armed(&mut app);
    assert!(screen(&app).contains("[j/k] select"));
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rx[0]), "auth.elevate.decide");
    assert_eq!(p, json!({"request": "el-1", "decision": "approve"}));
    // A second y while the first is in flight sends nothing.
    app.on_key(ch('y'));
    assert!(commands(&mut rx[0]).is_empty());
    let exp = now_ms() + 600_000;
    reply(
        &mut app,
        0,
        req,
        json!({"request": "el-1", "pane": "p1", "decision": "approved", "expires_at_ms": exp}),
    );
    assert_eq!(app.ux.elevate.requests.len(), 1);
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("approved elevation for w1:p1")
    );
    assert!(matches!(app.mode, Mode::Popup(Popup::Elevate)));
    app.on_key(ch('n'));
    let (req, p) = only(&commands(&mut rx[0]), "auth.elevate.decide");
    assert_eq!(p, json!({"request": "el-2", "decision": "deny"}));
    reply(
        &mut app,
        0,
        req,
        json!({"request": "el-2", "pane": "p2", "decision": "denied", "expires_at_ms": null}),
    );
    assert!(app.ux.elevate.requests.is_empty());
    assert!(app.toasts.last().unwrap().text.contains("denied elevation"));
    // Nothing left: the next key closes the view.
    app.on_key(ch('j'));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn auth_list_after_connect_and_requests_per_machine() {
    let (mut app, mut rx) = fleet_n(2);
    crate::elevate::on_connected(&mut app, 1);
    let (req, _) = only(&commands(&mut rx[1]), "auth.list");
    let t = now_ms() - 60_000;
    reply(
        &mut app,
        1,
        req,
        json!({"pending": [{"request": "el-9", "pane": "p2", "reason": "ci", "created_at_ms": t, "status": "pending"}],
               "elevated": [], "revoked": []}),
    );
    assert_eq!(app.ux.elevate.requests.len(), 1);
    assert_eq!(app.ux.elevate.requests[0].machine, 1);
    open_armed(&mut app);
    let s = screen(&app);
    assert!(s.contains("m1/w1:p2"), "{s}");
    app.on_key(ch('y'));
    // Decided on the request's own machine.
    assert!(commands(&mut rx[0]).is_empty());
    only(&commands(&mut rx[1]), "auth.elevate.decide");
    // An older server without auth.list: nothing shown, no error.
    crate::elevate::on_connected(&mut app, 0);
    let (req, _) = only(&commands(&mut rx[0]), "auth.list");
    reply_err(&mut app, 0, req, "method_not_found", json!({}));
    assert_eq!(app.ux.elevate.requests.len(), 1);
}

#[test]
fn scoped_client_cannot_decide_and_errors_are_explained() {
    let (mut app, mut rx) = fleet();
    requested(&mut app, 0, "el-1", "p1", "x");
    app.ux.elevate.scoped_override = Some(true);
    open_armed(&mut app);
    let s = screen(&app);
    assert!(
        s.contains("runs inside a Vibeke pane and can't decide"),
        "{s}"
    );
    assert!(s.contains("vibeke auth decide el-1 approve|deny"), "{s}");
    app.on_key(ch('y'));
    assert!(commands(&mut rx[0]).is_empty());
    assert!(screen(&app).contains("inside a Vibeke pane: run `vibeke auth decide el-1 approve`"));
    // Full scope here, but the server refuses (e.g. an elevated connection).
    app.ux.elevate.scoped_override = Some(false);
    app.on_key(ch('y'));
    let (req, _) = only(&commands(&mut rx[0]), "auth.elevate.decide");
    reply_err(&mut app, 0, req, "permission_denied", json!({}));
    assert!(screen(&app).contains("not full scope"));
    assert_eq!(
        app.ux.elevate.requests.len(),
        1,
        "kept: decide it elsewhere"
    );
    // Gone on the server: dropped here too.
    app.on_key(ch('y'));
    let (req, _) = only(&commands(&mut rx[0]), "auth.elevate.decide");
    reply_err(&mut app, 0, req, "not_found", json!({}));
    assert!(app.ux.elevate.requests.is_empty());
}

#[test]
fn expired_requests_drop_with_a_deadline() {
    let (mut app, _rx) = fleet();
    requested(&mut app, 0, "el-1", "p1", "x");
    assert!(
        app.deadlines(Instant::now())
            .get("elevate.expire")
            .is_some()
    );
    app.ux.elevate.requests[0].created_at_ms = now_ms() - REQUEST_TTL_MS - 1;
    app.on_tick();
    assert!(app.ux.elevate.requests.is_empty());
    assert!(
        app.deadlines(Instant::now())
            .get("elevate.expire")
            .is_none()
    );
    // No requests: the action only says so.
    app.action("elevation_requests", None);
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("no elevation requests")
    );
}
