//! Watch panes in the TUI (06 B7): the view goes to the machine running the agent's browser,
//! input is dropped while read-only, `prefix+t` takes over and releases, and the chrome says
//! which state the pane is in.

use super::*;
use crate::app::test_app;
use crate::tasks::grid_text;
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::{Key, Mods};
use vk_proto::model::*;

fn watch_pane(id: &str, tab: &str) -> Pane {
    serde_json::from_value(json!({
        "id": id, "handle": "w1:p2", "tab": tab, "workspace": "W", "title": null,
        "auto_title": "◉ watching b3", "cwd": null, "cols": 80, "rows": 24,
        "child_pid": null, "fg_cmdline": [], "exited": false, "exit_code": null,
        "unread": false, "marked_unread": false, "pinned": false, "created_by": "user",
        "recovered": null,
        "browser": {"url": "http://localhost:5173/login", "machine": "", "task": null,
                    "preview": null, "source_pane": "p1",
                    "history": ["http://localhost:5173/login"], "history_index": 0,
                    "title": "", "watch": "b3"}
    }))
    .unwrap()
}

/// Machine 0 is local; machine 1 (the devbox) runs the agent and owns a tab with the agent's
/// pane p1 and the watch pane wp (focused).
fn setup() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, rxs) = test_app(2);
    app.cur = 1;
    let m = &mut app.machines[1];
    m.model.workspaces = vec![Workspace {
        id: "W".into(),
        handle: "w1".into(),
        name: None,
        auto_name: "w".into(),
        root_path: "/".into(),
        task: None,
        order: 1.0,
        branch: None,
    }];
    m.model.tabs = vec![Tab {
        id: "T".into(),
        handle: "w1:t1".into(),
        workspace: "W".into(),
        title: None,
        number: 1,
        layout: LayoutNode::Split {
            dir: SplitDir::Horizontal,
            children: vec![
                (LayoutNode::Leaf { pane: "p1".into() }, 0.5),
                (LayoutNode::Leaf { pane: "wp".into() }, 0.5),
            ],
        },
        focused_pane: Some("wp".into()),
        zoomed_pane: None,
        order: 1.0,
    }];
    let mut shell = watch_pane("p1", "T");
    shell.browser = None;
    m.model.panes = vec![shell, watch_pane("wp", "T")];
    m.focus = ClientFocus {
        workspace: Some("W".into()),
        tab: Some("T".into()),
        pane: Some("wp".into()),
    };
    app.caps.kitty_graphics = true;
    app.caps.truecolor = true;
    app.caps.cell_w = 16;
    app.caps.cell_h = 32;
    app.caps.dpr_x100 = 200;
    app.sidebar = false;
    app.size = (121, 25);
    (app, rxs)
}

fn drain(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<ClientFrame> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        v.push(f);
    }
    v
}

fn browser_cmds(frames: &[ClientFrame]) -> Vec<BrowserCmd> {
    frames
        .iter()
        .filter_map(|f| match f {
            ClientFrame::Browser { cmd, .. } => Some(cmd.clone()),
            _ => None,
        })
        .collect()
}

fn state(here: bool, human: bool) -> BrowserStatus {
    BrowserStatus {
        url: "http://localhost:5173/login".into(),
        env: "agent session b3 · m1".into(),
        watch: Some("b3".into()),
        human_control: human,
        controlled_here: here,
        ..Default::default()
    }
}

fn prefix_t(app: &mut App) {
    app.on_key(KeyEvent::new(Key::Char('b'), Mods::CTRL));
    app.on_key(KeyEvent::new(Key::Char('t'), Mods::empty()));
}

#[test]
fn watch_view_goes_to_the_agents_machine() {
    let (mut app, mut rxs) = setup();
    update_views(&mut app);
    assert!(
        drain(&mut rxs[0])
            .iter()
            .all(|f| !matches!(f, ClientFrame::MediaView { .. })),
        "not the laptop's media host"
    );
    let mv = drain(&mut rxs[1])
        .into_iter()
        .find_map(|f| match f {
            ClientFrame::MediaView { panes, shm, .. } => Some((panes, shm)),
            _ => None,
        })
        .expect("MediaView to the owner");
    assert_eq!(mv.0[0].pane, "wp");
    assert_eq!(mv.0[0].owner, "", "the owner renders it itself");
    assert_eq!(mv.0[0].spec.watch.as_deref(), Some("b3"));
    assert!(!mv.1, "remote: zlib tiles over the link, no shm");
}

#[test]
fn read_only_until_taken_over_then_released() {
    let (mut app, mut rxs) = setup();
    on_state(&mut app, 1, "wp".into(), state(false, false));
    // Keys, paste and clicks are dropped (with a hint), nothing reaches either machine.
    app.on_key(KeyEvent::new(Key::Char('x'), Mods::empty()));
    app.on_paste("hello".into());
    let r = app.pane_rects()[1].1;
    app.on_mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(CtButton::Left),
        column: r.x + 3,
        row: r.y + 3,
        modifiers: KeyModifiers::empty(),
    });
    assert!(browser_cmds(&drain(&mut rxs[1])).is_empty());
    assert!(browser_cmds(&drain(&mut rxs[0])).is_empty());
    assert!(app.toasts.iter().any(|t| t.text.contains("read-only")));
    // prefix+t takes over (sent to the agent's machine).
    prefix_t(&mut app);
    assert_eq!(
        browser_cmds(&drain(&mut rxs[1])),
        vec![BrowserCmd::TakeOver(true)]
    );
    // Once the server confirms, input flows.
    on_state(&mut app, 1, "wp".into(), state(true, true));
    app.on_key(KeyEvent::new(Key::Char('q'), Mods::empty()));
    let cmds = browser_cmds(&drain(&mut rxs[1]));
    assert!(matches!(&cmds[..], [BrowserCmd::Key(k)] if k.key == Key::Char('q')));
    // prefix+t again releases.
    prefix_t(&mut app);
    assert_eq!(
        browser_cmds(&drain(&mut rxs[1])),
        vec![BrowserCmd::TakeOver(false)]
    );
    // Taken over elsewhere (CLI): read-only here, with a different hint.
    on_state(&mut app, 1, "wp".into(), state(false, true));
    app.toasts.clear();
    app.browser.ro_hint = None;
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::empty()));
    assert!(browser_cmds(&drain(&mut rxs[1])).is_empty());
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("taken over elsewhere"))
    );
}

#[test]
fn watch_chrome_states() {
    let (mut app, _rxs) = setup();
    let draw = |app: &App| {
        let mut g = Grid::new(121, 25);
        crate::draw::compose(app, &mut g);
        grid_text(&g)
    };
    on_state(&mut app, 1, "wp".into(), state(false, false));
    let s = draw(&app);
    assert!(s.contains("watching agent session b3 · read-only"), "{s}");
    assert!(s.contains("localhost:5173/login"));
    assert!(!s.contains(" ← "), "no navigation buttons while watching");
    on_state(&mut app, 1, "wp".into(), state(true, true));
    let s = draw(&app);
    assert!(
        s.contains("you control agent session b3 · prefix+t releases"),
        "{s}"
    );
    on_state(&mut app, 1, "wp".into(), state(false, true));
    let s = draw(&app);
    assert!(s.contains("taken over elsewhere"), "{s}");
    // A normal browser pane's take-over key explains itself.
    app.machines[1].model.panes[1]
        .browser
        .as_mut()
        .unwrap()
        .watch = None;
    prefix_t(&mut app);
    assert!(
        app.toasts
            .iter()
            .any(|t| t.text.contains("not watching an agent session"))
    );
}
