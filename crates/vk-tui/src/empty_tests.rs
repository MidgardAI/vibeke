//! Leaving the empty screen without a prefix, including decoded host input.

use super::*;
use crate::drafts::tests::{ctl, fleet, screen};

fn empty(app: &mut App) {
    let m = app.m_mut();
    m.model.panes.clear();
    m.model.tabs.clear();
    m.model.runs.clear();
    m.focus.tab = None;
    m.focus.pane = None;
}

#[test]
fn ctrl_c_detaches_after_the_last_pane_closes() {
    let (mut app, mut rx) = fleet();
    empty(&mut app);
    let mut decoder = crate::input::Decoder::default();
    for input in decoder.feed(b"\x03") {
        app.on_input(input);
    }
    assert_eq!(app.quit.as_deref(), Some("detached"));
    assert!(matches!(app.mode, Mode::Normal));
    while let Ok(frame) = rx[0].try_recv() {
        // Host input reports presence, but exiting must not mutate panes or the server.
        let ClientFrame::Command { json, .. } = frame else {
            panic!("unexpected input sent without a pane: {frame:?}");
        };
        let command: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(command["method"], "client.activity");
    }
}

#[test]
fn ctrl_c_still_reaches_a_focused_pane() {
    let (mut app, mut rx) = fleet();
    app.on_key(ctl('c'));
    assert!(app.quit.is_none());
    assert!(matches!(
        rx[0].try_recv().unwrap(),
        ClientFrame::Key { pane, key, .. } if pane == "p1" && key == ctl('c')
    ));
}

#[test]
fn empty_exit_works_with_an_unbound_detach_and_an_open_prefix_menu() {
    let (mut app, _rx) = fleet();
    empty(&mut app);
    app.config.keys.bindings.insert("detach".into(), "".into());
    app.keymap = Keymap::from_config(&app.config);
    app.mode = Mode::Prefix(PrefixState::menu());
    app.on_key(ctl('c'));
    assert_eq!(app.quit.as_deref(), Some("detached"));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn ctrl_c_release_and_modified_keys_do_not_detach() {
    let (mut app, _rx) = fleet();
    empty(&mut app);
    let mut release = ctl('c');
    release.kind = KeyKind::Release;
    for ev in [
        release,
        KeyEvent::ch('c'),
        KeyEvent::new(Key::Char('c'), Mods::CTRL | Mods::ALT),
        KeyEvent::new(Key::Char('c'), Mods::CTRL | Mods::SHIFT),
    ] {
        app.on_key(ev);
        assert!(app.quit.is_none());
    }
}

#[test]
fn ctrl_c_does_not_exit_from_a_prompt() {
    let (mut app, _rx) = fleet();
    empty(&mut app);
    app.mode = Mode::Prompt(Prompt {
        kind: PromptKind::TaskTitle,
        label: "new task title".into(),
        input: String::new(),
    });
    app.on_key(ctl('c'));
    assert!(app.quit.is_none());
}

#[test]
fn configured_detach_still_works_on_the_empty_screen() {
    let (mut app, _rx) = fleet();
    empty(&mut app);
    app.on_key(ctl('b'));
    app.on_key(KeyEvent::ch('q'));
    assert_eq!(app.quit.as_deref(), Some("detached"));
}

#[test]
fn empty_screen_shows_exit_and_live_bindings() {
    let (mut app, _rx) = fleet();
    empty(&mut app);
    app.config.keys.prefix = "ctrl+a".into();
    app.config
        .keys
        .bindings
        .insert("detach".into(), "prefix+d".into());
    app.config
        .keys
        .bindings
        .insert("new_tab".into(), "prefix+t".into());
    app.keymap = Keymap::from_config(&app.config);
    let text = screen(&app);
    assert!(text.contains("ctrl+c to exit"), "{text}");
    assert!(text.contains("ctrl+a then d to detach"), "{text}");
    assert!(text.contains("ctrl+a then t to create a tab"), "{text}");
    assert!(!text.contains("prefix+"), "{text}");
    app.config.keys.bindings.insert("detach".into(), "".into());
    app.config.keys.bindings.insert("new_tab".into(), "".into());
    app.keymap = Keymap::from_config(&app.config);
    let text = screen(&app);
    assert!(text.contains("ctrl+c to exit"), "{text}");
    assert!(!text.contains("to detach"), "{text}");
    assert!(!text.contains("to create a tab"), "{text}");
}
