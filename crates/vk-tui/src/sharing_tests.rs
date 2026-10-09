//! Hosts tab tests: invitation link parsing (the app URL, the bare `d` value,
//! `vibeke://pair?d=…`, garbage refused before any call), each action's exact `gateway.call`
//! params (peer.list / share.list on open, peer.remove, share.revoke for an invitation and a
//! device, share.create for a teammate, peer.invite, peer.redeem with and without the git
//! identity), the QR code and copying the link, the always-ask toggle, colleagues' shares left
//! to the People tab, j/k across the sections, and the gateway's "isn't running" message.

use super::*;
use crate::drafts::tests::{ch, commands, ctl, fleet, named, reply, screen, typ};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::{ClientFrame, ServerFrame};

const NOT_RUNNING: &str = "the gateway isn't running: start it with `vibeke gateway on`";

fn now_s() -> i64 {
    now_ms() / 1000
}

/// The `d` value of a pairing link.
fn d_value(share: Option<Value>, name: &str, exp: i64) -> String {
    let mut v = json!({
        "v": 1, "relay": "wss://relay.example", "host": "H1",
        "hk": URL_SAFE_NO_PAD.encode([1u8; 32]), "pid": "pid1",
        "psk": URL_SAFE_NO_PAD.encode([2u8; 32]), "exp": exp, "name": name
    });
    if let Some(s) = share {
        v["share"] = s;
    }
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap())
}

fn handoff_share(until: i64) -> Value {
    json!({"kind": "handoff", "scope": "full", "until": until, "label": null, "limit": null})
}

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

/// The view opened and its three reads answered.
fn opened() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("sharing", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Sharing)));
    let cmds = commands(&mut rxs[0]);
    let (preq, p) = gw(&cmds, "peer.list");
    assert_eq!(p, json!({"method": "peer.list", "params": {}}));
    let (sreq, p) = gw(&cmds, "share.list");
    assert_eq!(p, json!({"method": "share.list", "params": {}}));
    let (req, p) = crate::drafts::tests::only(&cmds, "handoff.prefs");
    assert_eq!(p, json!({}));
    let exp = now_s() + 23 * 3600 + 120;
    reply(
        &mut app,
        0,
        preq,
        json!({"peers": [
            {"id": "pr1", "name": "marvin", "owner": "self", "relay": "wss://r", "host": "H2",
             "device_id": "d1", "added_at": 1, "expires_at": null, "expired": false},
            {"id": "pr2", "name": "laptop-anna", "owner": "teammate", "relay": "wss://r",
             "host": "H3", "device_id": "d2", "added_at": 1, "expires_at": exp, "expired": false}
        ]}),
    );
    reply(
        &mut app,
        0,
        sreq,
        json!({
            "invitations": [{"id": "pidA", "kind": "handoff", "scope": "full", "label": null,
                             "limit": null, "created": 1, "link_expires_at": now_s() + 600,
                             "device_expires_at": now_s() + 86_400},
                            {"id": "pidS", "kind": "share", "scope": "view", "label": "Sam",
                             "limit": {"pane": "p1"}, "created": 1,
                             "link_expires_at": now_s() + 600,
                             "device_expires_at": now_s() + 7200}],
            "devices": [{"id": "dev9", "kind": "peer", "name": "laptop-bob", "scope": "full",
                         "paired_at": 1, "owner": "teammate",
                         "sender": {"host_name": "laptop-bob",
                                    "user": {"name": "Bob", "email": "bob@example.com"}},
                         "expires_at": exp, "limit": null},
                        {"id": "sh1", "kind": "share", "name": "Kim's browser",
                         "scope": "approve", "paired_at": 1, "owner": null, "sender": null,
                         "expires_at": exp, "limit": {"workspace": "W1"}}]
        }),
    );
    reply(
        &mut app,
        0,
        req,
        json!({"always_ask": false, "placement": {}, "repos": []}),
    );
    (app, rxs)
}

#[test]
fn links_parse_in_all_three_forms_and_garbage_is_refused() {
    let until = now_s() + 23 * 3600 + 120;
    let d = d_value(Some(handoff_share(until)), "laptop-anna", now_s() + 900);
    for form in [
        format!("https://app.example/#/pair?d={d}"),
        d.clone(),
        format!("vibeke://pair?d={d}"),
        format!("  https://app.example/#/pair?d={d}&x=1\n"),
    ] {
        let inv = parse_link(&form).unwrap_or_else(|e| panic!("{form}: {e}"));
        assert_eq!(inv.kind, InviteKind::Handoff);
        assert_eq!(inv.host_name, "laptop-anna");
        assert_eq!(inv.until, Some(until));
        assert_eq!(
            inv.describe(now_s()),
            "Handoff invitation from laptop-anna (teammate), valid 23h"
        );
        assert_eq!(inv.refusal(now_s()), None);
    }
    let peer = parse_link(&d_value(
        Some(json!({"kind": "peer", "scope": "full", "until": 0})),
        "mini",
        now_s() + 900,
    ))
    .unwrap();
    assert_eq!(peer.kind, InviteKind::Peer);
    assert_eq!(peer.describe(now_s()), "Pair your host mini");
    // Links for the app, and expired links, parse but are refused.
    let device = parse_link(&d_value(None, "mini", now_s() + 900)).unwrap();
    assert!(device.refusal(now_s()).unwrap().contains("Vibeke app"));
    let old = parse_link(&d_value(Some(handoff_share(until)), "x", now_s() - 60)).unwrap();
    assert!(old.refusal(now_s()).unwrap().contains("expired"));
    // Garbage.
    let not_json = URL_SAFE_NO_PAD.encode(b"hello");
    let short_key = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({"v": 1, "relay": "r", "host": "h", "hk": "AAAA",
            "pid": "p", "psk": URL_SAFE_NO_PAD.encode([2u8; 32]), "exp": 1, "name": "n"}))
        .unwrap(),
    );
    for bad in [
        "",
        "hello there",
        "https://example.com/#/pair?d=%%%",
        not_json.as_str(),
        short_key.as_str(),
    ] {
        assert!(parse_link(bad).is_err(), "{bad}");
    }
    let v2 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"v": 2})).unwrap());
    assert_eq!(
        parse_link(&v2).unwrap_err(),
        "this invitation needs a newer Vibeke"
    );
}

#[test]
fn the_view_lists_peers_invitations_and_devices() {
    let (app, _rxs) = opened();
    let s = screen(&app);
    assert!(s.contains("Vibeke · Connections · m0"), "{s}");
    assert!(s.contains("Devices · People · Hosts · Handoffs"), "{s}");
    // Colleagues' shares are the People tab's.
    assert!(!s.contains("Sam"), "{s}");
    assert!(!s.contains("Kim"), "{s}");
    let v = app.ux.sharing.view.as_ref().unwrap();
    assert_eq!(v.invitations.len(), 1);
    assert_eq!(v.devices.len(), 1);
    assert!(s.contains("Peers — hosts m0 can hand work off to"), "{s}");
    assert!(s.contains("› marvin"), "{s}");
    assert!(s.contains("your host"), "{s}");
    assert!(s.contains("teammate · expires in 23h"), "{s}");
    assert!(
        s.contains("handoff · a teammate sends to me · open for"),
        "{s}"
    );
    assert!(
        s.contains("laptop-bob · peer · teammate · from laptop-bob (Bob <bob@example.com>)"),
        "{s}"
    );
    assert!(
        s.contains("[ ] Always ask before importing handoffs"),
        "{s}"
    );
}

#[test]
fn remove_cancel_and_revoke_ask_first_and_send_the_exact_calls() {
    let (mut app, mut rxs) = opened();
    // Peers: the second one.
    app.on_key(ch('j'));
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Remove laptop-anna?"));
    // Any other key keeps it.
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('x'));
    app.on_key(ch('y'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "peer.remove");
    assert_eq!(p, json!({"method": "peer.remove", "params": {"id": "pr2"}}));
    reply(&mut app, 0, req, json!({}));
    let cmds = commands(&mut rxs[0]);
    gw(&cmds, "peer.list");
    assert!(screen(&app).contains("removed laptop-anna"));
    // Invitations: j moves past the last peer into the next section.
    app.on_key(ch('j'));
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Cancel this handoff invitation?"));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "share.revoke");
    assert_eq!(
        p,
        json!({"method": "share.revoke", "params": {"id": "pidA"}})
    );
    reply(&mut app, 0, req, json!({"cancelled": "invitation"}));
    let cmds = commands(&mut rxs[0]);
    gw(&cmds, "share.list");
    // Invited hosts.
    app.on_key(ch('j'));
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Revoke laptop-bob?"));
    app.on_key(ch('y'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "share.revoke");
    assert_eq!(
        p,
        json!({"method": "share.revoke", "params": {"id": "dev9"}})
    );
    reply(&mut app, 0, req, json!({"cancelled": "device"}));
    assert!(screen(&app).contains("revoked laptop-bob"));
}

#[test]
fn new_invitations_show_the_link_with_a_qr_code_and_copy_it() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    assert!(screen(&app).contains("[t] Invite a teammate to send to me"));
    app.on_key(ch('t'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "share.create");
    assert_eq!(
        p,
        json!({"method": "share.create", "params": {"kind": "handoff", "ttl_s": 86_400}})
    );
    let link = "https://app.example/#/pair?d=abc";
    reply(
        &mut app,
        0,
        req,
        json!({"link": link, "pid": "pidB", "open_by": now_s() + 900,
               "expires_at": now_s() + 86_400, "expires_after_s": 86_400}),
    );
    let s = screen(&app);
    assert!(s.contains(link), "{s}");
    assert!(s.contains("Invitation for a teammate"), "{s}");
    assert!(
        s.contains('▀') || s.contains('▄') || s.contains('█'),
        "a QR code: {s}"
    );
    assert!(s.contains("[c] copy link"), "{s}");
    // The list is read again for the new invitation.
    gw(&commands(&mut rxs[0]), "share.list");
    app.on_key(ch('c'));
    assert_eq!(
        app.clipboard_sink.as_ref().unwrap().last(),
        Some(&(link.as_bytes().to_vec(), false))
    );
    assert!(app.toasts.iter().any(|t| t.text.starts_with("copied")));
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Popup(Popup::Sharing)));
    assert!(app.ux.sharing.view.as_ref().unwrap().created.is_none());
    // Pair another of my hosts.
    app.on_key(ch('n'));
    app.on_key(ch('h'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "peer.invite");
    assert_eq!(p, json!({"method": "peer.invite", "params": {}}));
    reply(
        &mut app,
        0,
        req,
        json!({"link": "vibekeinvitecode", "pid": "pidC", "open_by": now_s() + 900}),
    );
    assert!(screen(&app).contains("Peer invitation: accept it on your other host"));
    assert!(render_qr("vibekeinvitecode").is_some());
}

#[test]
fn a_pasted_invitation_says_what_it_is_then_redeems() {
    let (mut app, mut rxs) = opened();
    let until = now_s() + 23 * 3600 + 120;
    let url = format!(
        "https://app.example/#/pair?d={}",
        d_value(Some(handoff_share(until)), "laptop-anna", now_s() + 900)
    );
    // A bracketed paste opens the field.
    app.on_paste(format!("{url}\n"));
    let s = screen(&app);
    assert!(
        s.contains("Handoff invitation from laptop-anna (teammate), valid 23h"),
        "{s}"
    );
    assert!(s.contains("[ ] Show my git name and email"), "{s}");
    assert!(commands(&mut rxs[0]).is_empty(), "nothing before Accept");
    // Show my git name and email, then Accept.
    app.on_key(named(NamedKey::Tab));
    app.on_key(named(NamedKey::Space));
    assert!(screen(&app).contains("[x] Show my git name and email"));
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw(&cmds, "peer.redeem");
    assert_eq!(
        p,
        json!({"method": "peer.redeem", "params": {"link": url, "share_user": true},
               "timeout_ms": 60_000})
    );
    assert!(screen(&app).contains("⏳ accepting the invitation from laptop-anna…"));
    reply(
        &mut app,
        0,
        req,
        json!({"peer": {"id": "pr3", "name": "laptop-anna", "owner": "teammate"}}),
    );
    let v = app.ux.sharing.view.as_ref().unwrap();
    assert!(v.paste.is_none());
    assert_eq!(
        v.notice.as_deref(),
        Some("paired with laptop-anna: you can hand off to it now")
    );
    gw(&commands(&mut rxs[0]), "peer.list");
    // Typed in place, a bare peer code; the git identity stays off by default.
    app.on_key(ch('p'));
    let code = d_value(
        Some(json!({"kind": "peer", "scope": "full", "until": 0})),
        "mini",
        now_s() + 900,
    );
    typ(&mut app, &code);
    assert!(screen(&app).contains("Pair your host mini"));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = gw(&cmds, "peer.redeem");
    assert_eq!(p["params"], json!({"link": code, "share_user": false}));
}

#[test]
fn garbage_and_expired_pastes_are_refused_before_any_call() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('p'));
    typ(&mut app, "nonsense");
    assert!(screen(&app).contains("✗ not a Vibeke invitation link"));
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ctl('u'));
    let old = d_value(
        Some(handoff_share(now_s() + 3600)),
        "laptop-anna",
        now_s() - 120,
    );
    typ(&mut app, &old);
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    let f = app.ux.sharing.view.as_ref().unwrap().paste.clone().unwrap();
    assert!(f.error.unwrap().contains("expired"));
    // An app share link: for the app, not this host.
    app.on_key(ctl('u'));
    typ(
        &mut app,
        &d_value(
            Some(json!({"kind": "share", "scope": "view", "until": now_s() + 3600})),
            "mini",
            now_s() + 900,
        ),
    );
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("open it there"));
    // esc leaves the field, esc again the view.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Popup(Popup::Sharing)));
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn always_ask_toggles_through_handoff_prefs() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('a'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = crate::drafts::tests::only(&cmds, "handoff.prefs");
    assert_eq!(p, json!({"always_ask": true}));
    reply(
        &mut app,
        0,
        req,
        json!({"always_ask": true, "placement": {}, "repos": []}),
    );
    let s = screen(&app);
    assert!(
        s.contains("[x] Always ask before importing handoffs"),
        "{s}"
    );
    assert!(s.contains("now wait for you"), "{s}");
}

#[test]
fn a_gateway_that_isnt_running_is_said_so() {
    let (mut app, mut rxs) = fleet();
    app.action("sharing", None);
    let cmds = commands(&mut rxs[0]);
    let (preq, _) = gw(&cmds, "peer.list");
    let (sreq, _) = gw(&cmds, "share.list");
    reply_msg(&mut app, 0, preq, "remote_unavailable", NOT_RUNNING);
    reply_msg(&mut app, 0, sreq, "remote_unavailable", NOT_RUNNING);
    let s = screen(&app);
    assert!(s.contains(&format!("⚠ {NOT_RUNNING}")), "{s}");
    // An older server has no bridge.
    app.on_key(ch('g'));
    let cmds = commands(&mut rxs[0]);
    let (preq, _) = gw(&cmds, "peer.list");
    reply_msg(&mut app, 0, preq, "method_not_found", "no such method");
    assert!(screen(&app).contains("has no gateway bridge"));
}
