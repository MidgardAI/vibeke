use super::*;
use crate::drafts::tests::{commands, fleet, only, pane, screen};
use crossterm::event::KeyModifiers;
use vk_proto::model::LayoutNode;

fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
    app.on_mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
}

/// `n` extra tabs T2.. with long titles in W1.
fn many_tabs(app: &mut App, n: usize) {
    let m = &mut app.machines[0];
    for i in 2..2 + n {
        let mut t = m.model.tabs[0].clone();
        t.id = format!("T{i}");
        t.number = i as u32;
        t.title = Some(format!("long-tab-title-{i}"));
        t.layout = LayoutNode::Leaf {
            pane: format!("q{i}"),
        };
        t.focused_pane = Some(format!("q{i}"));
        m.model.tabs.push(t);
        m.model
            .panes
            .push(pane(&format!("q{i}"), &format!("T{i}"), "W1"));
    }
}

#[test]
fn overflow_shows_arrows_follows_focus_and_scrolls_on_click() {
    let (mut app, mut rxs) = fleet();
    many_tabs(&mut app, 10);
    let laid = crate::draw::tab_layout(&app);
    assert!(laid.left.is_none() && laid.right.is_some(), "{laid:?}");
    assert_eq!(laid.first, 0);
    let row0 = screen(&app).lines().next().unwrap().to_string();
    assert!(row0.contains('›'), "{row0}");
    // Focus the last tab: the window follows it.
    app.focus_pane(0, "q11");
    let laid = crate::draw::tab_layout(&app);
    assert!(laid.left.is_some() && laid.right.is_none(), "{laid:?}");
    assert_eq!(laid.entries.last().unwrap().0.id, "T11");
    // Click ‹: scroll one tab left, held while the focus stays.
    let x = laid.left.unwrap();
    let first = laid.first;
    mouse(&mut app, MouseEventKind::Down(CtButton::Left), x, 0);
    assert_eq!(crate::draw::tab_layout(&app).first, first - 1);
    // A click on a visible tab still focuses it (no tab.move without a drag).
    let laid = crate::draw::tab_layout(&app);
    let (t, _, a, _) = laid.entries[0].clone();
    commands(&mut rxs[0]);
    mouse(&mut app, MouseEventKind::Down(CtButton::Left), a + 1, 0);
    mouse(&mut app, MouseEventKind::Up(CtButton::Left), a + 1, 0);
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "tab.focus").1["tab"], t.id);
    assert!(cmds.iter().all(|c| c.1 != "tab.move"));
}

#[test]
fn drag_reorders_with_tab_move_and_shows_a_marker() {
    let (mut app, mut rxs) = fleet();
    many_tabs(&mut app, 2);
    let laid = crate::draw::tab_layout(&app);
    assert!(laid.right.is_none());
    let (_, _, a0, _) = laid.entries[0].clone();
    let (_, _, a2, b2) = laid.entries[2].clone();
    mouse(&mut app, MouseEventKind::Down(CtButton::Left), a0 + 1, 0);
    mouse(&mut app, MouseEventKind::Drag(CtButton::Left), b2 - 1, 0);
    assert_eq!(app.ux.tabs.drag.as_ref().unwrap().2, 2);
    let laid = crate::draw::tab_layout(&app);
    assert_eq!(drop_marker(&app, &laid), Some(b2));
    assert!(screen(&app).lines().next().unwrap().contains('▏'));
    let _ = a2;
    mouse(&mut app, MouseEventKind::Up(CtButton::Left), b2 - 1, 0);
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "tab.move");
    assert_eq!(p, json!({"tab": "T1", "delta": 2}));
    assert!(app.ux.tabs.drag.is_none());
}

#[test]
fn middle_click_closes_and_asks_when_busy() {
    let (mut app, mut rxs) = fleet();
    many_tabs(&mut app, 1);
    let laid = crate::draw::tab_layout(&app);
    let (_, _, a, _) = laid.entries[1].clone();
    mouse(&mut app, MouseEventKind::Down(CtButton::Middle), a + 1, 0);
    let cmds = commands(&mut rxs[0]);
    assert_eq!(only(&cmds, "tab.close").1["tab"], "T2");
    // A running process: confirm first.
    app.machines[0].model.panes[0].fg_cmdline = vec!["vim".into()];
    let (_, _, a, _) = crate::draw::tab_layout(&app).entries[0].clone();
    mouse(&mut app, MouseEventKind::Down(CtButton::Middle), a + 1, 0);
    assert!(matches!(app.mode, Mode::Popup(Popup::Confirm { .. })));
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "tab.close"));
}

#[test]
fn show_numbers_off_drops_the_number() {
    let (mut app, _rx) = fleet();
    assert!(screen(&app).lines().next().unwrap().contains("1 claude "));
    app.config.ui.tabs.show_numbers = false;
    let row = screen(&app).lines().next().unwrap().to_string();
    assert!(
        row.contains(" claude ") && !row.contains("1 claude"),
        "{row}"
    );
}

#[test]
fn tab_renumber_asks_the_server_for_the_focused_workspace() {
    let (mut app, mut rxs) = fleet();
    many_tabs(&mut app, 2);
    // Palette-only: listed, unbound.
    let e = crate::nav::palette_entries(&app);
    let r = e.iter().find(|x| x.id == "tab_renumber").unwrap();
    assert!(r.binding.is_none());
    crate::nav::run_palette(&mut app, "tab_renumber");
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "tab.renumber");
    assert_eq!(p, json!({"workspace": "W1"}));
    crate::drafts::tests::reply(
        &mut app,
        0,
        req,
        json!({"tabs": [{"id": "T1"}, {"id": "T2"}, {"id": "T3"}]}),
    );
    assert!(screen(&app).contains("tabs renumbered 1..3"));
    // An older server: explained, nothing else happens.
    app.action("tab_renumber", None);
    let (req, _) = only(&commands(&mut rxs[0]), "tab.renumber");
    crate::drafts::tests::reply_err(&mut app, 0, req, "method_not_found", json!({}));
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("can't renumber tabs yet")
    );
    assert!(commands(&mut rxs[0]).is_empty());
}
