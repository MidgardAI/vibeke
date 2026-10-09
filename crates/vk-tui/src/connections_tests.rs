//! Connections view tests: the title and tab strip, tab / shift+tab cycling (and leaving a tab
//! as Esc would), text fields keeping tab, `connections` reopening the last tab, the actions and
//! bindings that open each tab, and sharing a right-clicked pane.

use super::*;
use crate::app::Popup;
use crate::drafts::tests::{ch, commands, fleet, named, reply, screen};
use serde_json::{Value, json};
use vk_proto::input::Mods;

/// The tab the view shows, if it is open.
fn showing(app: &App) -> Option<Tab> {
    match &app.mode {
        Mode::Popup(Popup::Devices) => Some(Tab::Devices),
        Mode::Popup(Popup::People) => Some(Tab::People),
        Mode::Popup(Popup::Sharing) => Some(Tab::Hosts),
        Mode::Popup(Popup::Handoffs) => Some(Tab::Handoffs),
        _ => None,
    }
}

fn shift_tab() -> KeyEvent {
    KeyEvent::new(Key::Named(NamedKey::Tab), Mods::SHIFT)
}

fn prefix(app: &mut App, k: KeyEvent) {
    app.on_key(app.keymap.prefix.clone());
    app.on_key(k);
}

/// The one `gateway.call` for `method` among `cmds`.
fn gw_of(cmds: &[(u64, String, Value)], method: &str) -> u64 {
    let m: Vec<_> = cmds
        .iter()
        .filter(|c| c.1 == "gateway.call" && c.2["method"] == method)
        .collect();
    assert_eq!(m.len(), 1, "expected one gateway.call {method} in {cmds:?}");
    m[0].0
}

#[test]
fn tab_cycles_the_tabs_and_connections_reopens_the_last() {
    let (mut app, mut rxs) = fleet();
    app.action("connections", None);
    assert_eq!(showing(&app), Some(Tab::Devices));
    let s = screen(&app);
    assert!(s.contains("Vibeke · Connections · m0"), "{s}");
    assert!(s.contains("Devices · People · Hosts · Handoffs"), "{s}");
    commands(&mut rxs[0]);
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::People));
    assert!(app.ux.devices.is_none(), "the tab left behind is dropped");
    gw_of(&commands(&mut rxs[0]), "share.list");
    assert!(screen(&app).contains("People you shared a pane or workspace with"));
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::Hosts));
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::Handoffs));
    let s = screen(&app);
    assert!(s.contains("Vibeke · Connections · m0"), "{s}");
    assert!(s.contains("Handoffs — incoming and sent"), "{s}");
    // Around the end, and back.
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::Devices));
    app.on_key(shift_tab());
    assert_eq!(showing(&app), Some(Tab::Handoffs));
    app.on_key(shift_tab());
    assert_eq!(showing(&app), Some(Tab::Hosts));
    // Esc at a tab's top level closes the view; `connections` opens the last tab again.
    app.on_key(named(NamedKey::Escape));
    assert_eq!(showing(&app), None);
    app.action("connections", None);
    assert_eq!(showing(&app), Some(Tab::Hosts));
}

#[test]
fn leaving_devices_while_a_link_is_up_cancels_it() {
    let (mut app, mut rxs) = fleet();
    app.action("pair_phone", None);
    commands(&mut rxs[0]);
    app.on_key(named(NamedKey::Enter));
    let req = gw_of(&commands(&mut rxs[0]), "account.status");
    reply(&mut app, 0, req, json!({"needs_account": false}));
    let req = gw_of(&commands(&mut rxs[0]), "pair.create");
    reply(
        &mut app,
        0,
        req,
        json!({"link": "https://app.example/#/pair?d=abc", "pid": "pid42",
               "open_by": 4_000_000_000i64, "scope": "full"}),
    );
    commands(&mut rxs[0]);
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::People));
    let cmds = commands(&mut rxs[0]);
    let revoke: Vec<_> = cmds
        .iter()
        .filter(|c| c.1 == "gateway.call" && c.2["method"] == "share.revoke")
        .collect();
    assert_eq!(revoke.len(), 1, "{cmds:?}");
    assert_eq!(revoke[0].2["params"], json!({"id": "pid42"}));
}

#[test]
fn text_fields_keep_tab() {
    let (mut app, _rxs) = fleet();
    // The Hosts paste field: tab moves inside the form.
    app.action("hosts", None);
    app.on_key(ch('p'));
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::Hosts));
    let v = app.ux.sharing.view.as_ref().unwrap();
    assert_eq!(
        v.paste.as_ref().unwrap().focus,
        crate::sharing::PasteRow::ShareUser
    );
    app.on_key(named(NamedKey::Escape));
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::Handoffs));
    // The People form: tab switches tabs on a choice, moves on from the name field.
    app.action("share_pane", None);
    assert_eq!(showing(&app), Some(Tab::People));
    for _ in 0..3 {
        app.on_key(named(NamedKey::Down));
    }
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::People));
    match &app.ux.people.as_ref().unwrap().stage {
        crate::people::Stage::Form(f) => assert_eq!(f.focus, crate::people::Field::Create),
        s => panic!("{s:?}"),
    }
    app.on_key(named(NamedKey::Tab));
    assert_eq!(showing(&app), Some(Tab::Hosts));
}

#[test]
fn the_actions_open_their_tabs() {
    let (mut app, _rxs) = fleet();
    for (action, tab) in [
        ("devices", Tab::Devices),
        ("hosts", Tab::Hosts),
        ("handoffs", Tab::Handoffs),
        ("people", Tab::People),
        ("pair_phone", Tab::Devices),
    ] {
        app.mode = Mode::Normal;
        app.action(action, None);
        assert_eq!(showing(&app), Some(tab), "{action}");
        assert_eq!(app.ux.connections.last, tab, "{action}");
    }
    // `pair_phone`: straight to the scope picker.
    assert!(matches!(
        app.ux.devices.as_ref().unwrap().stage,
        crate::devices::Stage::PickScope { sel: 0 }
    ));
    // `share_pane`: the form for the focused pane.
    app.mode = Mode::Normal;
    app.action("share_pane", None);
    assert_eq!(showing(&app), Some(Tab::People));
    match &app.ux.people.as_ref().unwrap().stage {
        crate::people::Stage::Form(f) => {
            assert_eq!(f.pane.as_deref(), Some("p1"));
            assert_eq!(f.workspace.as_deref(), Some("W1"));
        }
        s => panic!("{s:?}"),
    }
    app.mode = Mode::Normal;
    app.action("connections", None);
    assert_eq!(showing(&app), Some(Tab::People));
}

#[test]
fn the_pane_menu_shares_the_right_clicked_pane() {
    let (mut app, _rxs) = fleet();
    crate::handoff::pane_menu(&mut app, 0, "p2");
    app.action("share_pane", None);
    assert_eq!(showing(&app), Some(Tab::People));
    match &app.ux.people.as_ref().unwrap().stage {
        crate::people::Stage::Form(f) => assert_eq!(f.pane.as_deref(), Some("p2")),
        s => panic!("{s:?}"),
    }
    assert!(screen(&app).contains("pane in api (w1:p2)"));
}

#[test]
fn bindings_open_connections_handoffs_and_the_share_form() {
    let (mut app, _rxs) = fleet();
    assert_eq!(
        app.keymap.binding_for("connections").as_deref(),
        Some("prefix+alt+d")
    );
    assert_eq!(
        app.keymap.binding_for("share_pane").as_deref(),
        Some("prefix+alt+v")
    );
    assert!(crate::nav::describe("connections").starts_with("Connections: devices, people"));
    prefix(&mut app, KeyEvent::new(Key::Char('d'), Mods::ALT));
    assert_eq!(showing(&app), Some(Tab::Devices));
    app.on_key(named(NamedKey::Escape));
    prefix(&mut app, KeyEvent::new(Key::Char('h'), Mods::SHIFT));
    assert_eq!(showing(&app), Some(Tab::Handoffs));
    app.on_key(named(NamedKey::Escape));
    prefix(&mut app, KeyEvent::new(Key::Char('v'), Mods::ALT));
    assert_eq!(showing(&app), Some(Tab::People));
    assert!(screen(&app).contains("Share with someone"));
}
