//! Mention picker tests: which panes are offered, suggested names, Enter and Tab inserting
//! `@name `, the `mention.set` round trip and editing a name with ctrl+e.

use super::*;
use crate::app::{test_app, test_run};
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::Mods;
use vk_proto::mention::{MentionKind, MentionName};
use vk_proto::model::*;
use vk_proto::render::ClientFrame;

fn kev(k: Key) -> KeyEvent {
    KeyEvent::new(k, Mods::empty())
}

fn ch(c: char) -> KeyEvent {
    kev(Key::Char(c))
}

fn enter() -> KeyEvent {
    kev(Key::Named(NamedKey::Enter))
}

fn ctrl_e() -> KeyEvent {
    KeyEvent::new(Key::Char('e'), Mods::CTRL)
}

fn pane(id: &str, ws: &str, title: &str, cwd: &str) -> Pane {
    serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "tab": "T1", "workspace": ws, "title": title,
        "auto_title": "zsh", "cwd": cwd, "cols": 80, "rows": 24,
        "child_pid": null, "fg_cmdline": [], "exited": false, "exit_code": null,
        "unread": false, "marked_unread": false, "pinned": false, "created_by": "user",
        "recovered": null
    }))
    .unwrap()
}

fn mention(name: &str, pane: &str) -> MentionName {
    MentionName {
        name: name.into(),
        workspace: "W1".into(),
        pane: pane.into(),
        kind: MentionKind::Agent,
        delivery: true,
        created_at_ms: 0,
    }
}

/// Workspace `W1` with the focused shell `p1`, an agent `p2` (named `reviewer`), an unnamed
/// agent `p3`, a shell `p4` in `/src/api`, a browser pane, a plugin surface, and an agent `p7`
/// in another workspace.
fn setup() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, rxs) = test_app(1);
    let m = &mut app.machines[0];
    let mut browser = pane("p5", "W1", "docs", "/src/api");
    browser.browser = Some(BrowserPane::default());
    let mut plugin = pane("p6", "W1", "popup", "/src/api");
    plugin.created_by = "plugin-surface:popup:50%:50%:tools/main".into();
    m.model.panes = vec![
        pane("p1", "W1", "main", "/src/api"),
        pane("p2", "W1", "claude", "/src/api"),
        pane("p3", "W1", "claude", "/src/api"),
        pane("p4", "W1", "shell", "/src/api"),
        browser,
        plugin,
        pane("p7", "W2", "claude", "/src/web"),
    ];
    m.model.runs = vec![
        test_run("r2", "p2", "claude"),
        test_run("r3", "p3", "claude"),
        test_run("r7", "p7", "claude"),
    ];
    m.model.mentions = vec![mention("reviewer", "p2")];
    m.focus = ClientFocus {
        workspace: Some("W1".into()),
        tab: Some("T1".into()),
        pane: Some("p1".into()),
    };
    (app, rxs)
}

fn drain(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<ClientFrame> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        v.push(f);
    }
    v
}

fn pastes(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<(String, String)> {
    drain(rx)
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Paste { pane, text, .. } => Some((pane, text)),
            _ => None,
        })
        .collect()
}

fn commands(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<(String, Value)> {
    drain(rx)
        .into_iter()
        .filter_map(|f| match f {
            ClientFrame::Command { json, .. } => {
                let v: Value = serde_json::from_str(&json).unwrap();
                Some((
                    v["method"].as_str().unwrap().to_string(),
                    v["params"].clone(),
                ))
            }
            _ => None,
        })
        .collect()
}

fn open_picker(app: &mut App) {
    app.action("mention_picker", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::MentionPicker(_))));
}

fn picker(app: &App) -> &Picker {
    match &app.mode {
        Mode::Popup(Popup::MentionPicker(p)) => p,
        m => panic!("not the picker: {m:?}"),
    }
}

#[test]
fn offers_other_panes_of_the_workspace_only() {
    let (app, _rx) = setup();
    let rows = entries(&app, &[]);
    let ids: Vec<&str> = rows.iter().map(|e| e.pane.as_str()).collect();
    // Not the focused pane, the browser pane, the plugin surface or another workspace.
    assert_eq!(ids, ["p2", "p3", "p4"]);
    assert_eq!(rows[0].name.as_deref(), Some("reviewer"));
    assert_eq!(rows[0].state, "idle");
    assert_eq!(rows[2].state, "shell");
    assert!(rows[2].terminal);
}

#[test]
fn suggestions_are_free_and_never_repeat() {
    let (mut app, _rx) = setup();
    // `p3` proposes `claude`; a second unnamed agent gets `claude-2`.
    app.machines[0]
        .model
        .panes
        .push(pane("p8", "W1", "claude", "/src/api"));
    app.machines[0]
        .model
        .runs
        .push(test_run("r8", "p8", "claude"));
    let rows = entries(&app, &[]);
    let shown: Vec<&str> = rows.iter().map(|e| e.shown.as_str()).collect();
    assert_eq!(shown, ["reviewer", "claude", "shell", "claude-2"]);
    // An existing name of the workspace is never proposed.
    app.machines[0].model.mentions.push(mention("claude", "p3"));
    let rows = entries(&app, &[]);
    assert_eq!(rows[1].shown, "claude");
    assert_eq!(rows[3].shown, "claude-2");
    let mut all: Vec<&str> = rows.iter().map(|e| e.shown.as_str()).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), rows.len());
}

#[test]
fn fuzzy_search_covers_name_label_and_harness() {
    let (mut app, _rx) = setup();
    open_picker(&mut app);
    for c in "revi".chars() {
        app.on_key(ch(c));
    }
    let rows = ranked(&app, picker(&app));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].pane, "p2");
}

#[test]
fn enter_on_a_named_row_pastes_the_name_and_closes() {
    let (mut app, mut rxs) = setup();
    open_picker(&mut app);
    drain(&mut rxs[0]);
    // The first row is `p2`, named `reviewer`.
    app.on_key(enter());
    assert_eq!(
        pastes(&mut rxs[0]),
        vec![("p1".to_string(), "@reviewer ".to_string())]
    );
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn tab_on_a_named_row_pastes_and_keeps_the_picker_open() {
    let (mut app, mut rxs) = setup();
    open_picker(&mut app);
    drain(&mut rxs[0]);
    app.on_key(kev(Key::Named(NamedKey::Tab)));
    assert_eq!(
        pastes(&mut rxs[0]),
        vec![("p1".to_string(), "@reviewer ".to_string())]
    );
    assert_eq!(picker(&app).inserted, ["reviewer"]);
}

#[test]
fn enter_on_an_unnamed_row_names_it_first() {
    let (mut app, mut rxs) = setup();
    open_picker(&mut app);
    app.on_key(kev(Key::Named(NamedKey::Down)));
    drain(&mut rxs[0]);
    app.on_key(enter());
    let cmds = commands(&mut rxs[0]);
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    assert_eq!(cmds[0].0, "mention.set");
    assert_eq!(cmds[0].1["pane"], "p3");
    assert_eq!(cmds[0].1["name"], "claude");
    // Nothing is pasted before the server accepts the name; the picker waits.
    assert!(pastes(&mut rxs[0]).is_empty());
    assert!(picker(&app).busy);
    on_reply(
        &mut app,
        0,
        Reply::Set {
            target: "p1".into(),
            name: "claude".into(),
            keep_open: false,
        },
        Ok(json!({})),
    );
    assert_eq!(
        pastes(&mut rxs[0]),
        vec![("p1".to_string(), "@claude ".to_string())]
    );
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn a_refused_name_shows_the_error_and_stays_open() {
    let (mut app, mut rxs) = setup();
    open_picker(&mut app);
    app.on_key(kev(Key::Named(NamedKey::Down)));
    app.on_key(enter());
    drain(&mut rxs[0]);
    let err = RpcErr {
        kind: "conflict".into(),
        message: "name taken".into(),
        details: json!({"reason": "name_taken"}),
    };
    on_reply(
        &mut app,
        0,
        Reply::Set {
            target: "p1".into(),
            name: "claude".into(),
            keep_open: false,
        },
        Err(err),
    );
    let p = picker(&app);
    assert!(!p.busy);
    assert_eq!(p.error.as_deref(), Some("@claude is already taken"));
    assert!(pastes(&mut rxs[0]).is_empty());
}

#[test]
fn ctrl_e_edits_the_name_and_checks_it() {
    let (mut app, mut rxs) = setup();
    open_picker(&mut app);
    app.on_key(kev(Key::Named(NamedKey::Down)));
    app.on_key(ctrl_e());
    assert_eq!(picker(&app).edit.as_ref().unwrap().input, "claude");
    // Clear the input and type a name that does not start with a letter.
    for _ in 0.."claude".len() {
        app.on_key(kev(Key::Named(NamedKey::Backspace)));
    }
    for c in "1x".chars() {
        app.on_key(ch(c));
    }
    app.on_key(enter());
    assert!(picker(&app).edit.is_some(), "an invalid name keeps editing");
    assert!(picker(&app).error.is_some());
    // A name another pane has is refused too.
    for _ in 0.."1x".len() {
        app.on_key(kev(Key::Named(NamedKey::Backspace)));
    }
    for c in "reviewer".chars() {
        app.on_key(ch(c));
    }
    app.on_key(enter());
    assert!(picker(&app).edit.is_some());
    for _ in 0.."reviewer".len() {
        app.on_key(kev(Key::Named(NamedKey::Backspace)));
    }
    // Upper case is lowered; characters outside the name rule are dropped.
    for c in "My.Bot".chars() {
        app.on_key(ch(c));
    }
    assert_eq!(picker(&app).edit.as_ref().unwrap().input, "mybot");
    app.on_key(enter());
    assert!(picker(&app).edit.is_none());
    assert_eq!(
        picker(&app).overrides,
        [("p3".to_string(), "mybot".to_string())]
    );
    // Enter now names the pane with the edited name.
    drain(&mut rxs[0]);
    app.on_key(enter());
    let cmds = commands(&mut rxs[0]);
    assert_eq!(cmds[0].0, "mention.set");
    assert_eq!(cmds[0].1["name"], "mybot");
}

#[test]
fn esc_cancels_an_edit_and_then_closes() {
    let (mut app, _rx) = setup();
    open_picker(&mut app);
    app.on_key(ctrl_e());
    app.on_key(kev(Key::Named(NamedKey::Escape)));
    assert!(picker(&app).edit.is_none());
    app.on_key(kev(Key::Named(NamedKey::Escape)));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn name_this_pane_prefills_a_suggestion_and_remove_needs_a_name() {
    let (mut app, mut rxs) = setup();
    assert!(!focused_named(&app));
    app.action("name_pane", None);
    let Mode::Prompt(pr) = &app.mode else {
        panic!("no prompt")
    };
    assert_eq!(pr.kind, PromptKind::MentionName);
    // `p1` is a shell titled `main`.
    assert_eq!(pr.input, "main");
    drain(&mut rxs[0]);
    app.on_key(enter());
    let cmds = commands(&mut rxs[0]);
    assert_eq!(cmds[0].0, "mention.set");
    assert_eq!(cmds[0].1["pane"], "p1");
    assert_eq!(cmds[0].1["name"], "main");
    app.machines[0].model.mentions.push(mention("main", "p1"));
    assert!(focused_named(&app));
    app.action("unname_pane", None);
    assert_eq!(commands(&mut rxs[0])[0].0, "mention.clear");
}

#[test]
fn named_panes_show_their_name_in_titles() {
    let (app, _rx) = setup();
    let m = &app.machines[0];
    assert_eq!(crate::draw::pane_title_named(m, "p2"), "@reviewer · claude");
    assert_eq!(crate::draw::pane_title_named(m, "p3"), "claude");
}
