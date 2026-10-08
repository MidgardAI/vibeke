//! Request review view: elevation and approved-call requests from pushed events and
//! `auth.list`, the chrome-only notice, the view replacing the pane area (the requesting pane's
//! content is not on screen), the arm delay, explicit y/n (and a for approved calls) through
//! `auth.elevate.decide` / `auth.approve.decide`, never auto-approving, never opening by itself
//! (not even for the focused shell pane running vibeke), the selection kept by request id and
//! cleared when that request goes away, re-arming on every change of selection, fresh unmodified
//! presses only (no repeats, releases, hyper/meta, keys held since before arming), scoped
//! clients, and errors.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, fleet_n, named, only, reply, reply_err, screen};
use vk_proto::input::Mods;
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
    for m in [
        Mods::CTRL,
        Mods::ALT,
        Mods::SUPER,
        Mods::HYPER,
        Mods::META,
        Mods::SHIFT,
    ] {
        app.on_key(KeyEvent::new(Key::Char('y'), m));
    }
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
    // The decided request is gone: nothing is selected until the user selects again.
    app.on_key(ch('n'));
    assert!(commands(&mut rx[0]).is_empty());
    assert!(screen(&app).contains("select one with j/k"));
    app.on_key(ch('j'));
    arm(&mut app);
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
            .contains("no elevation or approval requests")
    );
}

// ---- approved calls (auth.approve) -------------------------------------------------------------

const SUMMARY: &str =
    "Send pane w1:p2 (repo api, branch main, 3 changed files, agent: none) to marvin (your host)";

fn approval(app: &mut App, mi: usize, id: &str, pane: &str, method: &str, always: bool) {
    event(
        app,
        mi,
        "auth.approval_requested",
        json!({"seq": 1, "ts": now_ms(), "type": "auth.approval_requested",
               "subject": {"pane": pane, "request": id},
               "data": {"method": method, "summary": SUMMARY, "reason": "ship\x1b[2J it",
                        "peer": "pe-1", "always_allowed": always}}),
    );
}

/// Focus the shell pane `p2` (no agent run) and give it a foreground process.
fn focus_shell(app: &mut App, fg: &[&str]) {
    app.machines[0].focus.pane = Some("p2".into());
    let p = app.machines[0]
        .model
        .panes
        .iter_mut()
        .find(|p| p.id == "p2")
        .unwrap();
    p.fg_cmdline = fg.iter().map(|s| s.to_string()).collect();
}

fn arm(app: &mut App) {
    app.ux.elevate.view.as_mut().unwrap().opened_at = Instant::now() - ARM_DELAY * 2;
}

fn with_kind(mut e: KeyEvent, k: KeyKind) -> KeyEvent {
    e.kind = k;
    e
}

#[test]
fn approval_from_the_focused_shell_pane_running_vibeke_does_not_open_the_view() {
    // A pane controls its own argv: looking like the vibeke CLI earns nothing.
    let (mut app, mut rx) = fleet();
    app.ux.elevate.scoped_override = Some(false);
    app.machines[0].panes.get_mut("p2").unwrap().lines = vec![row_of("PANE-CONTENT")];
    focus_shell(
        &mut app,
        &["/usr/local/bin/vibeke", "handoff", "send", "marvin"],
    );
    approval(&mut app, 0, "ap-1", "p2", "handoff.send", true);
    app.on_tick();
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.ux.elevate.view.is_none());
    let sent = commands(&mut rx[0]);
    assert!(
        !sent.iter().any(|(_, m, _)| m.starts_with("auth.")),
        "{sent:?}"
    );
    // Only the notice, and the pane stays on screen.
    let s = screen(&app);
    let top = s.lines().next().unwrap();
    assert!(top.contains("asks to send a handoff"), "{top}");
    assert!(top.contains("prefix+shift+e"), "{top}");
    assert!(s.contains("PANE-CONTENT"), "{s}");
    // The foreground process changing later does not open it either.
    focus_shell(&mut app, &["vibeke", "handoff", "send", "marvin"]);
    app.on_tick();
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn a_withdrawn_selection_selects_nothing_and_a_new_selection_re_arms() {
    let (mut app, mut rx) = fleet();
    app.ux.elevate.scoped_override = Some(false);
    approval(&mut app, 0, "ap-a", "p1", "handoff.send", true);
    approval(&mut app, 0, "ap-b", "p2", "handoff.send", true);
    open_armed(&mut app);
    assert_eq!(
        app.ux.elevate.view.as_ref().unwrap().sel,
        Some((0, "ap-a".to_string()))
    );
    // The pane withdraws A just before the user presses y: B must not be approved.
    event(
        &mut app,
        0,
        "auth.approval_withdrawn",
        json!({"subject": {"pane": "p1", "request": "ap-a"}, "data": {}}),
    );
    assert_eq!(app.ux.elevate.view.as_ref().unwrap().sel, None);
    for c in "yan".chars() {
        app.on_key(ch(c));
    }
    assert!(
        commands(&mut rx[0]).is_empty(),
        "nothing selected, nothing decided"
    );
    assert!(matches!(app.mode, Mode::Popup(Popup::Elevate)));
    let s = screen(&app);
    assert!(s.contains("select one with j/k"), "{s}");
    assert!(s.contains("ap-b"), "{s}");
    // Selecting B re-arms: keys right after it are ignored.
    app.on_key(ch('j'));
    assert_eq!(
        app.ux.elevate.view.as_ref().unwrap().sel,
        Some((0, "ap-b".to_string()))
    );
    app.on_key(ch('y'));
    assert!(
        commands(&mut rx[0]).is_empty(),
        "re-armed after the selection changed"
    );
    arm(&mut app);
    app.on_key(ch('y'));
    let (_, p) = only(&commands(&mut rx[0]), "auth.approve.decide");
    assert_eq!(p, json!({"request": "ap-b", "decision": "approve"}));
    // j/k to another request re-arms too.
    let (mut app, mut rx) = fleet();
    app.ux.elevate.scoped_override = Some(false);
    approval(&mut app, 0, "ap-a", "p1", "handoff.send", true);
    approval(&mut app, 0, "ap-b", "p2", "handoff.send", true);
    open_armed(&mut app);
    app.on_key(ch('j'));
    app.on_key(ch('y'));
    assert!(commands(&mut rx[0]).is_empty());
    // An auth.list snapshot without the selected request clears the selection as well.
    arm(&mut app);
    crate::elevate::on_connected(&mut app, 0);
    let (req, _) = only(&commands(&mut rx[0]), "auth.list");
    let t = now_ms();
    reply(
        &mut app,
        0,
        req,
        json!({"pending": [], "approvals": [{"request": "ap-a", "pane": "p1", "method": "handoff.send",
               "summary": "x", "created_at_ms": t, "status": "pending"}]}),
    );
    assert_eq!(app.ux.elevate.view.as_ref().unwrap().sel, None);
    app.on_key(ch('y'));
    assert!(commands(&mut rx[0]).is_empty());
}

#[test]
fn repeats_releases_and_keys_held_since_before_arming_never_decide() {
    let (mut app, mut rx) = fleet();
    app.kitty = true;
    app.ux.elevate.scoped_override = Some(false);
    approval(&mut app, 0, "ap-1", "p1", "handoff.send", true);
    // `y` held from before the view opened: only its repeats (and release) reach the view.
    open_armed(&mut app);
    app.on_key(with_kind(ch('y'), KeyKind::Repeat));
    app.on_key(with_kind(ch('y'), KeyKind::Repeat));
    assert!(commands(&mut rx[0]).is_empty(), "a repeat never decides");
    // A press without the release seen in between does not count either.
    app.on_key(ch('y'));
    assert!(commands(&mut rx[0]).is_empty());
    app.on_key(with_kind(ch('y'), KeyKind::Release));
    assert!(commands(&mut rx[0]).is_empty(), "a release never decides");
    // Pressed while not armed and still held when the delay ends.
    app.ux.elevate.view.as_mut().unwrap().opened_at = Instant::now();
    app.on_key(ch('a'));
    arm(&mut app);
    app.on_key(with_kind(ch('a'), KeyKind::Repeat));
    app.on_key(ch('a'));
    assert!(commands(&mut rx[0]).is_empty(), "held since before arming");
    app.on_key(with_kind(ch('a'), KeyKind::Release));
    // Hyper and meta are modifiers too.
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::HYPER));
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::META));
    assert!(commands(&mut rx[0]).is_empty());
    // Released, then a fresh plain press: decides.
    app.on_key(ch('y'));
    let (_, p) = only(&commands(&mut rx[0]), "auth.approve.decide");
    assert_eq!(p, json!({"request": "ap-1", "decision": "approve"}));
    // Without the kitty protocol (no repeats or releases reported) the arm delay is the guard:
    // a key typed during it does not lock that key out.
    let (mut app, mut rx) = fleet();
    app.ux.elevate.scoped_override = Some(false);
    approval(&mut app, 0, "ap-1", "p1", "handoff.send", true);
    app.action("elevation_requests", None);
    app.on_key(ch('n'));
    assert!(commands(&mut rx[0]).is_empty());
    arm(&mut app);
    app.on_key(ch('n'));
    let (_, p) = only(&commands(&mut rx[0]), "auth.approve.decide");
    assert_eq!(p, json!({"request": "ap-1", "decision": "deny"}));
}

#[test]
fn approval_view_shows_the_summary_then_the_unverified_reason() {
    let (mut app, _rx) = fleet();
    approval(&mut app, 0, "ap-1", "p1", "handoff.send", true);
    open_armed(&mut app);
    let s = screen(&app);
    assert!(
        s.contains("Vibeke · approval request — drawn by Vibeke"),
        "{s}"
    );
    assert!(
        s.contains("Pane w1:p1 (claude · api) asks to send a handoff"),
        "{s}"
    );
    assert!(s.contains("worked out by Vibeke from its own facts"), "{s}");
    assert!(s.contains("Send pane w1:p2 (repo api, branch main"), "{s}");
    assert!(s.contains("written by the pane; unverified"), "{s}");
    assert!(s.contains("ship[2J it"), "{s}");
    assert!(!s.contains('\x1b'));
    // The summary comes before the reason.
    assert!(s.find("Send pane w1:p2").unwrap() < s.find("unverified").unwrap());
    assert!(
        s.contains("[y] approve once   [a] always   [n] deny"),
        "{s}"
    );
}

#[test]
fn approval_keys_y_a_n_send_auth_approve_decide_with_exact_params() {
    let (mut app, mut rx) = fleet();
    app.ux.elevate.scoped_override = Some(false);
    approval(&mut app, 0, "ap-1", "p1", "handoff.send", true);
    approval(&mut app, 0, "ap-2", "p1", "handoff.cancel", true);
    approval(&mut app, 0, "ap-3", "p2", "gateway.call", false);
    open_armed(&mut app);
    // Modified keys never decide.
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
    app.on_key(KeyEvent::new(Key::Char('a'), Mods::ALT));
    app.on_key(KeyEvent::new(Key::Char('n'), Mods::SUPER));
    assert!(commands(&mut rx[0]).is_empty());
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rx[0]), "auth.approve.decide");
    assert_eq!(p, json!({"request": "ap-1", "decision": "approve"}));
    // One decision in flight per request.
    app.on_key(ch('a'));
    assert!(commands(&mut rx[0]).is_empty());
    reply(
        &mut app,
        0,
        req,
        json!({"request": "ap-1", "pane": "p1", "decision": "approved", "grant": "once",
               "ok": true, "result": {"job": {"id": "job-7", "state": "queued"}}, "error": null}),
    );
    let last = &app.toasts.last().unwrap().text;
    assert!(last.contains("approved for w1:p1"), "{last}");
    assert!(last.contains("job-7"), "{last}");
    // The decided request is gone: select the next one (and wait out the arm delay again).
    app.on_key(ch('j'));
    arm(&mut app);
    app.on_key(ch('a'));
    let (req, p) = only(&commands(&mut rx[0]), "auth.approve.decide");
    assert_eq!(p, json!({"request": "ap-2", "decision": "always"}));
    reply(
        &mut app,
        0,
        req,
        json!({"request": "ap-2", "pane": "p1", "decision": "approved", "grant": "always",
               "ok": false, "result": null, "error": {"kind": "conflict", "message": "the handoff is already done"}}),
    );
    let last = &app.toasts.last().unwrap().text;
    assert!(last.contains("always from this pane"), "{last}");
    assert!(last.contains("the handoff is already done"), "{last}");
    app.on_key(ch('j'));
    arm(&mut app);
    // peer.redeem: no "always" (not offered, and `a` sends nothing).
    let s = screen(&app);
    assert!(s.contains("can only be approved once"), "{s}");
    assert!(!s.contains("[a] always"), "{s}");
    app.on_key(ch('a'));
    assert!(commands(&mut rx[0]).is_empty());
    app.on_key(ch('n'));
    let (req, p) = only(&commands(&mut rx[0]), "auth.approve.decide");
    assert_eq!(p, json!({"request": "ap-3", "decision": "deny"}));
    reply(
        &mut app,
        0,
        req,
        json!({"request": "ap-3", "pane": "p2", "decision": "denied", "grant": null,
               "ok": false, "result": null, "error": null}),
    );
    assert!(app.ux.elevate.requests.is_empty());
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("denied the request of w1:p2")
    );
}

#[test]
fn approval_keys_within_the_arm_delay_never_decide() {
    let (mut app, mut rx) = fleet();
    app.ux.elevate.scoped_override = Some(false);
    approval(&mut app, 0, "ap-1", "p1", "handoff.send", true);
    app.action("elevation_requests", None);
    for c in "yan".chars() {
        app.on_key(ch(c));
    }
    assert!(commands(&mut rx[0]).is_empty());
    assert!(matches!(app.mode, Mode::Popup(Popup::Elevate)));
    arm(&mut app);
    app.on_key(ch('n'));
    let (_, p) = only(&commands(&mut rx[0]), "auth.approve.decide");
    assert_eq!(p, json!({"request": "ap-1", "decision": "deny"}));
}

#[test]
fn approvals_from_auth_list_and_their_end_events() {
    let (mut app, mut rx) = fleet();
    crate::elevate::on_connected(&mut app, 0);
    let (req, _) = only(&commands(&mut rx[0]), "auth.list");
    let t = now_ms() - 60_000;
    reply(
        &mut app,
        0,
        req,
        json!({"pending": [{"request": "el-1", "pane": "p1", "reason": "", "created_at_ms": t - 1, "status": "pending"}],
               "approvals": [{"request": "ap-1", "kind": "approval", "pane": "p2", "pane_handle": "w1:p2",
                              "workspace": "W1", "method": "handoff.send", "params": {}, "summary": SUMMARY,
                              "facts": {}, "reason": "", "reason_verified": false,
                              "peer": {"id": "pe-1", "name": "marvin", "owner": "self"},
                              "always_allowed": true, "created_at_ms": t, "status": "pending"},
                             {"request": "ap-0", "kind": "approval", "pane": "p2", "method": "handoff.send",
                              "summary": "x", "created_at_ms": t, "status": "running"}],
               "elevated": [], "revoked": [], "grants": []}),
    );
    assert_eq!(app.ux.elevate.requests.len(), 2);
    let ap = app.ux.elevate.requests[1].approval().unwrap().clone();
    assert_eq!(ap.peer.as_deref(), Some("marvin"));
    assert!(ap.always_allowed);
    // Listed requests never open by themselves.
    assert!(matches!(app.mode, Mode::Normal));
    open_armed(&mut app);
    app.on_key(ch('j'));
    let s = screen(&app);
    assert!(s.contains("Vibeke · requests (2)"), "{s}");
    assert!(s.contains("[a] also runs the same call"), "{s}");
    assert!(s.contains("to marvin"), "{s}");
    for (k, id) in [
        ("auth.approval_withdrawn", "ap-1"),
        ("auth.elevate_denied", "el-1"),
    ] {
        event(
            &mut app,
            0,
            k,
            json!({"subject": {"pane": "p2", "request": id}, "data": {"reason": "withdrawn"}}),
        );
    }
    assert!(app.ux.elevate.requests.is_empty());
}

#[test]
fn scoped_client_cannot_decide_an_approval_and_errors_are_explained() {
    let (mut app, mut rx) = fleet();
    app.ux.elevate.scoped_override = Some(true);
    approval(&mut app, 0, "ap-1", "p1", "handoff.send", true);
    open_armed(&mut app);
    let s = screen(&app);
    assert!(
        s.contains("vibeke auth approval ap-1 approve|always|deny"),
        "{s}"
    );
    app.on_key(ch('a'));
    assert!(commands(&mut rx[0]).is_empty());
    assert!(screen(&app).contains("run `vibeke auth approval ap-1 always` outside Vibeke"));
    app.ux.elevate.scoped_override = Some(false);
    app.on_key(ch('y'));
    let (req, _) = only(&commands(&mut rx[0]), "auth.approve.decide");
    reply_err(&mut app, 0, req, "permission_denied", json!({}));
    assert!(screen(&app).contains("not full scope"));
    assert_eq!(app.ux.elevate.requests.len(), 1);
    app.on_key(ch('y'));
    let (req, _) = only(&commands(&mut rx[0]), "auth.approve.decide");
    reply_err(&mut app, 0, req, "conflict", json!({}));
    assert!(app.ux.elevate.requests.is_empty());
}
