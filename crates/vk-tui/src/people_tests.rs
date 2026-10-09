//! People tab tests: the list (only `kind == "share"` devices and unused links, with the pane or
//! workspace named from the model), the form's exact `share.create` params (pane or workspace,
//! scope, ttl, label), the link with its QR code and what it grants, hiding the link while the
//! gateway is offline, revoking and cancelling after a confirm, an older gateway's refusal and a
//! late link for an abandoned form. Control: `scope: "full"`, only after `y`, and the warnings
//! for approve and Control.

use super::*;
use crate::drafts::tests::{ch, commands, ctl, fleet, named, only, reply, screen, typ};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::{ClientFrame, ServerFrame};

const LINK: &str = "https://app.example/#/pair?d=share";

/// (req, full `gateway.call` params) of the one gateway call for `method`.
fn gw_of(cmds: &[(u64, String, Value)], method: &str) -> (u64, Value) {
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

fn shares_json() -> Value {
    json!({
        "invitations": [
            {"id": "pidS", "kind": "share", "scope": "view", "label": "Sam",
             "limit": {"pane": "p1", "workspace": null}, "created": 1,
             "link_expires_at": now_s() + 600, "device_expires_at": now_s() + 7200 + 120},
            {"id": "pidH", "kind": "handoff", "scope": "full", "label": null, "limit": null,
             "created": 1, "link_expires_at": now_s() + 600,
             "device_expires_at": now_s() + 86_400}
        ],
        "devices": [
            {"id": "sh1", "kind": "share", "name": "Kim's browser", "scope": "approve",
             "paired_at": 1, "owner": null, "sender": null,
             "expires_at": now_s() + 8 * 3600 + 120,
             "limit": {"workspace": "W1", "pane": null}},
            {"id": "sh2", "kind": "share", "name": "web", "scope": "view", "paired_at": 1,
             "owner": null, "sender": null, "expires_at": now_s() + 600,
             "limit": {"workspace": null, "pane": "gone9"}},
            {"id": "pe1", "kind": "peer", "name": "laptop-bob", "scope": "full", "paired_at": 1,
             "owner": "teammate", "sender": null, "expires_at": null, "limit": null}
        ]
    })
}

/// The tab opened by `people`, its list answered (`gateway.status` left unanswered).
fn opened() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("people", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::People)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = gw_of(&cmds, "share.list");
    assert_eq!(p, json!({"method": "share.list", "params": {}}));
    assert!(cmds.iter().any(|c| c.1 == "gateway.status"), "{cmds:?}");
    reply(&mut app, 0, req, shares_json());
    (app, rxs)
}

fn stage(app: &App) -> &Stage {
    &app.ux.people.as_ref().unwrap().stage
}

fn form(app: &App) -> &Form {
    match stage(app) {
        Stage::Form(f) => f,
        s => panic!("not the form: {s:?}"),
    }
}

/// From the list: `n`, Enter, and the `share.create` answered with a link.
fn shared(app: &mut App, rxs: &mut [UnboundedReceiver<ClientFrame>]) {
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.create");
    reply(
        app,
        0,
        req,
        json!({"link": LINK, "pid": "pidN", "open_by": now_s() + 890,
               "expires_at": now_s() + 7200, "expires_after_s": 7200}),
    );
}

#[test]
fn the_list_shows_shares_with_names_access_and_expiry() {
    let (app, _rxs) = opened();
    let s = screen(&app);
    assert!(s.contains("Vibeke · Connections · m0"), "{s}");
    assert!(s.contains("Devices · People · Hosts · Handoffs"), "{s}");
    assert!(
        s.contains("People you shared a pane or workspace with"),
        "{s}"
    );
    assert!(
        s.contains("› Kim's browser · workspace api · View + approve · 8h left"),
        "{s}"
    );
    // About 10 minutes: the countdown rounds either way depending on the clock.
    assert!(
        s.contains("web · pane gone9 · View · 9m left")
            || s.contains("web · pane gone9 · View · 10m left"),
        "{s}"
    );
    assert!(
        s.contains("Sam · claude in api (w1:p1) · View · 2h left"),
        "{s}"
    );
    // Peers and handoffs are the Hosts tab's.
    assert!(!s.contains("laptop-bob"), "{s}");
    let v = app.ux.people.as_ref().unwrap();
    let ids: Vec<&str> = v.rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["sh1", "sh2", "pidS"]);
    assert!(v.rows[2].pending);
    let line = row_line(&app, 0, &v.rows[2], now_s() * 1000);
    assert!(
        line.starts_with(
            "Sam · claude in api (w1:p1) · View · 2h left · link not opened yet (open for "
        ),
        "{line}"
    );
}

#[test]
fn an_empty_list_says_how_to_share() {
    let (mut app, mut rxs) = fleet();
    commands(&mut rxs[0]);
    app.action("people", None);
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.list");
    reply(&mut app, 0, req, json!({"invitations": [], "devices": []}));
    assert!(screen(&app).contains("Nobody yet — press n to share this pane with a colleague"));
}

#[test]
fn n_shares_the_focused_pane_for_view_and_2h_by_default() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    let s = screen(&app);
    assert!(s.contains("Share with someone"), "{s}");
    assert!(s.contains("[This pane]"), "{s}");
    assert!(s.contains("claude in api (w1:p1)"), "{s}");
    assert!(s.contains("[View]"), "{s}");
    assert!(s.contains("[2h]"), "{s}");
    assert!(s.contains("(optional: who it's for)"), "{s}");
    let f = form(&app);
    assert_eq!(f.pane.as_deref(), Some("p1"));
    assert_eq!(f.workspace.as_deref(), Some("W1"));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = gw_of(&cmds, "share.create");
    assert_eq!(
        p,
        json!({"method": "share.create", "params": {"kind": "share", "scope": "view",
                                                     "pane": "p1", "ttl_s": 7200}})
    );
    assert!(screen(&app).contains("⏳ creating a share link…"));
}

#[test]
fn the_form_shares_the_workspace_with_approve_24h_and_a_name() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    // What → This workspace.
    app.on_key(named(NamedKey::Right));
    assert!(screen(&app).contains("workspace api"));
    // Access → View + approve.
    app.on_key(ch('j'));
    app.on_key(ch('l'));
    assert!(screen(&app).contains("[View + approve]"));
    // Expires → 24h (and no further than 7d).
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::Right));
    app.on_key(named(NamedKey::Right));
    assert_eq!(form(&app).ttl, 3);
    // Name: typed, j and k included; tab moves on inside the form there.
    app.on_key(ch('j'));
    assert!(typing(&app));
    typ(&mut app, "Kim jk");
    app.on_key(named(NamedKey::Backspace));
    app.on_key(named(NamedKey::Backspace));
    app.on_key(named(NamedKey::Backspace));
    typ(&mut app, " Lee");
    assert_eq!(form(&app).name, "Kim Lee");
    app.on_key(named(NamedKey::Tab));
    assert_eq!(form(&app).focus, Field::Create);
    assert!(matches!(app.mode, Mode::Popup(Popup::People)));
    app.on_key(named(NamedKey::Enter));
    let (_, p) = gw_of(&commands(&mut rxs[0]), "share.create");
    assert_eq!(
        p["params"],
        json!({"kind": "share", "scope": "approve", "workspace": "W1", "ttl_s": 86_400,
               "label": "Kim Lee"})
    );
}

#[test]
fn ctrl_u_and_paste_edit_the_name() {
    let (mut app, _rxs) = opened();
    app.on_key(ch('n'));
    for _ in 0..3 {
        app.on_key(named(NamedKey::Down));
    }
    assert_eq!(form(&app).focus, Field::Name);
    typ(&mut app, "x");
    app.on_key(ctl('u'));
    assert_eq!(form(&app).name, "");
    app.on_paste("Ann\nB".to_string());
    assert_eq!(form(&app).name, "Ann B");
}

#[test]
fn the_link_shows_with_a_qr_code_what_it_grants_and_copies() {
    let (mut app, mut rxs) = opened();
    shared(&mut app, &mut rxs);
    let s = screen(&app);
    assert!(s.contains(LINK), "{s}");
    assert!(
        s.contains("Anyone with this link can view claude in api (w1:p1) until "),
        "{s}"
    );
    assert!(
        s.contains("The link works once and must be opened within 14m."),
        "{s}"
    );
    assert!(
        s.contains('▀') || s.contains('▄') || s.contains('█'),
        "a QR code: {s}"
    );
    assert!(s.contains("c copy link"), "{s}");
    assert!(
        s.contains("Whoever opens this link first gets the access."),
        "{s}"
    );
    // The list is read again (the link waits there), and the gateway is asked about.
    let cmds = commands(&mut rxs[0]);
    gw_of(&cmds, "share.list");
    assert!(cmds.iter().any(|c| c.1 == "gateway.status"), "{cmds:?}");
    app.on_key(ch('c'));
    assert_eq!(
        app.clipboard_sink.as_ref().unwrap().last(),
        Some(&(LINK.as_bytes().to_vec(), false))
    );
    // Esc: back to the list; the link stays valid (nothing is revoked).
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(stage(&app), Stage::List));
    assert!(matches!(app.mode, Mode::Popup(Popup::People)));
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn an_approve_share_with_a_name_says_so() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(ch('j'));
    app.on_key(named(NamedKey::Space));
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    typ(&mut app, "Sam");
    app.on_key(named(NamedKey::Enter));
    let (req, p) = gw_of(&commands(&mut rxs[0]), "share.create");
    assert_eq!(p["params"]["scope"], "approve");
    assert_eq!(p["params"]["label"], "Sam");
    reply(
        &mut app,
        0,
        req,
        json!({"link": LINK, "pid": "pidN", "open_by": now_s() + 900,
               "expires_at": now_s() + 7 * 86_400}),
    );
    let s = screen(&app);
    assert!(s.contains("Share link for Sam"), "{s}");
    assert!(
        s.contains("Anyone with this link can view and answer the agent's questions"),
        "{s}"
    );
    assert!(s.contains(" for 6d") || s.contains(" for 7d"), "{s}");
}

#[test]
fn a_gateway_that_is_not_online_shows_no_link_or_qr() {
    let (mut app, mut rxs) = opened();
    shared(&mut app, &mut rxs);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "gateway.status");
    reply(
        &mut app,
        0,
        req,
        json!({"connected": true, "configured": true, "state": "offline"}),
    );
    let s = screen(&app);
    assert!(!s.contains(LINK), "{s}");
    assert!(!s.contains('▀') && !s.contains('█'), "no QR code: {s}");
    assert!(s.contains("The gateway is offline (state offline)"), "{s}");
    assert!(s.contains("your colleague can't use a link"), "{s}");
    assert!(!s.contains("c copy link"), "{s}");
    let copied = app.clipboard_sink.as_ref().unwrap().len();
    app.on_key(ch('c'));
    assert_eq!(app.clipboard_sink.as_ref().unwrap().len(), copied);
    // Asked again a little later; online again, the link shows.
    if let Some(v) = app.ux.people.as_mut() {
        v.gw_asked = Some(Instant::now() - 2 * POLL);
    }
    tick(&mut app);
    let (req, _) = only(&commands(&mut rxs[0]), "gateway.status");
    reply(
        &mut app,
        0,
        req,
        json!({"connected": true, "configured": true, "state": "online"}),
    );
    assert!(screen(&app).contains(LINK));
}

#[test]
fn revoke_and_cancel_ask_first_and_send_share_revoke() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('x'));
    assert!(
        screen(&app).contains("Revoke Kim's browser's access to workspace api?"),
        "{}",
        screen(&app)
    );
    // Anything but y / x / Enter keeps it.
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(matches!(stage(&app), Stage::List));
    app.on_key(ch('x'));
    app.on_key(ch('y'));
    let (req, p) = gw_of(&commands(&mut rxs[0]), "share.revoke");
    assert_eq!(
        p,
        json!({"method": "share.revoke", "params": {"id": "sh1"}})
    );
    reply(&mut app, 0, req, json!({"cancelled": "device"}));
    gw_of(&commands(&mut rxs[0]), "share.list");
    assert!(screen(&app).contains("revoked Kim's browser's access"));
    // The unused link.
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Cancel the unused link for Sam?"));
    app.on_key(named(NamedKey::Enter));
    let (req, p) = gw_of(&commands(&mut rxs[0]), "share.revoke");
    assert_eq!(p["params"], json!({"id": "pidS"}));
    reply(&mut app, 0, req, json!({"cancelled": "invitation"}));
    assert!(screen(&app).contains("cancelled the link for Sam"));
}

#[test]
fn an_older_gateway_refusing_shares_says_so_in_the_form() {
    const REFUSED: &str =
        "the server bridge carries share.create for handoff and peer invitations only";
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.create");
    reply_msg(&mut app, 0, req, "permission_denied", REFUSED);
    assert_eq!(form(&app).error.as_deref(), Some(REFUSED));
    assert!(screen(&app).contains("✗ the server bridge carries share.create"));
    // Not unreachable: no banner.
    assert!(app.ux.people.as_ref().unwrap().error.is_none());
}

#[test]
fn a_late_link_for_an_abandoned_form_is_cancelled() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.create");
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(stage(&app), Stage::List));
    reply(
        &mut app,
        0,
        req,
        json!({"link": LINK, "pid": "pidL", "open_by": now_s() + 900}),
    );
    let (_, p) = gw_of(&commands(&mut rxs[0]), "share.revoke");
    assert_eq!(p["params"], json!({"id": "pidL"}));
    assert!(matches!(stage(&app), Stage::List));
}

#[test]
fn esc_on_the_list_closes_the_view() {
    let (mut app, _rxs) = opened();
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.ux.people.is_none());
}

#[test]
fn control_sends_full_only_after_y() {
    let (mut app, mut rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(ch('j'));
    // Left from View wraps around to Control.
    app.on_key(named(NamedKey::Left));
    assert_eq!(form(&app).access, CONTROL);
    let s = screen(&app);
    assert!(s.contains("[Control]"), "{s}");
    assert!(
        s.contains("Control lets them type into this pane and prompt its agent"),
        "{s}"
    );
    app.on_key(named(NamedKey::Enter));
    assert!(form(&app).confirming);
    let s = screen(&app);
    assert!(
        s.contains("Give whoever opens the link control of claude in api (w1:p1) until "),
        "{s}"
    );
    assert!(commands(&mut rxs[0]).is_empty(), "nothing before y");
    // n and esc go back to the form without creating anything.
    app.on_key(ch('n'));
    assert!(!form(&app).confirming);
    app.on_key(named(NamedKey::Enter));
    app.on_key(named(NamedKey::Escape));
    assert!(!form(&app).confirming);
    assert!(commands(&mut rxs[0]).is_empty());
    // y creates it.
    app.on_key(named(NamedKey::Enter));
    app.on_key(ch('y'));
    let (req, p) = gw_of(&commands(&mut rxs[0]), "share.create");
    assert_eq!(
        p["params"],
        json!({"kind": "share", "scope": "full", "pane": "p1", "ttl_s": 7200})
    );
    reply(
        &mut app,
        0,
        req,
        json!({"link": LINK, "pid": "pidC", "open_by": now_s() + 890,
               "expires_at": now_s() + 7200}),
    );
    assert!(screen(&app).contains("Anyone with this link can control"));
}

#[test]
fn control_names_the_colleague_and_the_workspace() {
    let (mut app, _rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Right));
    app.on_key(ch('j'));
    app.on_key(ch('l'));
    app.on_key(ch('l'));
    assert!(
        screen(&app).contains("Control lets them type into the panes of this workspace"),
        "{}",
        screen(&app)
    );
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    typ(&mut app, "Kim");
    app.on_key(named(NamedKey::Enter));
    assert!(
        screen(&app).contains("Give Kim control of workspace api until "),
        "{}",
        screen(&app)
    );
}

#[test]
fn the_approve_warning_mentions_the_sandbox_unless_the_pane_runs_in_one() {
    let (mut app, _rxs) = opened();
    app.on_key(ch('n'));
    app.on_key(ch('j'));
    app.on_key(ch('l'));
    assert!(
        screen(&app).contains("They can say yes to what the agent asks"),
        "{}",
        screen(&app)
    );
    let (text, _) = access_text(&app, 0, form(&app));
    assert!(
        text.ends_with("(this pane's agent isn't sandboxed)"),
        "{text}"
    );
    app.machines[0].model.panes[0].isolation.level = vk_proto::model::IsolationLevel::Sandbox;
    let (text, _) = access_text(&app, 0, form(&app));
    assert_eq!(
        text,
        "They can say yes to what the agent asks — commands it runs as you"
    );
}
