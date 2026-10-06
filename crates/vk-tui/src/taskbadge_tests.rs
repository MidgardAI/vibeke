//! Task badges: the cached PR status from `task.get` (never `task.pr`, never `gh`), its
//! refresh cadence, and recreate/forget for tasks marked missing (sidebar marker, navigate keys,
//! palette entries, confirmation, `method_not_found`).

use super::*;
use crate::drafts::tests::{ch, commands, fleet, only, pane, reply, reply_err, screen};
use vk_proto::model::{LayoutNode, Workspace};

/// `fleet()` plus task workspace W2 (`fix-login`, pane p3) for task T1 (`#k7`).
fn with_task(
    status: &str,
) -> (
    App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
) {
    let (mut app, rx) = fleet();
    let m = &mut app.machines[0];
    m.model.workspaces.push(Workspace {
        id: "W2".into(),
        handle: "w2".into(),
        name: Some("fix-login".into()),
        auto_name: "fix-login".into(),
        root_path: "/wt/fix".into(),
        task: Some("T1".into()),
        order: 2.0,
        branch: None,
    });
    let mut t = m.model.tabs[0].clone();
    t.id = "T2".into();
    t.workspace = "W2".into();
    t.layout = LayoutNode::Leaf { pane: "p3".into() };
    t.focused_pane = Some("p3".into());
    m.model.tabs.push(t);
    m.model.panes.push(pane("p3", "T2", "W2"));
    m.model.tasks = vec![
        serde_json::from_value(json!({
            "id": "T1", "handle": "k7", "title": "fix login", "slug": "fix-login",
            "workspace": "W2", "repo_root": "/src/api", "worktree_path": "/wt/fix",
            "branch": "fix/login", "base_ref": "main", "port_range": null,
            "status": status, "setup_status": null, "created_at_ms": 0
        }))
        .unwrap(),
    ];
    app.config.ui.sidebar.attention_section = false;
    (app, rx)
}

fn pr(label: &str, checks: &str) -> Value {
    json!({"task": {"id": "T1"}, "branch_status": null,
           "pr": {"kind": "pr", "pr": {"number": 12, "state": "OPEN", "is_draft": false,
                  "review_decision": null, "checks": checks, "url": "https://example.test/pr/12",
                  "label": label}}})
}

fn sidebar_line(app: &App, needle: &str) -> String {
    screen(app)
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn pr_badge_comes_from_the_cached_status_and_refreshes_every_minute() {
    let (mut app, mut rx) = with_task("active");
    let t0 = Instant::now();
    tick(&mut app, t0);
    let cmds = commands(&mut rx[0]);
    assert!(
        cmds.iter().all(|c| c.1 != "task.pr"),
        "never runs gh: {cmds:?}"
    );
    let (req, p) = only(&cmds, "task.get");
    assert_eq!(p, json!({"task": "T1"}));
    reply(&mut app, 0, req, pr("#12 ✓", "passing"));
    let line = sidebar_line(&app, "fix-login");
    assert!(line.contains("#12 ✓"), "{line}");
    // Not again within the minute; due after it.
    tick(&mut app, t0 + Duration::from_secs(5));
    assert!(commands(&mut rx[0]).is_empty());
    assert_eq!(app.deadlines(t0).get("tasks.pr"), Some(t0 + REFRESH));
    tick(&mut app, t0 + REFRESH);
    let (req, _) = only(&commands(&mut rx[0]), "task.get");
    // Nothing cached any more: no badge.
    reply(&mut app, 0, req, json!({"task": {"id": "T1"}, "pr": null}));
    assert!(!sidebar_line(&app, "fix-login").contains("#12"));
    // A server without task.get stops being asked.
    tick(&mut app, t0 + REFRESH * 2);
    let (req, _) = only(&commands(&mut rx[0]), "task.get");
    reply_err(&mut app, 0, req, "method_not_found", json!({}));
    tick(&mut app, t0 + REFRESH * 4);
    assert!(commands(&mut rx[0]).is_empty());
    assert!(app.deadlines(t0).get("tasks.pr").is_none());
}

#[test]
fn badge_colour_and_text_follow_the_lookup() {
    let (mut app, _rx) = with_task("active");
    let key = (0, "T1".to_string());
    app.ux.tasks.pr.insert(
        key.clone(),
        Pr::from_value(&pr("#12 ✗ checks", "failing")["pr"]).unwrap(),
    );
    let s = segs(&app, 0, "T1");
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].0, " #12 ✗ checks");
    assert_eq!(s[0].1.fg, app.theme.red);
    // `no_pr` and `unavailable` show nothing.
    app.ux.tasks.pr.insert(
        key,
        Pr::from_value(&json!({"kind": "unavailable", "reason": "gh not logged in"})).unwrap(),
    );
    assert!(segs(&app, 0, "T1").is_empty());
}

#[test]
fn missing_task_shows_a_marker_and_offers_recreate_and_forget() {
    let (mut app, mut rx) = with_task("missing");
    // No PR polling for a task without a checkout.
    tick(&mut app, Instant::now());
    assert!(commands(&mut rx[0]).iter().all(|c| c.1 != "task.get"));
    let line = sidebar_line(&app, "fix-login");
    assert!(line.contains("⊘ missing"), "{line}");
    // Palette: one recreate and one forget entry per missing task.
    let e = crate::nav::palette_entries(&app);
    let rec = e
        .iter()
        .find(|x| x.desc.starts_with("Recreate missing task #k7 fix login"))
        .unwrap();
    assert_eq!(rec.id, "task_recreate:0:T1");
    assert!(e.iter().any(|x| x.id == "task_forget:0:T1"));
    crate::nav::run_palette(&mut app, "task_recreate:0:T1");
    let (req, p) = only(&commands(&mut rx[0]), "task.recreate");
    assert_eq!(p, json!({"task": "T1"}));
    reply_err(&mut app, 0, req, "method_not_found", json!({}));
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("can't recreate tasks yet")
    );
}

#[test]
fn navigate_keys_recreate_and_forget_with_confirmation() {
    let (mut app, mut rx) = with_task("missing");
    let sel = crate::draw::sidebar_targets(&app)
        .iter()
        .position(|(_, p)| p == "p3")
        .unwrap();
    app.mode = Mode::Navigate { sel };
    app.on_key(ch('R'));
    let (req, _) = only(&commands(&mut rx[0]), "task.recreate");
    reply(&mut app, 0, req, json!({}));
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("recreating the checkout of #k7")
    );
    assert!(matches!(app.mode, Mode::Navigate { .. }));
    // Forget asks first; `n` sends nothing.
    app.on_key(ch('F'));
    assert!(matches!(app.mode, Mode::Popup(Popup::Confirm { .. })));
    assert!(screen(&app).contains("Forget task #k7 fix login?"));
    app.on_key(ch('n'));
    assert!(commands(&mut rx[0]).is_empty());
    app.mode = Mode::Navigate { sel };
    app.on_key(ch('F'));
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rx[0]), "task.forget");
    assert_eq!(p, json!({"task": "T1"}));
    reply(&mut app, 0, req, json!({}));
    assert!(app.toasts.last().unwrap().text.contains("forgot #k7"));
}

#[test]
fn actions_target_the_focused_task_and_refuse_healthy_ones() {
    let (mut app, mut rx) = with_task("missing");
    app.focus_pane(0, "p3");
    commands(&mut rx[0]);
    app.action("task_recreate", None);
    only(&commands(&mut rx[0]), "task.recreate");
    app.machines[0].model.tasks[0].status = "active".into();
    app.action("task_recreate:0:T1", None);
    assert!(commands(&mut rx[0]).is_empty());
    assert!(app.toasts.last().unwrap().text.contains("isn't missing"));
    app.action("task_forget", None);
    assert!(
        app.toasts
            .last()
            .unwrap()
            .text
            .contains("no task is marked missing")
    );
}
