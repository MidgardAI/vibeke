//! Drafts composer tests: list / edit / reorder / combine / delete, the send flow with the send
//! check, unsafe-send refusal ("Open pane to send"), reconcile before a warned retry, persisted
//! idempotency keys for draft sends, notes with `expected_rev` conflicts, peek "Save as draft",
//! the shared text editor, and drawing. Also the fixtures shared with the desk, assist and
//! gallery tests.

use super::*;
use crate::app::{App, test_app, test_run};
use crate::tasks::grid_text;
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::Mods;
use vk_proto::model::*;
use vk_proto::render::{ClientFrame, ServerFrame};

// ---- shared fixtures -----------------------------------------------------------------------------

pub(crate) fn kev(k: Key) -> KeyEvent {
    KeyEvent::new(k, Mods::empty())
}
pub(crate) fn ch(c: char) -> KeyEvent {
    kev(Key::Char(c))
}
pub(crate) fn named(n: NamedKey) -> KeyEvent {
    kev(Key::Named(n))
}
pub(crate) fn ctl(c: char) -> KeyEvent {
    KeyEvent::new(Key::Char(c), Mods::CTRL)
}
pub(crate) fn typ(app: &mut App, s: &str) {
    for c in s.chars() {
        app.on_key(ch(c));
    }
}

pub(crate) fn pane(id: &str, tab: &str, ws: &str) -> Pane {
    serde_json::from_value(json!({
        "id": id, "handle": format!("w1:{id}"), "tab": tab, "workspace": ws, "title": null,
        "auto_title": "zsh", "cwd": "/src/api", "cols": 80, "rows": 24,
        "child_pid": null, "fg_cmdline": [], "exited": false, "exit_code": null,
        "unread": false, "marked_unread": false, "pinned": false, "created_by": "user",
        "recovered": null
    }))
    .unwrap()
}

/// One machine, workspace `W1` (`/src/api`), tab `T1` with a claude agent in `p1` (run `r1`)
/// and a shell in `p2`; `p1` focused.
pub(crate) fn fleet() -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    fleet_n(1)
}

pub(crate) fn fleet_n(n: usize) -> (App, Vec<UnboundedReceiver<ClientFrame>>) {
    let (mut app, rxs) = test_app(n);
    for m in app.machines.iter_mut() {
        m.model.workspaces = vec![Workspace {
            id: "W1".into(),
            handle: "w1".into(),
            name: Some("api".into()),
            auto_name: "api".into(),
            root_path: "/src/api".into(),
            task: None,
            order: 1.0,
            branch: None,
        }];
        m.model.tabs = vec![vk_proto::model::Tab {
            id: "T1".into(),
            handle: "t1".into(),
            workspace: "W1".into(),
            title: None,
            number: 1,
            layout: LayoutNode::Split {
                dir: SplitDir::Horizontal,
                children: vec![
                    (LayoutNode::Leaf { pane: "p1".into() }, 0.5),
                    (LayoutNode::Leaf { pane: "p2".into() }, 0.5),
                ],
            },
            focused_pane: Some("p1".into()),
            zoomed_pane: None,
            order: 1.0,
            floating: Default::default(),
            floats_hidden: false,
        }];
        m.model.panes = vec![pane("p1", "T1", "W1"), pane("p2", "T1", "W1")];
        m.model.runs = vec![test_run("r1", "p1", "claude")];
        m.focus = ClientFocus {
            workspace: Some("W1".into()),
            tab: Some("T1".into()),
            pane: Some("p1".into()),
        };
    }
    (app, rxs)
}

/// (req, method, params) of every command sent.
pub(crate) fn commands(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<(u64, String, Value)> {
    let mut v = Vec::new();
    while let Ok(f) = rx.try_recv() {
        if let ClientFrame::Command { req, json } = f {
            let j: Value = serde_json::from_str(&json).unwrap();
            v.push((
                req,
                j["method"].as_str().unwrap().to_string(),
                j["params"].clone(),
            ));
        }
    }
    v
}

pub(crate) fn only(cmds: &[(u64, String, Value)], method: &str) -> (u64, Value) {
    let m: Vec<_> = cmds.iter().filter(|c| c.1 == method).collect();
    assert_eq!(m.len(), 1, "expected one {method} in {cmds:?}");
    (m[0].0, m[0].2.clone())
}

pub(crate) fn reply(app: &mut App, mi: usize, req: u64, result: Value) {
    let json = json!({"jsonrpc": "2.0", "id": req, "result": result}).to_string();
    app.on_frame(mi, ServerFrame::CommandResult { req, json });
}

pub(crate) fn reply_err(app: &mut App, mi: usize, req: u64, kind: &str, details: Value) {
    let json = json!({"jsonrpc": "2.0", "id": req, "error": {"code": -32000, "message": format!("{kind}!"), "data": {"kind": kind, "details": details}}}).to_string();
    app.on_frame(mi, ServerFrame::CommandResult { req, json });
}

pub(crate) fn screen(app: &App) -> String {
    let mut g = Grid::new(app.size.0, app.size.1);
    crate::draw::compose(app, &mut g);
    grid_text(&g)
}

// ---- drafts ------------------------------------------------------------------------------------------

fn drafts_json() -> Value {
    json!({"drafts": [
        {"id": "d1", "title": "Fix tests", "text": "Please fix the tests", "attachments": [], "rev": 1, "sends": []},
        {"id": "d2", "title": null, "text": "Check the login page\nmore", "attachments": [{"kind": "file"}, {"kind": "screenshot"}], "rev": 4,
         "sends": [{"state": "delivery_unknown", "reconciled": false, "idempotency_key": "k0"}]},
        {"id": "d3", "title": "Ship it", "text": "Open the PR", "attachments": [], "rev": 2,
         "sends": [{"state": "failed", "detail": "not sent"}]}
    ]})
}

fn open_with_list(app: &mut App, rx: &mut UnboundedReceiver<ClientFrame>) {
    app.action("drafts", None);
    let cmds = commands(rx);
    let (req, p) = only(&cmds, "draft.list");
    assert_eq!(p, json!({"scope": "workspace", "id": "W1"}));
    reply(app, 0, req, drafts_json());
}

#[test]
fn list_reorder_combine_delete() {
    let (mut app, mut rxs) = fleet();
    open_with_list(&mut app, &mut rxs[0]);
    assert!(matches!(app.mode, Mode::Popup(Popup::Drafts)));
    let v = app.drafts.as_ref().unwrap();
    assert_eq!(v.items.len(), 3);
    assert_eq!(v.items[1].attachments, 2);
    assert!(v.items[1].unknown());
    // Draw: send markers and attachment counts.
    let s = screen(&app);
    assert!(s.contains("? unknown"), "{s}");
    assert!(s.contains("✗ failed"), "{s}");
    assert!(s.contains("📎2"), "{s}");
    assert!(s.contains("Check the login page"), "{s}");
    // J moves the selected draft down and reorders on the server.
    app.on_key(ch('J'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "draft.reorder");
    assert_eq!(p["order"], json!(["d2", "d1", "d3"]));
    assert_eq!(app.drafts.as_ref().unwrap().sel, 1);
    // Combine needs two selections.
    app.on_key(ch('m'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch(' '));
    app.on_key(ch('k'));
    app.on_key(ch(' '));
    app.on_key(ch('m'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "draft.combine");
    assert_eq!(p["ids"], json!(["d2", "d1"]));
    reply(&mut app, 0, req, json!({"draft": {"id": "d4"}}));
    assert!(app.drafts.as_ref().unwrap().selected.is_empty());
    assert!(commands(&mut rxs[0]).iter().any(|c| c.1 == "draft.list"));
    // Delete asks first; any other key keeps it.
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Delete this draft?"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('x'));
    app.on_key(ch('y'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "draft.delete");
    assert_eq!(p["draft"], "d2");
}

#[test]
fn new_and_edit_never_send() {
    let (mut app, mut rxs) = fleet();
    open_with_list(&mut app, &mut rxs[0]);
    app.on_key(ch('n'));
    typ(&mut app, "hello");
    app.on_key(named(NamedKey::Enter));
    typ(&mut app, "world");
    // ctrl+f queues a file attachment for the new draft.
    app.on_key(ctl('f'));
    typ(&mut app, "/tmp/a.png");
    app.on_key(named(NamedKey::Enter));
    app.on_key(ctl('x'));
    let cmds = commands(&mut rxs[0]);
    assert!(
        cmds.iter()
            .all(|c| c.1 != "draft.send" && c.1 != "agent.prompt")
    );
    let (req, p) = only(&cmds, "draft.create");
    assert_eq!(p["text"], "hello\nworld");
    assert_eq!(p["scope"], "workspace");
    assert_eq!(
        p["attachments"],
        json!([{"kind": "file", "path": "/tmp/a.png"}])
    );
    assert!(p["idempotency_key"].is_string());
    reply(&mut app, 0, req, json!({"draft": {"id": "d9"}}));
    assert!(matches!(app.drafts.as_ref().unwrap().sub, Sub::None));
    assert!(screen(&app).contains("nothing was sent"));
    commands(&mut rxs[0]);
    // Edit the first draft: update with the revision it was loaded at.
    app.on_key(ch('e'));
    typ(&mut app, "!");
    app.on_key(ctl('x'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "draft.update");
    assert_eq!(p["draft"], "d1");
    assert_eq!(p["text"], "Please fix the tests!");
    assert_eq!(p["expected_rev"], 1);
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "draft_changed"}),
    );
    assert!(screen(&app).contains("changed elsewhere"));
    // esc with changes asks before discarding.
    app.on_key(named(NamedKey::Escape));
    assert!(screen(&app).contains("Discard your changes?"));
    app.on_key(ch('y'));
    assert!(matches!(app.drafts.as_ref().unwrap().sub, Sub::None));
}

#[test]
fn send_shows_check_and_persists_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let (mut app, mut rxs) = fleet();
    app.pending_ops = crate::pending::PendingStore::open(dir.path().to_path_buf(), "tui-test");
    open_with_list(&mut app, &mut rxs[0]);
    app.on_key(ch('s'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "draft.check");
    assert_eq!(p, json!({"target_run": "r1", "draft": "d1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"run": "r1", "pane": "p1", "harness": "claude", "send_path": "prompt_input", "follow_up": "prompt_input", "steer": false, "hidden_attachments": []}),
    );
    let s = screen(&app);
    assert!(s.contains("send path: prompt_input"), "{s}");
    assert!(s.contains("steer: not supported"), "{s}");
    assert!(s.contains("[ ] include notes"), "{s}");
    app.on_key(ch('i'));
    assert!(screen(&app).contains("[x] include notes"));
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "draft.send");
    assert_eq!(p["draft"], "d1");
    assert_eq!(p["target_run"], "r1");
    assert_eq!(p["include_notes"], true);
    let key = p["idempotency_key"].as_str().unwrap().to_string();
    // Persisted before dispatch, under its idempotency key.
    let op = app.pending_ops.get(&key).expect("pending op");
    assert_eq!(op.method, "draft.send");
    assert_eq!(op.describe(), "Send draft");
    let file = std::fs::read_to_string(dir.path().join("client-pending-tui-test.json")).unwrap();
    assert!(file.contains(&key) && file.contains("draft.send"), "{file}");
    // An unknown outcome keeps the operation; a reconnect asks the owner, never resends.
    reply_err(&mut app, 0, req, "timeout", json!({}));
    assert!(app.pending_ops.get(&key).is_some());
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "task.operation.get");
    assert_eq!(p["idempotency_key"], key.as_str());
    assert!(cmds.iter().all(|c| c.1 != "draft.send"));
    assert!(screen(&app).contains("may have arrived"));
}

#[test]
fn unsafe_send_is_refused_with_open_pane() {
    let (mut app, mut rxs) = fleet();
    open_with_list(&mut app, &mut rxs[0]);
    app.on_key(ch('s'));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "draft.check");
    reply(
        &mut app,
        0,
        req,
        json!({"run": "r1", "send_path": "open_pane_only", "unsafe": "an attached client focuses the pane", "steer": false}),
    );
    let s = screen(&app);
    assert!(s.contains("Open pane to send"), "{s}");
    assert!(s.contains("an attached client focuses the pane"), "{s}");
    // Enter does not send when the check says it isn't safe.
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "draft.send"));
    // The server refusing a send (state changed since the check) keeps the draft.
    if let Some(v) = &mut app.drafts
        && let Sub::Send(f) = &mut v.sub
    {
        f.check = Some(json!({"send_path": "prompt_input"}));
    }
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "draft.send");
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "send_unsafe", "detail": "input box not empty", "fallback": "open_pane_to_send", "pane": "p1", "draft_kept": true}),
    );
    let s = screen(&app);
    assert!(s.contains("zero bytes written; draft kept"), "{s}");
    assert!(s.contains("input box not empty"), "{s}");
    assert!(s.contains("[o] Open pane to send"), "{s}");
    // o focuses that pane (the only focus change) and closes the view.
    app.machines[0].focus.pane = Some("p2".into());
    app.on_key(ch('o'));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.drafts.is_none());
    assert_eq!(app.machines[0].focus.pane.as_deref(), Some("p1"));
}

#[test]
fn unknown_send_reconciles_before_a_warned_retry() {
    let (mut app, mut rxs) = fleet();
    open_with_list(&mut app, &mut rxs[0]);
    app.on_key(ch('j')); // d2: ? unknown
    app.on_key(ch('s'));
    commands(&mut rxs[0]);
    let s = screen(&app);
    assert!(s.contains("[R] reconcile first"), "{s}");
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).iter().all(|c| c.1 != "draft.send"));
    app.on_key(ch('R'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "draft.reconcile");
    assert_eq!(p["draft"], "d2");
    reply(
        &mut app,
        0,
        req,
        json!({"send": {"state": "delivery_unknown", "reconciled": true}, "receipt": {"known": false}, "may_retry": true, "note": "no matching turn"}),
    );
    let s = screen(&app);
    assert!(s.contains("[!] send again anyway"), "{s}");
    app.on_key(ch('!'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "draft.send");
    assert_eq!(p["retry_despite_unknown"], true);
    assert_eq!(p["draft"], "d2");
}

#[test]
fn delivery_events_update_the_send() {
    let (mut app, mut rxs) = fleet();
    open_with_list(&mut app, &mut rxs[0]);
    app.on_key(ch('s'));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "draft.check");
    reply(
        &mut app,
        0,
        req,
        json!({"send_path": "prompt_input", "harness": "claude"}),
    );
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "draft.send");
    reply(
        &mut app,
        0,
        req,
        json!({"draft": "d1", "send": {"state": "sending"}, "send_path": "prompt_input"}),
    );
    assert!(screen(&app).contains("Sending — delivered once"));
    on_event(
        &mut app,
        0,
        "draft.delivered",
        &json!({"subject": {"draft": "d1"}, "data": {}}),
    );
    assert!(screen(&app).contains("✓ delivered"));
    assert!(commands(&mut rxs[0]).iter().any(|c| c.1 == "draft.list"));
}

#[test]
fn peek_reply_saves_as_draft() {
    let (mut app, mut rxs) = fleet();
    app.mode = Mode::Popup(Popup::Peek { pane: "p1".into() });
    app.on_key(ch('r'));
    typ(&mut app, "half a thought");
    app.on_key(ctl('d'));
    assert!(matches!(&app.mode, Mode::Popup(Popup::Peek { pane }) if pane == "p1"));
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().all(|c| c.1 != "agent.prompt"));
    let (req, p) = only(&cmds, "draft.create");
    assert_eq!(p["text"], "half a thought");
    assert_eq!(p["id"], "W1");
    reply(&mut app, 0, req, json!({"draft": {"id": "d5"}}));
    assert!(app.toasts.iter().any(|t| t.text.contains("Saved as draft")));
    // Peek d opens the workspace's drafts targeting that agent.
    app.on_key(ch('d'));
    assert!(matches!(app.mode, Mode::Popup(Popup::Drafts)));
    assert_eq!(
        app.drafts.as_ref().unwrap().default_run.as_deref(),
        Some("r1")
    );
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Popup(Popup::Peek { .. })));
}

#[test]
fn notes_conflict_asks_before_overwriting() {
    let (mut app, mut rxs) = fleet();
    app.action("notes", None);
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "notes.get");
    assert_eq!(p["workspace"], "W1");
    reply(
        &mut app,
        0,
        req,
        json!({"notes": {"workspace": "W1", "text": "remember", "rev": 3}}),
    );
    let s = screen(&app);
    assert!(s.contains("Never sent unless you include it"), "{s}");
    assert!(s.contains("remember"), "{s}");
    app.on_key(ch('e'));
    typ(&mut app, " this");
    app.on_key(ctl('x'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "notes.set");
    assert_eq!(p["text"], "remember this");
    assert_eq!(p["expected_rev"], 3);
    reply_err(&mut app, 0, req, "conflict", json!({}));
    assert!(screen(&app).contains("[o] overwrite with yours"));
    app.on_key(ch('o'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "notes.set");
    assert!(p.get("expected_rev").is_none());
    reply(
        &mut app,
        0,
        req,
        json!({"notes": {"text": "remember this", "rev": 5}}),
    );
    let v = app.drafts.as_ref().unwrap();
    assert_eq!(v.notes.rev, Some(5));
    assert!(v.notes.editing.is_none());
}

#[test]
fn text_editor_edits_lines() {
    let mut e = TextEditor::new("ab\ncd", true);
    assert_eq!((e.row, e.col), (1, 2));
    e.key(&named(NamedKey::Up));
    e.key(&named(NamedKey::Home));
    e.key(&ch('x'));
    assert_eq!(e.text(), "xab\ncd");
    e.key(&named(NamedKey::End));
    e.key(&named(NamedKey::Delete));
    assert_eq!(e.text(), "xabcd");
    e.key(&named(NamedKey::Enter));
    assert_eq!(e.text(), "xab\ncd");
    e.key(&named(NamedKey::Backspace));
    assert_eq!(e.text(), "xabcd");
    e.insert_str("1\n2");
    assert_eq!(e.text(), "xab1\n2cd");
    let mut one = TextEditor::new("a\nb", false);
    assert_eq!(one.text(), "a b");
    one.key(&named(NamedKey::Enter));
    one.insert_str("\nc");
    assert_eq!(one.text(), "a b c");
    assert!(!one.key(&ctl('x')));
}
