use super::*;
use crate::app::{Popup, test_run};
use crate::drafts::tests::{commands, fleet, only, pane, screen};
use vk_proto::input::Mods;

fn key(app: &mut App, k: Key) {
    app.on_key(KeyEvent::new(k, Mods::empty()));
}
fn typ(app: &mut App, s: &str) {
    for c in s.chars() {
        key(app, Key::Char(c));
    }
}

/// Two workspaces: api (claude r1) and web (codex r2 on p3).
fn two() -> (
    App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
) {
    let (mut app, rxs) = fleet();
    let m = &mut app.machines[0];
    let mut w = m.model.workspaces[0].clone();
    w.id = "W2".into();
    w.name = Some("web".into());
    w.root_path = "/src/web".into();
    m.model.workspaces.push(w);
    let mut t = m.model.tabs[0].clone();
    t.id = "T2".into();
    t.workspace = "W2".into();
    t.layout = vk_proto::model::LayoutNode::Leaf { pane: "p3".into() };
    t.focused_pane = Some("p3".into());
    m.model.tabs.push(t);
    m.model.panes.push(pane("p3", "T2", "W2"));
    m.model.runs.push(test_run("r2", "p3", "codex"));
    (app, rxs)
}

#[test]
fn slash_filters_the_sidebar_and_esc_clears() {
    let (mut app, _rx) = two();
    app.action("workspace_picker", None);
    let all = crate::draw::sidebar_targets(&app).len();
    key(&mut app, Key::Char('/'));
    typ(&mut app, "codex");
    assert!(screen(&app).contains("NAV /codex"));
    let t = crate::draw::sidebar_targets(&app);
    assert!(t.len() < all && !t.is_empty(), "{t:?}");
    assert!(t.iter().all(|(_, p)| p == "p3"), "{t:?}");
    // enter stops typing; j/k and enter move/focus within the filtered rows.
    key(&mut app, Key::Named(NamedKey::Enter));
    assert!(!app.ux.nav.typing);
    key(&mut app, Key::Named(NamedKey::Enter));
    assert_eq!(app.focused_pane().as_deref(), Some("p3"));
    // Leaving navigate mode forgets the filter.
    app.on_tick();
    assert!(app.ux.nav.filter.is_none());
    // esc while filtered clears the filter, a second esc leaves.
    app.action("workspace_picker", None);
    key(&mut app, Key::Char('/'));
    typ(&mut app, "zz");
    key(&mut app, Key::Named(NamedKey::Enter));
    key(&mut app, Key::Named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    assert!(app.ux.nav.filter.is_none());
    key(&mut app, Key::Named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn t_new_task_and_p_pin_on_the_selected_row() {
    let (mut app, mut rxs) = two();
    app.action("workspace_picker", None);
    let t = crate::draw::sidebar_targets(&app);
    let sel = t.iter().position(|(_, p)| p == "p3").unwrap();
    app.mode = Mode::Navigate { sel };
    key(&mut app, Key::Char('p'));
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "pane.pin").1["pane"], "p3");
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    key(&mut app, Key::Char('t'));
    assert!(matches!(
        app.mode,
        Mode::Prompt(Prompt {
            kind: PromptKind::TaskTitle,
            ..
        })
    ));
    assert_eq!(
        app.focused_pane().as_deref(),
        Some("p3"),
        "task in that row's repo"
    );
}

#[test]
fn palette_actions_ask_for_their_argument() {
    let (mut app, mut rxs) = two();
    crate::nav::run_palette(&mut app, "split_vertical");
    assert!(screen(&app).contains("size?"));
    typ(&mut app, "30%");
    key(&mut app, Key::Named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "pane.split");
    assert_eq!(p["direction"], "right");
    assert!((p["ratio"].as_f64().unwrap() - 0.3).abs() < 1e-9);
    // Empty = half (no ratio); garbage is refused.
    crate::nav::run_palette(&mut app, "split_horizontal");
    key(&mut app, Key::Named(NamedKey::Enter));
    let (_, p) = only(&commands(&mut rxs[0]), "pane.split");
    assert!(p.get("ratio").is_none() && p["direction"] == "down");
    crate::nav::run_palette(&mut app, "split_horizontal");
    typ(&mut app, "huge");
    key(&mut app, Key::Named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "pane.split"));
    // switch_workspace 2 → web; focus_agent 1 → p1; new_tab with a command.
    crate::nav::run_palette(&mut app, "switch_workspace");
    typ(&mut app, "2");
    key(&mut app, Key::Named(NamedKey::Enter));
    assert_eq!(app.m().focus.workspace.as_deref(), Some("W2"));
    crate::nav::run_palette(&mut app, "focus_agent");
    typ(&mut app, "1");
    key(&mut app, Key::Named(NamedKey::Enter));
    assert_eq!(app.focused_pane().as_deref(), Some("p1"));
    crate::nav::run_palette(&mut app, "new_tab");
    typ(&mut app, "htop");
    key(&mut app, Key::Named(NamedKey::Enter));
    let (_, p) = only(&commands(&mut rxs[0]), "tab.create");
    assert_eq!(p["command"], json!(["/bin/sh", "-c", "htop"]));
    assert_eq!(parse_size("0.25"), Some(0.25));
    assert_eq!(parse_size("100%"), None);
    let _ = Popup::Help;
}

#[test]
fn ctrl_shift_p_opens_the_palette_on_kitty_hosts_only() {
    let (mut app, _rx) = fleet();
    // Legacy hosts send plain ctrl+p: that goes to the pane.
    app.on_key(KeyEvent::new(Key::Char('p'), Mods::CTRL));
    assert!(matches!(app.mode, Mode::Normal));
    app.on_key(KeyEvent::new(Key::Char('p'), Mods::CTRL | Mods::SHIFT));
    assert!(matches!(app.mode, Mode::Popup(Popup::Palette { .. })));
}
