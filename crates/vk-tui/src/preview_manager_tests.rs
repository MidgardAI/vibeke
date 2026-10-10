//! Preview manager popup (rows, markers, filter, keys → RPCs on the right machine, mirror and
//! forget confirmations, plain URLs only, the browser banner and install) and the sidebar's
//! expandable preview rows (chips, hit testing, navigate mode).

use super::*;
use crate::app::App;
use crate::drafts::tests::{ch, commands, fleet_n, named, only, reply, screen, typ};
use crossterm::event::{KeyModifiers, MouseEvent as CtMouse};
use vk_proto::model::Pane;
use vk_proto::render::{BrowserStatus, ClientFrame};

fn preview(handle: &str, port: u16, pane: &str, status: &str) -> Preview {
    serde_json::from_value(json!({
        "id": format!("PV-{handle}"), "handle": handle, "machine": "m", "pane": pane,
        "task": null, "port": port, "path": "/", "label": format!("site-{handle}"),
        "url": format!("http://localhost:{port}/"), "scheme": "http", "status": status,
        "source": "declared", "pid": null, "first_seen_ms": 0, "last_seen_ms": 0
    }))
    .unwrap()
}

fn browser_pane(id: &str, preview: &str) -> Pane {
    let mut p = crate::drafts::tests::pane(id, "T1", "W1");
    p.browser = serde_json::from_value(json!({
        "url": "http://localhost:8799/", "machine": "", "task": null, "preview": preview,
        "source_pane": "p1", "history": [], "history_index": 0, "title": ""
    }))
    .unwrap();
    p
}

/// Local m0 has `v3` (on the focused pane); remote m1 has `v14` (shown in a browser pane), a
/// down `v7` and a gone `v9`. m0 is focused.
fn app2() -> (App, Vec<tokio::sync::mpsc::UnboundedReceiver<ClientFrame>>) {
    let (mut app, mut rxs) = fleet_n(2);
    app.caps.kitty_graphics = true;
    app.machines[0].model.previews = vec![preview("v3", 5173, "p1", "up")];
    app.machines[1].model.previews = vec![
        preview("v14", 8799, "p1", "up"),
        preview("v7", 3000, "p2", "down"),
        preview("v9", 4000, "p2", "gone"),
    ];
    app.machines[1]
        .model
        .panes
        .push(browser_pane("bp", "PV-v14"));
    for rx in &mut rxs {
        commands(rx);
    }
    (app, rxs)
}

fn status(pane: Value) -> Value {
    json!({
        "mirrors": [{"machine": "m1", "preview": "PV-v14", "preview_handle": "v14", "local_port": 8799}],
        "proxy": {"port": 47800, "tls": false, "routes": [
            {"host": "v3-abc.vibeke.localhost", "machine": "local", "preview": "PV-v3", "handle": "v3",
             "port": 5173, "scheme": "http", "tls": false}
        ], "stats": {}},
        // Never shown or copied, even if a server sent one.
        "open_url": "http://v3-abc.vibeke.localhost:47800/?vk_token=SECRET",
        "available_browsers": {"pane": pane, "window": {"binary": "/x/chrome", "kind": "chrome"}},
        "browser_install": null,
    })
}

/// Open the popup and answer its `preview.status` poll on m0.
fn open_with(
    app: &mut App,
    rxs: &mut [tokio::sync::mpsc::UnboundedReceiver<ClientFrame>],
    st: Value,
) {
    app.action("preview_list", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Previews)));
    let c = commands(&mut rxs[0]);
    let (req, _) = only(&c, "preview.status");
    reply(app, 0, req, st);
}

fn names(app: &App) -> Vec<String> {
    filtered(app).into_iter().map(|r| r.name).collect()
}

fn select(app: &mut App, name: &str) {
    app.preview_mgr.sel = names(app).iter().position(|n| n == name).unwrap();
}

#[test]
fn rows_markers_order_and_detail() {
    let (mut app, mut rxs) = app2();
    open_with(
        &mut app,
        &mut rxs,
        status(json!({"binary": "/b/shell", "kind": "installed"})),
    );
    // Gone previews are left out; the focused workspace's first, then machine and handle.
    assert_eq!(names(&app), vec!["m0/v3", "m1/v7", "m1/v14"]);
    let rows = filtered(&app);
    assert_eq!(
        rows[0].proxy.as_deref(),
        Some("http://v3-abc.vibeke.localhost:47800/")
    );
    assert!(!rows[0].pane_open && rows[0].mirror.is_none());
    assert_eq!(rows[0].owner, "w1:p1 zsh");
    assert!(rows[2].pane_open);
    assert_eq!(rows[2].mirror, Some(8799));
    assert!(rows[2].proxy.is_none());
    // The mirror list came with the status.
    assert_eq!(app.browser.mirrors.len(), 1);
    let s = screen(&app);
    assert!(
        s.contains("Previews") && s.contains("3 · 1 mirrored"),
        "{s}"
    );
    assert!(
        s.contains("◎ proxy") && s.contains("▣ pane") && s.contains("⇄ :8799"),
        "{s}"
    );
    assert!(s.contains("(down "), "{s}");
    assert!(
        s.contains("browser on m0: installed · window: chrome"),
        "{s}"
    );
    // Detail of the selected (first) row: URL, machine, proxy origin without a token.
    assert!(s.contains("http://localhost:5173/  ·  m0  ·  up"), "{s}");
    assert!(
        s.contains("proxy  http://v3-abc.vibeke.localhost:47800/"),
        "{s}"
    );
    assert!(!s.contains("vk_token") && !s.contains("SECRET"), "{s}");
    // The mirrored remote one shows its warning.
    select(&mut app, "m1/v14");
    let s = screen(&app);
    assert!(
        s.contains("mirror localhost:8799  ⚠ unauthenticated"),
        "{s}"
    );
}

#[test]
fn labels_are_escaped() {
    let (mut app, mut rxs) = app2();
    app.machines[0].model.previews[0].label = Some("evil\x1b[2Jlabel".into());
    open_with(&mut app, &mut rxs, status(Value::Null));
    let r = &filtered(&app)[0];
    assert!(!r.label.contains('\x1b'), "{:?}", r.label);
}

#[test]
fn filter_keeps_matching_rows() {
    let (mut app, mut rxs) = app2();
    open_with(&mut app, &mut rxs, status(Value::Null));
    app.on_key(ch('/'));
    typ(&mut app, "v14");
    assert_eq!(names(&app), vec!["m1/v14"]);
    app.on_key(named(NamedKey::Enter));
    assert!(!app.preview_mgr.typing);
    // Esc clears the filter first, then closes.
    app.on_key(named(NamedKey::Escape));
    assert_eq!(names(&app).len(), 3);
    assert!(matches!(app.mode, Mode::Popup(Popup::Previews)));
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn polls_while_open_only() {
    let (mut app, mut rxs) = app2();
    let now = Instant::now();
    assert!(!app.deadlines(now).names().contains(&"previews.poll"));
    open_with(&mut app, &mut rxs, status(Value::Null));
    let d = app.deadlines(now);
    let at = d.get("previews.poll").unwrap();
    assert!(at > now && at <= now + POLL + Duration::from_millis(50));
    // Due: the next tick asks again.
    app.preview_mgr.polled = Some(Instant::now() - POLL);
    tick(&mut app);
    only(&commands(&mut rxs[0]), "preview.status");
    app.on_key(ch('q'));
    assert!(!app.deadlines(now).names().contains(&"previews.poll"));
    app.preview_mgr.polled = Some(Instant::now() - POLL);
    tick(&mut app);
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn keys_send_the_right_calls_to_the_right_machine() {
    let (mut app, mut rxs) = app2();
    open_with(&mut app, &mut rxs, status(Value::Null));
    select(&mut app, "m1/v7");
    // Window and proxy go through the local server, naming the remote preview.
    app.on_key(ch('w'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.open");
    assert_eq!(p, json!({"preview": "m1/v7", "window": true}));
    app.on_key(ch('p'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.open");
    assert_eq!(p["proxy"], true);
    assert_eq!(p["no_open"], true);
    assert_eq!(p["preview"], "m1/v7");
    assert!(matches!(app.mode, Mode::Popup(Popup::Previews)));
    // Mirroring asks first; `n` sends nothing, `y` mirrors.
    app.on_key(ch('m'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("unauthenticated port"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('m'));
    app.on_key(ch('y'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.mirror");
    assert_eq!(p["preview"], "m1/v7");
    // A mirrored one unmirrors at once.
    select(&mut app, "m1/v14");
    app.on_key(ch('m'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.unmirror");
    assert_eq!(p["port"], 8799);
    // Forget asks, then goes through the local server.
    app.on_key(ch('d'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('y'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.forget");
    assert_eq!(p, json!({"preview": "m1/v14"}));
    // A local preview: no machine prefix in the target, and mirroring is refused at once.
    select(&mut app, "m0/v3");
    app.on_key(ch('m'));
    assert!(app.preview_mgr.confirm.is_none());
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('w'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.open");
    assert_eq!(p["preview"], "v3");
    // Enter opens a browser pane on the preview's own machine and closes the popup.
    select(&mut app, "m1/v14");
    app.on_key(named(NamedKey::Enter));
    assert!(matches!(app.mode, Mode::Normal));
    let (_, p) = only(&commands(&mut rxs[1]), "browser.pane.create");
    assert_eq!(p["preview"], "PV-v14");
    assert_eq!(p["pane"], "p1");
}

#[test]
fn go_to_pane_and_copy_the_plain_url() {
    let (mut app, mut rxs) = app2();
    open_with(&mut app, &mut rxs, status(Value::Null));
    select(&mut app, "m0/v3");
    app.on_key(ch('y'));
    let sink = app.clipboard_sink.clone().unwrap();
    assert_eq!(sink[0].0, b"http://localhost:5173/".to_vec());
    assert!(
        sink.iter()
            .all(|(d, _)| !String::from_utf8_lossy(d).contains("vk_token"))
    );
    // g focuses the owning pane on its machine.
    select(&mut app, "m1/v7");
    app.on_key(ch('g'));
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.cur, 1);
}

#[test]
fn banner_and_install_when_the_media_host_has_no_browser() {
    let (mut app, mut rxs) = app2();
    open_with(&mut app, &mut rxs, status(Value::Null));
    let s = screen(&app);
    assert!(
        s.contains("✗ no browser on m0 for browser panes: run `vibeke browser install` there"),
        "{s}"
    );
    assert!(s.contains("· i install"), "{s}");
    // `i` asks; `y` starts a background install on the media host.
    app.on_key(ch('i'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("about 100 MB"));
    app.on_key(ch('y'));
    let c = commands(&mut rxs[0]);
    let (req, p) = only(&c, "browser.install");
    assert_eq!(p, json!({"confirm": true, "background": true}));
    reply(
        &mut app,
        0,
        req,
        json!({"started": true, "running": true, "plan": {}}),
    );
    assert_eq!(app.preview_mgr.installing, Some(0));
    assert!(screen(&app).contains("installing a browser on m0"));
    // Closed, it keeps polling until the install is done, then says how it went.
    app.on_key(ch('q'));
    assert!(
        app.deadlines(Instant::now())
            .names()
            .contains(&"previews.poll")
    );
    let mut st = status(Value::Null);
    st["browser_install"] = json!({"running": false, "binary": null, "error": "checksum mismatch"});
    on_status(&mut app, 0, st);
    assert!(app.preview_mgr.installing.is_none());
    let shown: String = app.toasts.iter().map(|t| t.text.clone()).collect();
    assert!(
        shown.contains("browser install on m0 failed: checksum mismatch"),
        "{shown}"
    );
    // A refused install (no recorded checksum) is toasted.
    app.action("preview_list", None);
    app.on_key(ch('i'));
    app.on_key(ch('y'));
    let c = commands(&mut rxs[0]);
    let (req, _) = only(&c, "browser.install");
    crate::drafts::tests::reply_err(&mut app, 0, req, "invalid_params", Value::Null);
    let shown: String = app.toasts.iter().map(|t| t.text.clone()).collect();
    assert!(
        shown.contains("✗ browser install on m0: invalid_params!"),
        "{shown}"
    );
}

#[test]
fn an_older_server_shows_no_banner() {
    let (mut app, mut rxs) = app2();
    let mut st = status(Value::Null);
    st.as_object_mut().unwrap().remove("available_browsers");
    open_with(&mut app, &mut rxs, st);
    assert_eq!(pane_browser(&app), None);
    assert!(!screen(&app).contains("no browser on"));
    app.on_key(ch('i'));
    assert!(app.preview_mgr.confirm.is_none());
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn mouse_selects_and_double_click_opens() {
    let (mut app, mut rxs) = app2();
    open_with(&mut app, &mut rxs, status(Value::Null));
    let g = geo(&app, 3);
    let click = |row: u16| CtMouse {
        kind: MouseEventKind::Down(CtButton::Left),
        column: g.r.x + 5,
        row,
        modifiers: KeyModifiers::NONE,
    };
    assert!(on_mouse(&mut app, &click(g.list_y + 2)));
    assert_eq!(app.preview_mgr.sel, 2);
    assert!(commands(&mut rxs[1]).is_empty());
    assert!(on_mouse(&mut app, &click(g.list_y + 2)));
    assert!(matches!(app.mode, Mode::Normal));
    let (_, p) = only(&commands(&mut rxs[1]), "browser.pane.create");
    assert_eq!(p["preview"], "PV-v14");
}

#[test]
fn no_browser_errors_name_the_machine_and_the_fix() {
    let (mut app, _rxs) = app2();
    let st = BrowserStatus {
        error: Some(
            "no Chromium found for the browser pane on this machine: run `vibeke browser install` here"
                .into(),
        ),
        ..Default::default()
    };
    crate::browser::on_state(&mut app, 0, "bp".into(), st.clone());
    crate::browser::on_state(&mut app, 0, "bp".into(), st);
    let hits: Vec<String> = app
        .toasts
        .iter()
        .map(|t| t.text.clone())
        .filter(|t| t.contains("no browser on m0"))
        .collect();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].contains("vibeke browser install") && hits[0].contains("prefix+shift+o"));
    // A window with no browser on the viewing machine.
    crate::browser::on_reply(
        &mut app,
        0,
        Reply::Window {
            machine: "m0".into(),
            handle: "v3".into(),
        },
        Err(crate::app::RpcErr {
            kind: "unsupported".into(),
            message: "no Chromium-family browser found".into(),
            details: Value::Null,
        }),
    );
    let shown: String = app.toasts.iter().map(|t| t.text.clone()).collect();
    assert!(
        shown.contains("no browser on m0 for preview windows"),
        "{shown}"
    );
}

// ---- sidebar ------------------------------------------------------------------------------

fn row_of(app: &App, mi: usize, id: &str) -> u16 {
    let rows = crate::draw::sidebar_rows(app);
    rows.iter()
        .position(|r| r.preview == Some((mi, id.to_string())) && r.chips.is_empty())
        .unwrap() as u16
        + 1
}

fn left(x: u16, y: u16) -> CtMouse {
    CtMouse {
        kind: MouseEventKind::Down(CtButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn sidebar_row_expands_into_chips() {
    let (mut app, mut rxs) = app2();
    app.sidebar = true;
    let sx = crate::chrome::sidebar_x(&app).unwrap();
    let y3 = row_of(&app, 0, "PV-v3");
    // A click expands (nothing is opened), a second click elsewhere later collapses.
    assert!(crate::browser::on_mouse(&mut app, &left(sx + 3, y3), None));
    assert_eq!(app.preview_mgr.expanded, Some((0, "PV-v3".into())));
    assert!(commands(&mut rxs[0]).is_empty());
    let rows = crate::draw::sidebar_rows(&app);
    let chips: Vec<Chip> = rows
        .iter()
        .flat_map(|r| r.chips.iter().map(|c| c.0))
        .collect();
    assert_eq!(
        chips,
        vec![
            Chip::Pane,
            Chip::Window,
            Chip::Proxy,
            Chip::Mirror,
            Chip::Copy
        ]
    );
    // Chips wrap to the sidebar width.
    let w = app.sidebar_w.saturating_sub(1);
    assert!(rows.iter().all(|r| r.chips.iter().all(|c| c.2 <= w)));
    // Rows after the chips still map to their previews.
    let y14 = row_of(&app, 1, "PV-v14");
    assert_eq!(
        crate::browser::preview_hit(&app, y14).map(|(m, p)| (m, p.id)),
        Some((1, "PV-v14".to_string()))
    );
    assert!(crate::browser::preview_hit(&app, y3 + 1).is_none());
    // Click the [window] chip.
    let (cy, cx) = rows
        .iter()
        .enumerate()
        .find_map(|(i, r)| {
            r.chips
                .iter()
                .find(|c| c.0 == Chip::Window)
                .map(|c| (i as u16 + 1, c.1))
        })
        .unwrap();
    assert_eq!(
        chip_hit(&app, cx, cy).map(|(m, p, c)| (m, p.id, c)),
        Some((0, "PV-v3".to_string(), Chip::Window))
    );
    assert!(crate::browser::on_mouse(&mut app, &left(sx + cx, cy), None));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.open");
    assert_eq!(p, json!({"preview": "v3", "window": true}));
    // Another row: only one is expanded at a time.
    app.preview_mgr.side_click = None;
    crate::browser::on_mouse(&mut app, &left(sx + 3, y14), None);
    assert_eq!(app.preview_mgr.expanded, Some((1, "PV-v14".into())));
    // The preview going away collapses it.
    app.machines[1].model.previews.retain(|p| p.id != "PV-v14");
    tick(&mut app);
    assert!(app.preview_mgr.expanded.is_none());
}

#[test]
fn sidebar_double_click_opens_and_mirror_chip_asks() {
    let (mut app, mut rxs) = app2();
    app.sidebar = true;
    let sx = crate::chrome::sidebar_x(&app).unwrap();
    let y7 = row_of(&app, 1, "PV-v7");
    crate::browser::on_mouse(&mut app, &left(sx + 3, y7), None);
    crate::browser::on_mouse(&mut app, &left(sx + 3, y7), None);
    let (_, p) = only(&commands(&mut rxs[1]), "browser.pane.create");
    assert_eq!(p["preview"], "PV-v7");
    assert!(app.preview_mgr.expanded.is_none());
    // [mirror] on a remote preview asks with a confirm popup.
    let p7 = app.machines[1].model.previews[1].clone();
    run_chip(&mut app, 1, &p7, Chip::Mirror);
    assert!(commands(&mut rxs[0]).is_empty());
    match &app.mode {
        Mode::Popup(Popup::Confirm { message, .. }) => {
            assert!(message.contains("unauthenticated"), "{message}")
        }
        _ => panic!("no confirmation"),
    }
    app.on_key(ch('y'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.mirror");
    assert_eq!(p["preview"], "m1/v7");
    // [copy] copies the plain URL.
    run_chip(&mut app, 1, &p7, Chip::Copy);
    let sink = app.clipboard_sink.clone().unwrap();
    assert_eq!(sink.last().unwrap().0, b"http://localhost:3000/".to_vec());
}

#[test]
fn navigate_mode_enter_expands_and_esc_collapses() {
    let (mut app, mut rxs) = app2();
    app.sidebar = true;
    let targets = crate::draw::sidebar_targets(&app);
    let sel = targets
        .iter()
        .position(|t| *t == (0, "PV-v3".to_string()))
        .unwrap();
    app.mode = Mode::Navigate { sel };
    app.on_key(named(NamedKey::Enter));
    assert_eq!(app.preview_mgr.expanded, Some((0, "PV-v3".into())));
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    // Pane keys are swallowed on a preview row (nothing closes, nothing is sent).
    app.on_key(ch('x'));
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(named(NamedKey::Escape));
    assert!(app.preview_mgr.expanded.is_none());
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    // `w` opens a window from navigate mode.
    app.on_key(ch('w'));
    let (_, p) = only(&commands(&mut rxs[0]), "preview.open");
    assert_eq!(p["preview"], "v3");
}
