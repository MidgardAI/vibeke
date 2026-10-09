//! Devices tab tests: the list (only `kind == "device"` rows), revoke with a confirm, the scope
//! picker, `pair.create` and the link with its QR code, polling `pair.status` (claimed, done,
//! rejected, gone), cancelling a pending link, and the gateway's own messages. Signing in to a
//! relay that needs an account: `account.status` before each link, the device code and its
//! polling, the account line and `s`, and an older gateway without `account.*`.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, named, reply, reply_err, screen};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::{ClientFrame, ServerFrame};

const NOT_RUNNING: &str = "the gateway isn't running: start it with `vibeke gateway on`";
const NO_RELAY: &str = "no relay or app URL is configured: set one in the gateway config";

/// (req, full `gateway.call` params) of the one gateway call for `method`.
fn gw(cmds: &[(u64, String, Value)], method: &str) -> (u64, Value) {
    let m: Vec<_> = cmds
        .iter()
        .filter(|c| c.1 == "gateway.call" && c.2["method"] == method)
        .collect();
    assert_eq!(m.len(), 1, "expected one gateway.call {method} in {cmds:?}");
    (m[0].0, m[0].2.clone())
}

fn reply_msg(app: &mut App, mi: usize, req: u64, kind: &str, message: &str) {
    let json = json!({"jsonrpc": "2.0", "id": req, "error": {"code": -32000, "message": message,
        "data": {"kind": kind, "details": null}}})
    .to_string();
    app.on_frame(mi, ServerFrame::CommandResult { req, json });
}

fn devices_json() -> Value {
    json!({"devices": [
        {"id": "dev1", "name": "Pixel 8", "platform": "android", "scope": "full",
         "paired_at": 1, "fingerprint": "ab:cd", "push": true, "this": false, "kind": "device"},
        {"id": "dev2", "name": "iPhone", "platform": "ios", "scope": "View",
         "paired_at": 1, "fingerprint": "ef:01", "push": false, "this": false, "kind": "device"},
        {"id": "sh1", "name": "shared-pane", "platform": "web", "scope": "view",
         "paired_at": 1, "fingerprint": "", "push": false, "this": false, "kind": "share"},
        {"id": "pe1", "name": "laptop-bob", "platform": "", "scope": "full",
         "paired_at": 1, "fingerprint": "", "push": false, "this": false, "kind": "peer"}
    ]})
}

/// `account.status` of an open relay.
fn open_relay() -> Value {
    json!({"needs_account": false, "account_url": null, "logged_in": false, "login": null,
           "relay": "wss://relay.example"})
}

/// `account.status` of a relay that needs an account nobody has signed in to.
fn signed_out() -> Value {
    json!({"needs_account": true, "account_url": "https://account.example", "logged_in": false,
           "login": null, "relay": "wss://relay.example"})
}

/// The view opened by `action`, its list read and `account.status` answered with `account`.
fn opened_with(account: Value) -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("devices", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Devices)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "devices.list");
    assert_eq!(p, json!({"method": "devices.list", "params": {}}));
    reply(&mut app, 0, req, devices_json());
    let (req, p) = gw(&cmds, "account.status");
    assert_eq!(p, json!({"method": "account.status", "params": {}}));
    reply(&mut app, 0, req, account);
    (app, rxs)
}

fn opened() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    opened_with(open_relay())
}

/// Enter at the scope picker, its `account.status` answered with `account`.
fn confirm(app: &mut App, rxs: &mut [UnboundedReceiver<ClientFrame>], account: Value) {
    app.on_key(named(NamedKey::Enter));
    assert!(screen(app).contains("checking the relay account…"));
    let (req, p) = gw(&commands(&mut rxs[0]), "account.status");
    assert_eq!(p["params"], json!({}));
    reply(app, 0, req, account);
}

/// From the list: `n`, Enter, and the `pair.create` answered with a link.
fn pairing(app: &mut App, rxs: &mut [UnboundedReceiver<ClientFrame>]) -> String {
    app.on_key(ch('n'));
    confirm(app, rxs, open_relay());
    let cmds = commands(&mut rxs[0]);
    let (req, _) = gw(&cmds, "pair.create");
    let pid = "pid42".to_string();
    reply(
        app,
        0,
        req,
        json!({"link": "https://app.example/#/pair?d=abc", "pid": pid,
               "open_by": now_s() + 600, "scope": "full"}),
    );
    // The `gateway.status` asked after the link was made.
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().any(|c| c.1 == "gateway.status"), "{cmds:?}");
    pid
}

#[test]
fn the_list_shows_only_devices() {
    let (app, _rxs) = opened();
    let s = screen(&app);
    assert!(s.contains("Vibeke · Connections · m0"), "{s}");
    assert!(s.contains("Devices · People · Hosts · Handoffs"), "{s}");
    assert!(s.contains("› Pixel 8 · android · full · paired"), "{s}");
    assert!(s.contains("🔔"), "{s}");
    assert!(s.contains("iPhone · ios · view"), "{s}");
    assert!(!s.contains("shared-pane"), "{s}");
    assert!(!s.contains("laptop-bob"), "{s}");
}

#[test]
fn an_empty_list_says_how_to_pair() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("devices", None);
    let (req, _) = gw(&commands(&mut rxs[0]), "devices.list");
    reply(&mut app, 0, req, json!({"devices": []}));
    assert!(screen(&app).contains("No phones paired yet — press n to pair one"));
}

#[test]
fn revoke_asks_first_then_sends_devices_revoke() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('j'));
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Revoke iPhone?"));
    // Anything but y / Enter keeps it.
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('x'));
    app.on_key(ch('y'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "devices.revoke");
    assert_eq!(
        p,
        json!({"method": "devices.revoke", "params": {"device": "dev2"}})
    );
    reply(&mut app, 0, req, json!({}));
    gw(&commands(&mut rxs[0]), "devices.list");
    assert!(screen(&app).contains("revoked iPhone"));
}

#[test]
fn n_then_enter_creates_a_full_pairing_and_shows_the_link() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    let s = screen(&app);
    assert!(s.contains("› Full"), "{s}");
    assert!(s.contains("Approve"), "{s}");
    assert!(s.contains("read-only"), "{s}");
    confirm(&mut app, &mut rxs, open_relay());
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "pair.create");
    assert_eq!(
        p,
        json!({"method": "pair.create", "params": {"scope": "full"}})
    );
    assert!(screen(&app).contains("creating a pairing link…"));
    let link = "https://app.example/#/pair?d=abc";
    reply(
        &mut app,
        0,
        req,
        json!({"link": link, "pid": "pid42", "open_by": now_s() + 600, "scope": "full"}),
    );
    let s = screen(&app);
    assert!(s.contains(link), "{s}");
    assert!(s.contains("Scan with your phone's camera"), "{s}");
    assert!(s.contains("full access"), "{s}");
    assert!(
        s.contains('▀') || s.contains('▄') || s.contains('█'),
        "a QR code: {s}"
    );
}

#[test]
fn another_scope_is_sent_as_picked() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(ch('j'));
    confirm(&mut app, &mut rxs, open_relay());
    let (_, p) = gw(&commands(&mut rxs[0]), "pair.create");
    assert_eq!(p["params"], json!({"scope": "approve"}));
}

#[test]
fn polling_follows_claimed_then_done() {
    let (mut app, mut rxs) = opened();
    let pid = pairing(&mut app, &mut rxs);
    // The first tick asks; one request at a time.
    tick(&mut app);
    tick(&mut app);
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "pair.status");
    assert_eq!(p, json!({"method": "pair.status", "params": {"pid": pid}}));
    reply(
        &mut app,
        0,
        req,
        json!({"status": "claimed", "name": "Pixel 9", "platform": "android",
               "fingerprint": "12:34"}),
    );
    let s = screen(&app);
    assert!(
        s.contains("'Pixel 9' (android) is asking to pair — press y to pair or n to reject"),
        "{s}"
    );
    assert!(s.contains("fingerprint 12:34"), "{s}");
    // Not again before a second has passed.
    tick(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
    let Some(Stage::Pairing(p)) = app.ux.devices.as_mut().map(|v| &mut v.stage) else {
        panic!("pairing");
    };
    p.polled_at = Some(Instant::now() - POLL);
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.status");
    reply(
        &mut app,
        0,
        req,
        json!({"status": "done", "device_id": "dev9", "name": "Pixel 9"}),
    );
    let s = screen(&app);
    assert!(s.contains("Paired ✓ Pixel 9"), "{s}");
    gw(&commands(&mut rxs[0]), "devices.list");
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::List
    ));
}

#[test]
fn rejected_keeps_polling_and_gone_stops() {
    let (mut app, mut rxs) = opened();
    pairing(&mut app, &mut rxs);
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.status");
    reply(&mut app, 0, req, json!({"status": "rejected"}));
    assert!(screen(&app).contains("Rejected — the link stays valid until it expires"));
    let Some(Stage::Pairing(p)) = app.ux.devices.as_mut().map(|v| &mut v.stage) else {
        panic!("pairing");
    };
    p.polled_at = Some(Instant::now() - POLL);
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.status");
    reply(&mut app, 0, req, json!({"status": "gone"}));
    assert!(screen(&app).contains("Link expired — press n for a new one"));
    // Polling stopped, and the link isn't cancelled again on the way out.
    let Some(Stage::Pairing(p)) = app.ux.devices.as_mut().map(|v| &mut v.stage) else {
        panic!("pairing");
    };
    p.polled_at = Some(Instant::now() - POLL);
    tick(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
    // n starts over at the scope picker.
    app.on_key(ch('n'));
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::PickScope { .. }
    ));
}

#[test]
fn past_open_by_the_gateway_still_decides() {
    let (mut app, mut rxs) = opened();
    let pid = pairing(&mut app, &mut rxs);
    let Some(Stage::Pairing(p)) = app.ux.devices.as_mut().map(|v| &mut v.stage) else {
        panic!("pairing");
    };
    p.open_by = now_s() - 1;
    tick(&mut app);
    // Still asked: the pairing may have finished at the last second.
    let (req, p) = gw(&commands(&mut rxs[0]), "pair.status");
    assert_eq!(p["params"], json!({"pid": pid}));
    reply(&mut app, 0, req, json!({"status": "gone"}));
    assert!(screen(&app).contains("Link expired — press n for a new one"));
}

#[test]
fn a_lost_status_reply_is_asked_again_later() {
    let (mut app, mut rxs) = opened();
    pairing(&mut app, &mut rxs);
    tick(&mut app);
    gw(&commands(&mut rxs[0]), "pair.status");
    // No answer (a reconnect dropped it): nothing new until it is stale.
    let Some(Stage::Pairing(p)) = app.ux.devices.as_mut().map(|v| &mut v.stage) else {
        panic!("pairing");
    };
    p.polled_at = Some(Instant::now() - POLL * 2);
    tick(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
    let Some(Stage::Pairing(p)) = app.ux.devices.as_mut().map(|v| &mut v.stage) else {
        panic!("pairing");
    };
    p.inflight = Some(Instant::now() - STALE - POLL);
    tick(&mut app);
    gw(&commands(&mut rxs[0]), "pair.status");
}

#[test]
fn a_late_link_for_an_abandoned_attempt_is_cancelled() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    confirm(&mut app, &mut rxs, open_relay());
    let (old, _) = gw(&commands(&mut rxs[0]), "pair.create");
    // Back out and ask again (another scope) before the first answer.
    app.on_key(named(NamedKey::Escape));
    app.on_key(ch('n'));
    app.on_key(ch('j'));
    confirm(&mut app, &mut rxs, open_relay());
    let (new, p) = gw(&commands(&mut rxs[0]), "pair.create");
    assert_eq!(p["params"], json!({"scope": "approve"}));
    reply(
        &mut app,
        0,
        old,
        json!({"link": "https://app.example/#/pair?d=old", "pid": "old1",
               "open_by": now_s() + 600, "scope": "full"}),
    );
    let (_, p) = gw(&commands(&mut rxs[0]), "share.revoke");
    assert_eq!(p["params"], json!({"id": "old1"}));
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::PickScope { .. }
    ));
    reply(
        &mut app,
        0,
        new,
        json!({"link": "https://app.example/#/pair?d=new", "pid": "new1",
               "open_by": now_s() + 600, "scope": "approve"}),
    );
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().all(|c| c.1 == "gateway.status"), "{cmds:?}");
    assert!(screen(&app).contains("approve access"));
}

#[test]
fn a_link_made_after_closing_is_cancelled() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    confirm(&mut app, &mut rxs, open_relay());
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.create");
    app.on_key(named(NamedKey::Escape));
    app.on_key(named(NamedKey::Escape));
    assert!(app.ux.devices.is_none());
    reply(
        &mut app,
        0,
        req,
        json!({"link": "https://app.example/#/pair?d=x", "pid": "x1",
               "open_by": now_s() + 600, "scope": "full"}),
    );
    let (_, p) = gw(&commands(&mut rxs[0]), "share.revoke");
    assert_eq!(p["params"], json!({"id": "x1"}));
}

#[test]
fn esc_while_pending_cancels_the_link() {
    let (mut app, mut rxs) = opened();
    let pid = pairing(&mut app, &mut rxs);
    app.on_key(named(NamedKey::Escape));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = gw(&cmds, "share.revoke");
    assert_eq!(p, json!({"method": "share.revoke", "params": {"id": pid}}));
    assert!(matches!(app.mode, Mode::Popup(Popup::Devices)));
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::List
    ));
}

#[test]
fn an_unconfigured_relay_shows_the_servers_message() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    confirm(&mut app, &mut rxs, open_relay());
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.create");
    reply_msg(&mut app, 0, req, "unavailable", NO_RELAY);
    let s = screen(&app);
    assert!(s.contains(&format!("✗ {NO_RELAY}")), "{s}");
    // Still at the picker: Esc goes back to the list.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::List
    ));
}

#[test]
fn a_gateway_that_isnt_running_is_said_so() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("devices", None);
    let (req, _) = gw(&commands(&mut rxs[0]), "devices.list");
    reply_msg(&mut app, 0, req, "remote_unavailable", NOT_RUNNING);
    let s = screen(&app);
    assert!(s.contains(&format!("⚠ {NOT_RUNNING}")), "{s}");
    app.on_key(ch('r'));
    let (req, _) = gw(&commands(&mut rxs[0]), "devices.list");
    reply_msg(&mut app, 0, req, "method_not_found", "no such method");
    assert!(screen(&app).contains("has no gateway bridge"));
}

#[test]
fn pair_phone_opens_the_scope_picker() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("pair_phone", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Devices)));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(cmds.len(), 3, "{cmds:?}");
    gw(&cmds, "devices.list");
    gw(&cmds, "account.status");
    assert!(cmds.iter().any(|c| c.1 == "gateway.status"), "{cmds:?}");
    let s = screen(&app);
    assert!(s.contains("Pair a phone: what may it do?"), "{s}");
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::List
    ));
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn the_action_is_bound_and_described() {
    let b = vk_config::default_bindings();
    // `prefix+alt+d` opens the Connections view; `devices` is the palette's way to this tab.
    assert_eq!(b["connections"], "prefix+alt+d");
    assert!(crate::nav::describe("devices").starts_with("Devices"));
    assert_eq!(crate::nav::describe("pair_phone"), "Pair a phone");
}

// ---- signing in to the relay account ------------------------------------------------------------

const CODE: &str = "WDJB-MJHT";
const URI: &str = "https://account.example/device";
const URI_COMPLETE: &str = "https://account.example/device?code=WDJB-MJHT";

fn login_started() -> Value {
    json!({"id": "login1", "verification_uri": URI, "verification_uri_complete": URI_COMPLETE,
           "user_code": CODE, "expires_in": 600})
}

fn sign_in_mut(app: &mut App) -> &mut SignIn {
    match app.ux.devices.as_mut().map(|v| &mut v.stage) {
        Some(Stage::SignIn(s)) => s,
        other => panic!("sign-in: {other:?}"),
    }
}

/// Signed out, `n`, `j` (approve), Enter: `account.login.start` answered with a code.
fn signing_in(app: &mut App, rxs: &mut [UnboundedReceiver<ClientFrame>]) {
    app.on_key(ch('n'));
    app.on_key(ch('j'));
    confirm(app, rxs, signed_out());
    let cmds = commands(&mut rxs[0]);
    assert!(
        !cmds.iter().any(|c| c.2["method"] == "pair.create"),
        "{cmds:?}"
    );
    let (req, p) = gw(&cmds, "account.login.start");
    assert_eq!(p["params"], json!({}));
    reply(app, 0, req, login_started());
}

#[test]
fn an_open_relay_has_no_account_line() {
    let (app, _rxs) = opened();
    let s = screen(&app);
    assert!(!s.contains("Account:"), "{s}");
    assert!(!s.contains("s sign in"), "{s}");
}

#[test]
fn a_signed_out_relay_signs_in_then_pairs_with_the_picked_scope() {
    let (mut app, mut rxs) = opened_with(signed_out());
    signing_in(&mut app, &mut rxs);
    let s = screen(&app);
    assert!(s.contains("Sign in to the relay account"), "{s}");
    assert!(
        s.contains(&format!("Open {URI} on any device and enter the code")),
        "{s}"
    );
    assert!(s.contains(CODE), "{s}");
    assert!(s.contains(URI_COMPLETE), "{s}");
    assert!(s.contains("the code works for 9m 5"), "{s}");
    assert!(s.contains("c copy link · esc cancel"), "{s}");
    assert!(
        s.contains('▀') || s.contains('▄') || s.contains('█'),
        "a QR code: {s}"
    );
    // One request at a time.
    tick(&mut app);
    tick(&mut app);
    let (req, p) = gw(&commands(&mut rxs[0]), "account.login.status");
    assert_eq!(p["params"], json!({"id": "login1"}));
    reply(&mut app, 0, req, json!({"status": "pending"}));
    tick(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
    sign_in_mut(&mut app).polled_at = Some(Instant::now() - POLL);
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.status");
    reply(&mut app, 0, req, json!({"status": "done", "login": "octo"}));
    let (req, p) = gw(&commands(&mut rxs[0]), "pair.create");
    assert_eq!(p["params"], json!({"scope": "approve"}));
    assert!(screen(&app).contains("Signed in as octo"));
    reply(
        &mut app,
        0,
        req,
        json!({"link": "https://app.example/#/pair?d=abc", "pid": "pid42",
               "open_by": now_s() + 600, "scope": "approve"}),
    );
    let s = screen(&app);
    assert!(s.contains("approve access"), "{s}");
    assert!(s.contains("Signed in as octo"), "{s}");
}

#[test]
fn an_expired_code_goes_back_to_the_scope_picker() {
    let (mut app, mut rxs) = opened_with(signed_out());
    signing_in(&mut app, &mut rxs);
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.status");
    reply(&mut app, 0, req, json!({"status": "expired"}));
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::PickScope { sel: 1 }
    ));
    let s = screen(&app);
    assert!(s.contains("the sign-in code expired"), "{s}");
    assert!(s.contains("› Approve"), "{s}");
}

#[test]
fn a_denied_sign_in_goes_back_to_the_list() {
    let (mut app, mut rxs) = opened_with(signed_out());
    signing_in(&mut app, &mut rxs);
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.status");
    reply(
        &mut app,
        0,
        req,
        json!({"status": "error", "message": "account suspended"}),
    );
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::List
    ));
    assert!(screen(&app).contains("✗ the sign-in failed: account suspended"));
}

#[test]
fn esc_cancels_the_sign_in() {
    let (mut app, mut rxs) = opened_with(signed_out());
    signing_in(&mut app, &mut rxs);
    app.on_key(named(NamedKey::Escape));
    let (_, p) = gw(&commands(&mut rxs[0]), "account.login.cancel");
    assert_eq!(p["params"], json!({"id": "login1"}));
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::List
    ));
    // No more polling.
    tick(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn the_list_shows_the_account_and_s_signs_in() {
    let (mut app, mut rxs) = opened_with(signed_out());
    let s = screen(&app);
    assert!(s.contains("Account: sign in required (s)"), "{s}");
    assert!(s.contains("s sign in"), "{s}");
    app.on_key(ch('s'));
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.start");
    reply(&mut app, 0, req, login_started());
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.status");
    reply(&mut app, 0, req, json!({"status": "done", "login": "octo"}));
    // Back at the list, no link made.
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::List
    ));
    let s = screen(&app);
    assert!(s.contains("Signed in as octo"), "{s}");
    assert!(s.contains("Account: octo"), "{s}");
    assert!(!s.contains("s sign in"), "{s}");
    // Signed in: s does nothing.
    app.on_key(ch('s'));
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn a_signed_in_relay_pairs_straight_away() {
    let (mut app, mut rxs) = opened_with(
        json!({"needs_account": true, "logged_in": true, "login": "octo", "relay": null,
               "account_url": "https://account.example"}),
    );
    assert!(screen(&app).contains("Account: octo"));
    app.on_key(ch('n'));
    confirm(
        &mut app,
        &mut rxs,
        json!({"needs_account": true, "logged_in": true, "login": "octo"}),
    );
    gw(&commands(&mut rxs[0]), "pair.create");
}

#[test]
fn an_older_gateway_without_accounts_pairs_as_before() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("devices", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = gw(&cmds, "devices.list");
    reply(&mut app, 0, req, devices_json());
    let (req, _) = gw(&cmds, "account.status");
    reply_msg(
        &mut app,
        0,
        req,
        "method_not_found",
        "unknown method account.status",
    );
    let s = screen(&app);
    assert!(!s.contains("Account:"), "{s}");
    assert!(!s.contains('⚠'), "{s}");
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw(&commands(&mut rxs[0]), "account.status");
    reply_msg(
        &mut app,
        0,
        req,
        "method_not_found",
        "unknown method account.status",
    );
    let (_, p) = gw(&commands(&mut rxs[0]), "pair.create");
    assert_eq!(p["params"], json!({"scope": "full"}));
    // A server whose bridge doesn't carry account.* yet is just as old.
    app.on_key(named(NamedKey::Escape));
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw(&commands(&mut rxs[0]), "account.status");
    reply_msg(
        &mut app,
        0,
        req,
        "invalid_params",
        "gateway.call does not carry account.status (allowed: …)",
    );
    gw(&commands(&mut rxs[0]), "pair.create");
}

#[test]
fn not_needed_after_all_pairs() {
    let (mut app, mut rxs) = opened_with(signed_out());
    app.on_key(ch('n'));
    confirm(&mut app, &mut rxs, signed_out());
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.start");
    reply_err(&mut app, 0, req, "invalid", json!({"reason": "not_needed"}));
    gw(&commands(&mut rxs[0]), "pair.create");
}

#[test]
fn an_unreachable_account_server_is_a_notice() {
    let (mut app, mut rxs) = opened_with(signed_out());
    app.on_key(ch('n'));
    confirm(&mut app, &mut rxs, signed_out());
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.start");
    reply_msg(
        &mut app,
        0,
        req,
        "unavailable",
        "the account server can't be reached",
    );
    let s = screen(&app);
    assert!(s.contains("✗ the account server can't be reached"), "{s}");
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        Stage::PickScope { .. }
    ));
}

#[test]
fn the_gateways_sign_in_text_is_cleaned() {
    assert_eq!(clean("AB\u{202E}CD-\u{2066}EF\u{2069}\u{200F}"), "ABCD-EF");
    assert_eq!(clean(" a\u{1b}[31mb\n "), "a[31mb");
    assert_eq!(clean(&"x".repeat(2000)).len(), CLEAN_MAX);
    let (mut app, mut rxs) = opened_with(signed_out());
    app.on_key(ch('n'));
    confirm(&mut app, &mut rxs, signed_out());
    let (req, _) = gw(&commands(&mut rxs[0]), "account.login.start");
    reply(
        &mut app,
        0,
        req,
        json!({"id": "login1", "verification_uri": URI,
               "verification_uri_complete": format!("{URI_COMPLETE}\u{202E}"),
               "user_code": "WDJB\u{202E}-MJHT", "expires_in": 600}),
    );
    let s = sign_in_mut(&mut app);
    assert_eq!(s.code, CODE);
    assert_eq!(s.uri_complete, URI_COMPLETE);
    assert!(screen(&app).contains(CODE));
}

fn status_of(cmds: &[(u64, String, Value)]) -> u64 {
    let m: Vec<_> = cmds.iter().filter(|c| c.1 == "gateway.status").collect();
    assert_eq!(m.len(), 1, "expected one gateway.status in {cmds:?}");
    m[0].0
}

fn due(app: &mut App) {
    let Some(Stage::Pairing(p)) = app.ux.devices.as_mut().map(|v| &mut v.stage) else {
        panic!("pairing");
    };
    p.polled_at = Some(Instant::now() - POLL);
    app.ux.devices.as_mut().unwrap().gw_asked = Some(Instant::now() - POLL * 3);
}

#[test]
fn an_unreachable_gateway_hides_the_link_until_a_poll_succeeds() {
    let (mut app, mut rxs) = opened();
    pairing(&mut app, &mut rxs);
    assert!(screen(&app).contains("https://app.example/#/pair?d=abc"));
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.status");
    reply_msg(&mut app, 0, req, "remote_unavailable", NOT_RUNNING);
    let s = screen(&app);
    assert!(s.contains(&format!("⚠ {NOT_RUNNING}")), "{s}");
    assert!(!s.contains("https://app.example"), "{s}");
    assert!(!s.contains('▀') && !s.contains('█'), "no QR: {s}");
    assert!(s.contains("until it's back"), "{s}");
    // `c` doesn't copy a link that can't be used.
    app.on_key(ch('c'));
    // While blocked, the server is asked whether the gateway is back.
    due(&mut app);
    tick(&mut app);
    let cmds = commands(&mut rxs[0]);
    let st = status_of(&cmds);
    reply(
        &mut app,
        0,
        st,
        json!({"connected": true, "configured": true, "state": "online"}),
    );
    due(&mut app);
    app.ux.devices.as_mut().unwrap().gw_asked = Some(Instant::now());
    tick(&mut app);
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.status");
    reply(&mut app, 0, req, json!({"status": "pending"}));
    let s = screen(&app);
    assert!(!s.contains('⚠'), "{s}");
    assert!(s.contains("https://app.example/#/pair?d=abc"), "{s}");
}

#[test]
fn a_gateway_that_is_not_online_shows_no_link_or_qr() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    confirm(&mut app, &mut rxs, open_relay());
    let (req, _) = gw(&commands(&mut rxs[0]), "pair.create");
    reply(
        &mut app,
        0,
        req,
        json!({"link": "https://app.example/#/pair?d=abc", "pid": "p1",
               "open_by": now_s() + 600, "scope": "full"}),
    );
    let st = status_of(&commands(&mut rxs[0]));
    reply(
        &mut app,
        0,
        st,
        json!({"connected": true, "configured": true, "state": "offline",
               "last_error": "relay refused the connection"}),
    );
    let s = screen(&app);
    assert!(s.contains("The gateway is offline (state offline)"), "{s}");
    assert!(s.contains("relay refused the connection"), "{s}");
    assert!(s.contains("vibeke gateway on"), "{s}");
    assert!(!s.contains("https://app.example"), "{s}");
    assert!(!s.contains('▀') && !s.contains('█'), "no QR: {s}");
    // Back online (the poll while blocked asks again): the link returns.
    due(&mut app);
    tick(&mut app);
    let st = status_of(&commands(&mut rxs[0]));
    reply(
        &mut app,
        0,
        st,
        json!({"connected": true, "configured": true, "state": "online"}),
    );
    assert!(screen(&app).contains("https://app.example/#/pair?d=abc"));
}

#[test]
fn a_server_without_gateway_status_behaves_as_before() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("devices", None);
    let cmds = commands(&mut rxs[0]);
    let st = status_of(&cmds);
    reply_msg(&mut app, 0, st, "method_not_found", "no such method");
    let (req, _) = gw(&cmds, "devices.list");
    reply(&mut app, 0, req, devices_json());
    let pid = pairing(&mut app, &mut rxs);
    assert_eq!(pid, "pid42");
}
