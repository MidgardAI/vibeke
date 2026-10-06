use super::*;
use crate::drafts::tests::{fleet, pane, screen};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::{Key, Mods};

fn keys_to(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<(String, String)> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        match f {
            ClientFrame::Key { pane, key, .. } => v.push((pane, format!("{:?}", key.key))),
            ClientFrame::Paste { pane, text, .. } => v.push((pane, text)),
            _ => {}
        }
    }
    v
}

/// T1: p1 (claude agent), p2 shell, p3 shell; p2 focused.
fn setup() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, rxs) = fleet();
    let m = &mut app.machines[0];
    m.model.panes.push(pane("p3", "T1", "W1"));
    m.model.tabs[0].layout = vk_proto::model::LayoutNode::Split {
        dir: vk_proto::model::SplitDir::Horizontal,
        children: vec![
            (
                vk_proto::model::LayoutNode::Leaf { pane: "p1".into() },
                0.34,
            ),
            (
                vk_proto::model::LayoutNode::Leaf { pane: "p2".into() },
                0.33,
            ),
            (
                vk_proto::model::LayoutNode::Leaf { pane: "p3".into() },
                0.33,
            ),
        ],
    };
    app.focus_pane(0, "p2");
    (app, rxs)
}

#[test]
fn keys_and_paste_mirror_to_shell_panes_agents_excluded() {
    let (mut app, mut rxs) = setup();
    keys_to(&mut rxs[0]);
    app.action("sync_input", None);
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("SYNC on: typing goes to 2 pane(s)")
    );
    let s = screen(&app);
    assert!(s.contains(" SYNC 2 ") && s.contains("⇉"), "{s}");
    app.on_key(KeyEvent::new(Key::Char('l'), Mods::empty()));
    let got = keys_to(&mut rxs[0]);
    let panes: Vec<&str> = got.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(panes, ["p2", "p3"], "the agent in p1 gets nothing");
    app.on_paste("ls -la".into());
    let got = keys_to(&mut rxs[0]);
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|x| x.1 == "ls -la"));
    // prefix+alt+s on the agent adds it; on p3 removes it.
    app.focus_pane(0, "p1");
    app.action("sync_input_pane", None);
    app.focus_pane(0, "p3");
    app.action("sync_input_pane", None);
    app.focus_pane(0, "p2");
    app.on_key(KeyEvent::new(Key::Char('x'), Mods::empty()));
    let mut panes: Vec<String> = keys_to(&mut rxs[0]).into_iter().map(|x| x.0).collect();
    panes.sort();
    assert_eq!(panes, ["p1", "p2"]);
    // Typing in a pane outside the set mirrors nothing.
    app.focus_pane(0, "p3");
    app.on_key(KeyEvent::new(Key::Char('y'), Mods::empty()));
    assert_eq!(keys_to(&mut rxs[0]).len(), 1);
    // Quick exit.
    app.action("sync_input_off", None);
    assert!(!active(&app));
    assert!(!screen(&app).contains("SYNC"));
}

#[test]
fn include_agents_and_status_segment_and_cleanup() {
    let (mut app, mut rxs) = setup();
    app.config.ui.sync_input.include_agents = true;
    app.action("sync_input", None);
    assert_eq!(members(&app, 0, "T1").len(), 3);
    keys_to(&mut rxs[0]);
    app.on_key(KeyEvent::new(Key::Char('a'), Mods::empty()));
    assert_eq!(keys_to(&mut rxs[0]).len(), 3);
    assert_eq!(badge(&app).as_deref(), Some(" SYNC 3 "));
    app.config.ui.status_bar.enabled = true;
    app.config.ui.status_bar.right = vec!["sync_input".into()];
    assert!(screen(&app).contains("SYNC 3"));
    // The same key toggles it off; a closed tab is forgotten.
    app.action("sync_input", None);
    assert!(!active(&app));
    app.action("sync_input", None);
    app.machines[0].model.tabs.clear();
    tick(&mut app);
    assert!(app.ux.sync.tabs.is_empty());
}
