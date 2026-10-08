//! Devices view tests: the list (only `kind == "device"` rows), revoke with a confirm, the scope
//! picker, `pair.create` and the link with its QR code, polling `pair.status` (claimed, done,
//! rejected, gone), cancelling a pending link, and the gateway's own messages.

use super::*;
use crate::drafts::tests::{ch, commands, fleet, named, only, reply, screen};
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

/// The view opened by `action` and its list read answered.
fn opened() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("devices", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Devices)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "devices.list");
    assert_eq!(p, json!({"method": "devices.list", "params": {}}));
    reply(&mut app, 0, req, devices_json());
    (app, rxs)
}

/// From the list: `n`, Enter, and the `pair.create` answered with a link.
fn pairing(app: &mut App, rxs: &mut [UnboundedReceiver<ClientFrame>]) -> String {
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
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
    pid
}

#[test]
fn the_list_shows_only_devices() {
    let (app, _rxs) = opened();
    let s = screen(&app);
    assert!(s.contains("Vibeke · Devices · m0"), "{s}");
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
    app.on_key(named(NamedKey::Enter));
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
    app.on_key(named(NamedKey::Enter));
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
        s.contains(
            "'Pixel 9' (android) is asking to pair — confirm in the prompt, fingerprint 12:34"
        ),
        "{s}"
    );
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
    app.on_key(named(NamedKey::Enter));
    let (old, _) = gw(&commands(&mut rxs[0]), "pair.create");
    // Back out and ask again (another scope) before the first answer.
    app.on_key(named(NamedKey::Escape));
    app.on_key(ch('n'));
    app.on_key(ch('j'));
    app.on_key(named(NamedKey::Enter));
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
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("approve access"));
}

#[test]
fn a_link_made_after_closing_is_cancelled() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
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
    app.on_key(named(NamedKey::Enter));
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
    only(&commands(&mut rxs[0]), "gateway.call");
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
    assert_eq!(b["devices"], "prefix+alt+d");
    assert!(crate::nav::describe("devices").starts_with("Devices"));
    assert_eq!(crate::nav::describe("pair_phone"), "Pair a phone");
}
