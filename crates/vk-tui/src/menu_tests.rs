//! The prefix menu (08 §10.4): groups from the live keymap, the delay, no timeout once up,
//! submenus for key sequences, the resize level.

use crate::app::{App, Mode, PrefixState};
use crate::drafts::tests::{commands, fleet, named, only, screen};
use crate::keymap::Keymap;
use crate::menu::{Group, build};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::{Key, KeyEvent, Mods, NamedKey};
use vk_proto::render::ClientFrame;

fn prefix(app: &mut App) {
    app.on_key(KeyEvent::new(Key::Char('b'), Mods::CTRL));
}

fn key(app: &mut App, c: char) {
    app.on_key(KeyEvent::ch(c));
}

fn rebind(app: &mut App, pairs: &[(&str, &str)]) {
    for (a, b) in pairs {
        app.config
            .keys
            .bindings
            .insert(a.to_string(), b.to_string());
    }
    app.keymap = Keymap::from_config(&app.config);
}

fn group<'a>(groups: &'a [Group], title: &str) -> &'a Group {
    groups
        .iter()
        .find(|g| g.title == title)
        .unwrap_or_else(|| panic!("no group {title} in {groups:?}"))
}

fn item<'a>(groups: &'a [Group], title: &str, key: &str) -> &'a crate::menu::Item {
    group(groups, title)
        .items
        .iter()
        .find(|i| i.key == key)
        .unwrap_or_else(|| panic!("no {key} in {title}: {groups:?}"))
}

fn state(app: &App) -> &PrefixState {
    match &app.mode {
        Mode::Prefix(p) => p,
        m => panic!("not in prefix mode: {m:?}"),
    }
}

fn sent_keys(rx: &mut UnboundedReceiver<ClientFrame>) -> usize {
    let mut n = 0;
    while let Ok(f) = rx.try_recv() {
        if matches!(f, ClientFrame::Key { .. }) {
            n += 1;
        }
    }
    n
}

#[test]
fn default_keymap_fills_the_groups() {
    let (app, _rx) = fleet();
    let groups = build(&app, &[]);
    let titles: Vec<&str> = groups.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "pane",
            "focus",
            "text",
            "tab",
            "workspace",
            "agents",
            "hosts",
            "session"
        ]
    );
    assert_eq!(item(&groups, "pane", "v").label, "split side by side");
    assert_eq!(item(&groups, "pane", "-").label, "split stacked");
    assert!(item(&groups, "pane", "r").submenu, "resize opens a level");
    assert_eq!(item(&groups, "tab", "1‥9").label, "jump to tab");
    assert_eq!(item(&groups, "tab", "c").label, "new tab");
    assert_eq!(item(&groups, "workspace", "N").label, "new workspace");
    assert_eq!(item(&groups, "workspace", "g").label, "goto anything");
    assert_eq!(item(&groups, "agents", "a").label, "next needing you");
    assert_eq!(item(&groups, "hosts", "alt+d").label, "devices");
    assert_eq!(item(&groups, "focus", "h").label, "focus left");
    assert_eq!(item(&groups, "text", "[").label, "copy mode");
    // Items follow the group table, not the alphabet of action names.
    let pane: Vec<&str> = group(&groups, "pane")
        .items
        .iter()
        .map(|i| i.key.as_str())
        .collect();
    assert_eq!(&pane[..4], &["v", "-", "x", "z"]);
    assert_eq!(item(&groups, "session", ":").label, "palette");
    assert_eq!(item(&groups, "session", "?").label, "this menu");
    // Every default prefix binding is placed somewhere.
    let n: usize = groups.iter().map(|g| g.items.len()).sum();
    let bound = app.keymap.bindings.iter().filter(|b| b.prefix).count();
    assert_eq!(
        n,
        bound - 8,
        "nine switch_tab bindings collapse into one item"
    );
}

#[test]
fn rebinds_unbinds_commands_and_sequences_show_up() {
    let (mut app, _rx) = fleet();
    rebind(
        &mut app,
        &[
            ("split_vertical", "prefix+alt+v"),
            ("zoom", ""),
            ("new_tab", "prefix+m w"),
            ("rename_tab", "prefix+m t"),
            ("goto", "prefix+ctrl+shift+g"),
        ],
    );
    app.config.keys.command.push(vk_config::KeyCommand {
        key: "prefix+alt+t".into(),
        command: "make test".into(),
        title: Some("run the tests".into()),
        ..Default::default()
    });
    app.keymap = Keymap::from_config(&app.config);
    let groups = build(&app, &[]);
    assert_eq!(item(&groups, "pane", "alt+v").label, "split side by side");
    assert!(group(&groups, "pane").items.iter().all(|i| i.key != "z"));
    assert_eq!(item(&groups, "workspace", "ctrl+G").label, "goto anything");
    assert_eq!(item(&groups, "commands", "alt+t").label, "run the tests");
    // The sequence draws as a submenu in the group of its first action.
    let m = item(&groups, "tab", "m");
    assert!(m.submenu);
    assert_eq!(m.label, "2 keys");
    assert!(group(&groups, "tab").items.iter().all(|i| i.key != "c"));
    let under = build(&app, &[KeyEvent::ch('m')]);
    assert_eq!(item(&under, "tab", "w").label, "new tab");
    assert_eq!(item(&under, "tab", "t").label, "rename tab");
    assert_eq!(under.len(), 1);
}

#[test]
fn menu_appears_after_the_delay_and_then_never_times_out() {
    let (mut app, _rx) = fleet();
    prefix(&mut app);
    let p = state(&app);
    assert!(!p.menu);
    assert!(p.seq.is_empty());
    let d = app.deadlines(Instant::now());
    assert!(d.get("prefix").is_some() && d.get("prefix.menu").is_some());
    assert_eq!(
        d.get("prefix.menu"),
        Some(p.since + Duration::from_millis(400))
    );
    assert_eq!(d.get("prefix"), Some(p.since + Duration::from_millis(1500)));
    assert!(!screen(&app).contains("WORKSPACE"));
    // The delay passes.
    if let Mode::Prefix(p) = &mut app.mode {
        p.since = Instant::now()
            .checked_sub(Duration::from_millis(500))
            .unwrap();
    }
    app.on_tick();
    assert!(state(&app).menu);
    let s = screen(&app);
    assert!(
        s.contains("PANE") && s.contains("WORKSPACE") && s.contains("AGENTS"),
        "{s}"
    );
    assert!(s.contains("split side by side"), "{s}");
    assert!(s.contains("esc close"), "{s}");
    assert!(s.contains(" PREFIX "), "{s}");
    // No deadline and no timeout while the menu is up.
    assert!(app.deadlines(Instant::now()).get("prefix").is_none());
    if let Mode::Prefix(p) = &mut app.mode {
        p.since = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
    }
    app.on_tick();
    assert!(state(&app).menu);
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(!screen(&app).contains("WORKSPACE"));
}

#[test]
fn a_key_runs_its_action_from_the_menu() {
    let (mut app, mut rxs) = fleet();
    app.mode = Mode::Prefix(PrefixState::menu());
    key(&mut app, 'v');
    assert!(matches!(app.mode, Mode::Normal));
    only(&commands(&mut rxs[0]), "pane.split");
}

#[test]
fn help_opens_the_menu_at_once_and_unbound_keys_close_it() {
    let (mut app, _rx) = fleet();
    app.action("help", None);
    assert!(state(&app).menu);
    assert!(screen(&app).contains("WORKSPACE"));
    key(&mut app, '~');
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text == "no binding for prefix+~")
    );
    // `prefix_menu_ms = 0` shows it with the prefix.
    app.keymap.menu_ms = Some(0);
    prefix(&mut app);
    assert!(state(&app).menu);
    // Disabled: only the timeout is armed.
    app.keymap.menu_ms = None;
    app.mode = Mode::Normal;
    prefix(&mut app);
    assert!(!state(&app).menu);
    let d = app.deadlines(Instant::now());
    assert!(d.get("prefix").is_some() && d.get("prefix.menu").is_none());
}

#[test]
fn sequences_descend_and_esc_goes_back_up() {
    let (mut app, mut rxs) = fleet();
    rebind(
        &mut app,
        &[("new_tab", "prefix+m w"), ("rename_tab", "prefix+m t")],
    );
    prefix(&mut app);
    key(&mut app, 'm');
    let p = state(&app);
    assert!(p.menu, "a submenu is always drawn");
    assert_eq!(p.seq.len(), 1);
    let s = screen(&app);
    assert!(s.contains(" PREFIX m "), "{s}");
    assert!(s.contains("ctrl+b m"), "{s}");
    assert!(s.contains("esc back"), "{s}");
    app.on_key(named(NamedKey::Escape));
    let p = state(&app);
    assert!(p.menu && p.seq.is_empty());
    key(&mut app, 'm');
    key(&mut app, 'q');
    assert!(matches!(app.mode, Mode::Normal));
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text == "no binding for prefix+m q")
    );
    prefix(&mut app);
    key(&mut app, 'm');
    key(&mut app, 'w');
    assert!(matches!(app.mode, Mode::Normal));
    only(&commands(&mut rxs[0]), "tab.create");
}

#[test]
fn prefix_twice_still_passes_through_and_releases_keep_the_menu() {
    let (mut app, mut rxs) = fleet();
    let _ = sent_keys(&mut rxs[0]);
    prefix(&mut app);
    prefix(&mut app);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(sent_keys(&mut rxs[0]), 1);
    app.mode = Mode::Prefix(PrefixState::menu());
    let mut rel = KeyEvent::new(Key::Char('b'), Mods::CTRL);
    rel.kind = vk_proto::input::KeyKind::Release;
    app.on_key(rel);
    app.on_key(named(NamedKey::LeftShift));
    assert!(state(&app).menu);
    assert_eq!(sent_keys(&mut rxs[0]), 0);
}

#[test]
fn esc_still_reaches_bindings_and_hooks_first() {
    let (mut app, mut rxs) = fleet();
    rebind(&mut app, &[("zoom", "prefix+esc")]);
    app.mode = Mode::Prefix(PrefixState::menu());
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    only(&commands(&mut rxs[0]), "pane.zoom");
}

#[test]
fn a_short_screen_says_how_many_keys_are_hidden() {
    let (mut app, _rx) = fleet();
    app.size = (80, 24);
    app.action("help", None);
    let s = screen(&app);
    assert!(
        s.contains(" more · : palette has everything · esc close"),
        "{s}"
    );
    assert!(!s.contains("esc close · : palette"), "{s}");
    assert!(s.lines().count() <= 24);
    // Too narrow beside the sidebar: the menu takes the whole width and gets two columns.
    assert!(
        s.lines().any(|l| l.contains("PANE") && l.contains("FOCUS")),
        "{s}"
    );
}

#[test]
fn resize_mode_draws_as_a_sticky_level() {
    let (mut app, _rx) = fleet();
    app.action("resize_mode", None);
    assert!(matches!(app.mode, Mode::Resize));
    let s = screen(&app);
    assert!(s.contains(" RESIZE "), "{s}");
    assert!(s.contains("ctrl+b r · resize"), "{s}");
    assert!(s.contains("equalize") && s.contains("keys repeat"), "{s}");
    key(&mut app, 'l');
    assert!(matches!(app.mode, Mode::Resize));
}

#[test]
fn small_terminals_still_draw() {
    let (mut app, _rx) = fleet();
    app.action("help", None);
    for size in [(60, 20), (40, 12), (24, 8), (12, 5)] {
        app.size = size;
        let s = screen(&app);
        assert!(s.lines().count() as u16 <= size.1, "{size:?}");
    }
    app.size = (60, 20);
    let s = screen(&app);
    assert!(s.contains("PANE"), "{s}");
}
