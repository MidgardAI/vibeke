use super::*;
use crate::app::test_interaction;
use crate::drafts::tests::{commands, fleet, only, pane, reply, screen};

fn popup_pane(id: &str) -> vk_proto::model::Pane {
    let mut p = pane(id, "T1", "W1");
    p.created_by = "plugin-surface:popup:50%:50%:vibeke/popup".into();
    p
}

#[test]
fn key_command_popup_floats_with_the_popup_tag_and_returns_focus() {
    let (mut app, mut rxs) = fleet();
    app.config.keys.command = vec![vk_config::KeyCommand {
        key: "prefix+alt+g".into(),
        kind: vk_config::CommandType::Popup,
        command: "lazygit".into(),
        width: Some("60%".into()),
        height: Some("50%".into()),
        cwd: Some("workspace".into()),
        ..Default::default()
    }];
    app.action("command:0", None);
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "pane.float");
    assert_eq!(p["popup"], json!({"width": "60%", "height": "50%"}));
    assert_eq!(p["command"], json!(["/bin/sh", "-c", "lazygit"]));
    assert_eq!(p["cwd"], "/src/api");
    assert_eq!(p["rect"]["w"], 60.0);
    assert_eq!(p["rect"]["x"], 20.0);
    reply(&mut app, 0, req, json!({"pane": {"id": "P9"}}));
    assert_eq!(
        app.ux
            .popups
            .prev
            .get(&(0, "P9".into()))
            .map(String::as_str),
        Some("p1")
    );
    // The popup shows (drawn by the plugin popup machinery) and dims the rest.
    app.machines[0].model.panes.push(popup_pane("P9"));
    app.machines[0].model.tabs[0]
        .floating
        .push(vk_proto::model::FloatingPane::centred("P9", 1));
    app.machines[0]
        .panes
        .insert("P9".into(), crate::app::PaneBuf::blank());
    app.focus_pane(0, "P9");
    let mut g = Grid::new(120, 40);
    crate::draw::compose(&app, &mut g);
    let a = app.pane_area();
    assert!(
        g.get(a.x, a.y + a.h - 1).unwrap().style.attrs & attr::DIM != 0,
        "dimmed"
    );
    assert!(screen(&app).contains("vibeke"), "popup frame title");
    // The command exits and the pane goes away: focus back to p1.
    app.machines[0].model.panes.retain(|p| p.id != "P9");
    app.machines[0].model.tabs[0].floating.clear();
    tick(&mut app);
    assert_eq!(app.focused_pane().as_deref(), Some("p1"));
    assert!(app.ux.popups.prev.is_empty());
}

#[test]
fn popups_move_and_resize_by_their_frame() {
    let (mut app, _rx) = fleet();
    app.machines[0].model.panes.push(popup_pane("P9"));
    app.machines[0].model.tabs[0]
        .floating
        .push(vk_proto::model::FloatingPane::centred("P9", 1));
    let s0 = crate::plugins::popup(&app).unwrap();
    let ev = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    // Move: drag the top border 3 right, 2 down.
    app.on_mouse(ev(
        MouseEventKind::Down(CtButton::Left),
        s0.outer.x + 4,
        s0.outer.y,
    ));
    app.on_mouse(ev(
        MouseEventKind::Drag(CtButton::Left),
        s0.outer.x + 7,
        s0.outer.y + 2,
    ));
    app.on_mouse(ev(
        MouseEventKind::Up(CtButton::Left),
        s0.outer.x + 7,
        s0.outer.y + 2,
    ));
    let s1 = crate::plugins::popup(&app).unwrap();
    assert_eq!((s1.outer.x, s1.outer.y), (s0.outer.x + 3, s0.outer.y + 2));
    assert_eq!(s1.inner, crate::floats::inner_rect(s1.outer));
    // Resize from the bottom-right corner.
    let (cx, cy) = (s1.outer.x + s1.outer.w - 1, s1.outer.y + s1.outer.h - 1);
    app.on_mouse(ev(MouseEventKind::Down(CtButton::Left), cx, cy));
    app.on_mouse(ev(MouseEventKind::Drag(CtButton::Left), cx + 4, cy - 1));
    app.on_mouse(ev(MouseEventKind::Up(CtButton::Left), cx + 4, cy - 1));
    let s2 = crate::plugins::popup(&app).unwrap();
    assert_eq!((s2.outer.w, s2.outer.h), (s1.outer.w + 4, s1.outer.h - 1));
    // The pane rects (view hints) follow.
    assert!(
        app.pane_rects()
            .iter()
            .any(|(p, r)| p == "P9" && *r == s2.inner)
    );
}

#[test]
fn edit_scrollback_editor_runs_in_a_popup_locally() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, mut rxs) = fleet();
    let f = tmp.path().join("sb.txt");
    std::fs::write(&f, "x").unwrap();
    let file = crate::scrollback::test_temp_file(f.clone());
    editor(
        &mut app,
        0,
        vec!["vi".into(), "+3".into(), f.display().to_string()],
        file,
    );
    assert!(app.external.is_none(), "no suspend on the local machine");
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "pane.float");
    assert_eq!(p["command"][0], "vi");
    assert_eq!(p["popup"]["width"], "90%");
    reply(&mut app, 0, req, json!({"pane": {"id": "E1"}}));
    assert!(app.ux.popups.files.contains_key(&(0, "E1".into())));
    app.machines[0].model.panes.push(popup_pane("E1"));
    tick(&mut app);
    assert!(f.exists(), "kept while the editor runs");
    app.machines[0].model.panes.retain(|p| p.id != "E1");
    tick(&mut app);
    assert!(!f.exists(), "deleted when the popup is gone");
    // A remote pane (or the opt-out) keeps the suspend path.
    let f2 = tmp.path().join("sb2.txt");
    std::fs::write(&f2, "x").unwrap();
    app.ux.popups.editor_suspends = true;
    editor(
        &mut app,
        0,
        vec!["vi".into()],
        crate::scrollback::test_temp_file(f2),
    );
    assert!(app.external.is_some());
}

#[test]
fn interaction_overlay_modes() {
    let (mut app, _rx) = fleet();
    app.machines[0]
        .model
        .interactions
        .push(test_interaction("i2", "p2", "Run it", 1));
    app.machines[0]
        .model
        .interactions
        .push(test_interaction("i1", "p1", "Focused one", 2));
    // unfocused (default): a card for p2, none for the focused p1.
    open_card(&mut app, 0, "i2");
    assert!(matches!(app.mode, Mode::Popup(Popup::Card { .. })));
    app.mode = Mode::Normal;
    open_card(&mut app, 0, "i1");
    assert!(matches!(app.mode, Mode::Normal));
    // always: the focused pane's card on explicit invocation.
    app.config.ui.interaction_overlay = InteractionOverlay::Always;
    open_card(&mut app, 0, "i1");
    assert!(matches!(app.mode, Mode::Popup(Popup::Card { .. })));
    // off: never a card; the pane is focused instead.
    app.mode = Mode::Normal;
    app.config.ui.interaction_overlay = InteractionOverlay::Off;
    app.next_attention_m1(false);
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.focused_pane().as_deref(), Some("p2"));
}
