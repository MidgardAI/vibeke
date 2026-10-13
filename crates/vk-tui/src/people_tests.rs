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

// ---- spec 20: kinds, who may open, grouping --------------------------------------------------------

fn preview_of(handle: &str, pane: &str, label: &str) -> vk_proto::model::Preview {
    vk_proto::model::Preview {
        id: format!("PV-{handle}"),
        handle: handle.into(),
        machine: "local".into(),
        pane: Some(pane.into()),
        task: None,
        port: 5173,
        path: "/".into(),
        label: (!label.is_empty()).then(|| label.to_string()),
        url: "http://localhost:5173/".into(),
        scheme: "http".into(),
        status: vk_proto::model::PreviewStatus::Up,
        source: vk_proto::model::PreviewSource::Declared,
        pid: None,
        first_seen_ms: 0,
        last_seen_ms: 0,
    }
}

/// The form on its Kind field.
fn kind_form(app: &mut App) {
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Up));
    app.on_key(named(NamedKey::Up));
    assert_eq!(form(app).focus, Field::Kind);
}

fn with_previews(app: &mut App) {
    let m = &mut app.machines[0].model;
    m.panes.push(crate::drafts::tests::pane("p3", "T1", "W2"));
    m.previews = vec![
        preview_of("v4", "p1", "shop"),
        preview_of("v5", "p3", "other workspace"),
        {
            let mut gone = preview_of("v6", "p1", "old");
            gone.status = vk_proto::model::PreviewStatus::Gone;
            gone
        },
    ];
}

#[test]
fn a_design_review_lists_the_workspaces_previews_and_defaults_to_github() {
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    app.config.sharing.github = vec!["@alice".into(), "bob".into()];
    with_previews(&mut app);
    kind_form(&mut app);
    // Only the workspace's live preview is offered.
    assert_eq!(form(&app).previews.len(), 1);
    app.on_key(named(NamedKey::Right));
    let f = form(&app);
    assert_eq!(f.kind, Kind::Review);
    assert_eq!(f.who, Who::Github);
    assert_eq!(f.notes, Notes::Direct);
    let s = screen(&app);
    assert!(s.contains("Share a design review"), "{s}");
    assert!(s.contains("[Design review]"), "{s}");
    assert!(s.contains("[v4 shop]"), "{s}");
    assert!(s.contains("[GitHub]"), "{s}");
    assert!(
        s.contains("only @alice, @bob, signed in with GitHub"),
        "{s}"
    );
    assert!(s.contains("[Straight to the agent]"), "{s}");
    assert!(!s.contains("This pane"), "{s}");
    app.on_key(named(NamedKey::Enter));
    let (_, p) = gw_of(&commands(&mut rxs[0]), "share.create");
    assert_eq!(
        p["params"],
        json!({"kind": "review", "preview": "v4", "notes": "direct", "access": "github",
               "github": ["alice", "bob"], "ttl_s": 7200})
    );
}

#[test]
fn a_plain_link_review_holds_notes_unless_told_otherwise() {
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    app.config.sharing.github = vec!["alice".into()];
    with_previews(&mut app);
    kind_form(&mut app);
    app.on_key(named(NamedKey::Right));
    // Who → Anyone with the link: notes follow to Hold for me.
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::Left));
    assert_eq!(form(&app).who, Who::Link);
    assert_eq!(form(&app).notes, Notes::Held);
    assert!(screen(&app).contains("[Hold for me]"));
    // Picking Notes by hand keeps it when Who changes again.
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::Down));
    assert_eq!(form(&app).focus, Field::Notes);
    app.on_key(named(NamedKey::Right));
    assert_eq!(form(&app).notes, Notes::Direct);
    app.on_key(named(NamedKey::Up));
    app.on_key(named(NamedKey::Up));
    app.on_key(named(NamedKey::Right));
    assert_eq!(form(&app).who, Who::Github);
    assert_eq!(form(&app).notes, Notes::Direct);
    app.on_key(named(NamedKey::Left));
    app.on_key(named(NamedKey::Enter));
    let (_, p) = gw_of(&commands(&mut rxs[0]), "share.create");
    assert_eq!(
        p["params"],
        json!({"kind": "review", "preview": "v4", "notes": "direct", "access": "link",
               "ttl_s": 7200})
    );
}

#[test]
fn github_is_off_without_a_configured_list_but_any_account_works() {
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    with_previews(&mut app);
    kind_form(&mut app);
    app.on_key(named(NamedKey::Right));
    // No list: a review starts as a plain link.
    assert_eq!(form(&app).who, Who::Link);
    let s = screen(&app);
    assert!(s.contains("GitHub (off)"), "{s}");
    assert!(s.contains("set [sharing] github in your config"), "{s}");
    // Right from Link skips GitHub.
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::Right));
    assert_eq!(form(&app).who, Who::Any);
    assert!(screen(&app).contains("[Any GitHub account]"));
    app.on_key(named(NamedKey::Enter));
    let (_, p) = gw_of(&commands(&mut rxs[0]), "share.create");
    assert_eq!(p["params"]["access"], "github");
    assert_eq!(p["params"]["github"], json!(["*"]));
    assert_eq!(p["params"]["notes"], "direct");
}

#[test]
fn a_review_without_a_preview_says_what_to_do() {
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    kind_form(&mut app);
    app.on_key(named(NamedKey::Right));
    assert!(screen(&app).contains("no preview in this workspace"));
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(
        form(&app)
            .error
            .as_deref()
            .unwrap()
            .contains("no preview in this workspace")
    );
}

#[test]
fn a_watch_share_for_github_accounts_names_them() {
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    app.config.sharing.github = vec!["alice".into()];
    app.on_key(ch('n'));
    // What → Who is above it.
    app.on_key(named(NamedKey::Up));
    app.on_key(named(NamedKey::Right));
    assert_eq!(form(&app).who, Who::Github);
    assert!(screen(&app).contains("only @alice, signed in with GitHub"));
    app.on_key(named(NamedKey::Enter));
    let (_, p) = gw_of(&commands(&mut rxs[0]), "share.create");
    assert_eq!(
        p["params"],
        json!({"kind": "share", "scope": "view", "pane": "p1", "ttl_s": 7200,
               "access": "github", "github": ["alice"]})
    );
}

#[test]
fn the_created_review_link_shows_who_can_open_it_and_the_uses_left() {
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    app.config.sharing.github = vec!["alice".into(), "bob".into()];
    with_previews(&mut app);
    kind_form(&mut app);
    app.on_key(named(NamedKey::Right));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.create");
    reply(
        &mut app,
        0,
        req,
        json!({"link": LINK, "pid": "pidR", "open_by": now_s() + 890,
               "expires_at": now_s() + 7200, "access": "github", "uses": 20,
               "github": [{"id": 1, "login": "alice"}, {"id": 2, "login": "bob"}],
               "notes": "direct", "preview": {"id": "PV-v4", "handle": "v4"}}),
    );
    let s = screen(&app);
    assert!(s.contains("Design review link"), "{s}");
    assert!(
        s.contains("Only @alice, @bob can see and leave notes on"),
        "{s}"
    );
    assert!(s.contains("preview v4 shop"), "{s}");
    assert!(s.contains("Their notes go straight to the agent."), "{s}");
    assert!(s.contains("Uses left: 20"), "{s}");
    assert!(s.contains("Revoke it in People when done."), "{s}");
    assert!(!s.contains("works once"), "{s}");
    assert!(s.contains(LINK), "{s}");
}

#[test]
fn a_held_review_link_says_notes_wait() {
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    with_previews(&mut app);
    kind_form(&mut app);
    app.on_key(named(NamedKey::Right));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.create");
    reply(
        &mut app,
        0,
        req,
        json!({"link": LINK, "pid": "pidR", "open_by": now_s() + 890, "uses": 1}),
    );
    let s = screen(&app);
    assert!(
        s.contains("Anyone with this link can see and leave notes on"),
        "{s}"
    );
    assert!(
        s.contains("Their notes wait for you: send or discard each one."),
        "{s}"
    );
    assert!(
        s.contains("The link works once and must be opened within"),
        "{s}"
    );
}

#[test]
fn account_errors_read_as_sentences_in_the_form() {
    for (kind, text) in [
        ("account_required", "vibeke login"),
        ("unavailable", "account server"),
    ] {
        let (mut app, mut rxs) = opened();
        app.size = (140, 40);
        app.on_key(ch('n'));
        app.on_key(named(NamedKey::Enter));
        let (req, _) = gw_of(&commands(&mut rxs[0]), "share.create");
        reply_msg(&mut app, 0, req, kind, "raw");
        let e = form(&app).error.clone().unwrap();
        assert!(e.contains(text), "{kind}: {e}");
    }
    let (mut app, mut rxs) = opened();
    app.size = (140, 40);
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.create");
    let json = json!({"jsonrpc": "2.0", "id": req, "error": {"code": -32000, "message": "raw",
        "data": {"kind": "unknown_user", "details": {"login": "ghost"}}}})
    .to_string();
    app.on_frame(0, ServerFrame::CommandResult { req, json });
    assert_eq!(
        form(&app).error.as_deref(),
        Some("@ghost is not a GitHub user: check the login")
    );
}

fn grouped_json() -> Value {
    let rv = json!({"preview": "PV-v4", "handle": "v4", "label": "shop"});
    json!({
        "invitations": [
            {"id": "pidR", "kind": "share", "scope": "view", "label": "Review for Kim",
             "limit": null, "created": 1, "link_expires_at": now_s() + 600,
             "device_expires_at": now_s() + 7200 + 120, "access": "github", "uses": 20,
             "redeemed": 2, "review": rv}
        ],
        "devices": [
            {"id": "dv1", "kind": "share", "name": "Kim's laptop", "scope": "view",
             "expires_at": now_s() + 3600 + 120, "limit": null,
             "github": {"id": 5, "login": "kim"}, "invitation": "pidR"},
            {"id": "dv3", "kind": "share", "name": "web", "scope": "view",
             "expires_at": now_s() + 600, "limit": {"pane": "p1"}},
            {"id": "dv2", "kind": "review", "name": "Lee's phone", "scope": "view",
             "expires_at": now_s() + 3600 + 120, "limit": null, "github": "lee",
             "invitation": "pidR"},
            {"id": "dv4", "kind": "share", "name": "orphan", "scope": "view",
             "expires_at": now_s() + 600, "limit": null, "invitation": "gone"}
        ]
    })
}

#[test]
fn devices_group_under_their_invitation_with_their_github_login() {
    let (mut app, mut rxs) = fleet();
    app.size = (140, 40);
    commands(&mut rxs[0]);
    app.action("people", None);
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.list");
    reply(&mut app, 0, req, grouped_json());
    let v = app.ux.people.as_ref().unwrap();
    let order: Vec<(&str, bool)> = v.rows.iter().map(|r| (r.id.as_str(), r.child)).collect();
    // Devices without a live invitation first, then each invitation with its devices.
    assert_eq!(
        order,
        [
            ("dv3", false),
            ("dv4", false),
            ("pidR", false),
            ("dv1", true),
            ("dv2", true)
        ]
    );
    let s = screen(&app);
    assert!(s.contains("Kim's laptop (@kim)"), "{s}");
    assert!(s.contains("Lee's phone (@lee)"), "{s}");
    assert!(s.contains("└ Kim's laptop"), "{s}");
    let line = row_line(
        &app,
        0,
        &app.ux.people.as_ref().unwrap().rows[2],
        now_s() * 1000,
    );
    assert_eq!(
        line,
        "Review for Kim · design review of v4 shop · Review · 2h left · link opened 2x · 18 uses left"
    );
}

#[test]
fn revoking_an_invitation_revokes_its_devices_with_it() {
    let (mut app, mut rxs) = fleet();
    app.size = (140, 40);
    commands(&mut rxs[0]);
    app.action("people", None);
    let (req, _) = gw_of(&commands(&mut rxs[0]), "share.list");
    reply(&mut app, 0, req, grouped_json());
    // dv3, dv4, pidR.
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    app.on_key(ch('x'));
    let s = screen(&app);
    assert!(
        s.contains("Revoke the link for Review for Kim and the 2 devices that opened it?"),
        "{s}"
    );
    app.on_key(ch('y'));
    let (_, p) = gw_of(&commands(&mut rxs[0]), "share.revoke");
    assert_eq!(p["params"], json!({"id": "pidR"}));
}

#[test]
fn group_rows_keeps_the_order_of_a_list_without_invitations_for_its_devices() {
    let row = |id: &str, pending: bool, inv: Option<&str>| ShareRow {
        id: id.into(),
        pending,
        who: String::new(),
        scope: "view".into(),
        workspace: None,
        pane: None,
        expires_at: None,
        link_expires_at: None,
        github: None,
        invitation: inv.map(str::to_string),
        uses_left: None,
        redeemed: 0,
        review: None,
        child: false,
    };
    let out = group_rows(vec![
        row("d2", false, Some("i2")),
        row("d1", false, None),
        row("i1", true, None),
        row("i2", true, None),
        row("d3", false, Some("i2")),
    ]);
    let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["d1", "i1", "i2", "d2", "d3"]);
    assert_eq!(
        out.iter().map(|r| r.child).collect::<Vec<_>>(),
        [false, false, false, true, true]
    );
}
