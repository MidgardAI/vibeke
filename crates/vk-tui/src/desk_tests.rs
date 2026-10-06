//! Session desk tests: search with filter chips, status markers and exactly labelled actions,
//! Focus live pane (explicit `focus: true` only), Resume native session (exact command and target
//! choice), Start new agent with context (editable package saved as a draft, never sent), the
//! sessions tab, forget with confirmation, and drawing.

use super::*;
use crate::drafts::tests::{ch, commands, ctl, fleet, named, only, reply, reply_err, screen, typ};

fn hits() -> Value {
    json!({"hits": [
        {"session": "sess-live", "harness": "claude", "repo": "/src/api", "turn": 4, "ts": 1, "snippet": "fixed the login redirect",
         "status": "live", "live": {"run": "r1", "pane": "p1"}, "resume": {"argv": ["claude", "--resume", "sess-live"], "command": "claude --resume sess-live"}},
        {"session": "sess-old", "harness": "codex", "repo": "/src/api", "turn": 2, "ts": 1, "snippet": "login tests",
         "status": "resumable", "resume": {"argv": ["codex", "resume", "sess-old"], "command": "codex resume sess-old"}},
        {"session": "sess-gone", "harness": "claude", "repo": "/src/api", "turn": 1, "ts": 1, "snippet": "login idea", "status": "none"}
    ], "index": {"sources": 3, "rows": 10}})
}

fn open_and_search(
    app: &mut crate::app::App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>,
) {
    app.action("desk", None);
    let cmds = commands(rx);
    let (req, _) = only(&cmds, "desk.status");
    reply(
        app,
        0,
        req,
        json!({"counts": {"sources": 3, "rows": 10}, "selection": {"roots": {"claude": ["~/.claude/projects"]}}, "exclude": ["/secret"], "retention_days": 90, "sources": [{"pending_bytes": 2048}]}),
    );
    typ(app, "login");
    app.on_key(named(NamedKey::Enter));
    let cmds = commands(rx);
    let (req, p) = only(&cmds, "desk.search");
    assert_eq!(p, json!({"text": "login", "repo": "/src/api"}));
    reply(app, 0, req, hits());
}

#[test]
fn search_filters_markers_and_labels() {
    let (mut app, mut rxs) = fleet();
    open_and_search(&mut app, &mut rxs[0]);
    let s = screen(&app);
    assert!(s.contains("● live"), "{s}");
    assert!(s.contains("↻ resumable"), "{s}");
    assert!(s.contains("· none"), "{s}");
    assert!(s.contains("claude · api · sess-live · turn 4"), "{s}");
    assert!(s.contains("[repo: /src/api] (R)"), "{s}");
    assert!(s.contains("coverage: 3 sources"), "{s}");
    assert!(s.contains("claude(1)"), "{s}");
    assert!(s.contains("90d retention"), "{s}");
    // Actions are labelled exactly, and only the available ones are offered.
    assert!(
        s.contains("[o] Focus live pane  [c] Start new agent with context"),
        "{s}"
    );
    app.on_key(ch('j'));
    let s = screen(&app);
    assert!(
        s.contains("[r] Resume native session  [c] Start new agent with context"),
        "{s}"
    );
    // Filter chips: repo off, harness claude, date 7d — each re-runs the search.
    app.on_key(ch('R'));
    let (_, p) = only(&commands(&mut rxs[0]), "desk.search");
    assert_eq!(p, json!({"text": "login"}));
    app.on_key(ch('h'));
    let (_, p) = only(&commands(&mut rxs[0]), "desk.search");
    assert_eq!(p["harness"], "claude");
    app.on_key(ch('D'));
    let (_, p) = only(&commands(&mut rxs[0]), "desk.search");
    assert_eq!(p["since"], "7d");
    assert!(screen(&app).contains("[harness: claude] (h)  [date: 7d] (D)"));
    // Sessions tab.
    app.on_key(named(NamedKey::Tab));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.sessions");
    assert!(p.get("since").is_none());
    reply(
        &mut app,
        0,
        req,
        json!({"sessions": [{"session": "sess-old", "harness": "codex", "repo": "/src/api", "last_ts": 1, "turns": 7, "status": "resumable"}]}),
    );
    assert!(screen(&app).contains("7 turns"));
}

#[test]
fn focus_live_pane_only_when_live_and_explicit() {
    let (mut app, mut rxs) = fleet();
    open_and_search(&mut app, &mut rxs[0]);
    // A resumable session can't be focused: nothing is sent.
    app.on_key(ch('j'));
    app.on_key(ch('o'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("Not live"));
    app.on_key(ch('k'));
    app.on_key(ch('o'));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.open");
    assert_eq!(p["focus"], true);
    assert_eq!(p["session"], "sess-live");
    app.machines[0].focus.pane = Some("p2".into());
    reply(
        &mut app,
        0,
        req,
        json!({"status": "live", "focused": true, "live": {"run": "r1", "pane": "p1"}}),
    );
    assert!(matches!(app.mode, Mode::Normal));
    assert_eq!(app.machines[0].focus.pane.as_deref(), Some("p1"));
}

#[test]
fn enter_opens_the_turn_without_focusing() {
    let (mut app, mut rxs) = fleet();
    open_and_search(&mut app, &mut rxs[0]);
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.open");
    assert!(p.get("focus").is_none());
    assert_eq!(p["turn"], 4);
    reply(
        &mut app,
        0,
        req,
        json!({"status": "live", "turn_items": [{"role": "user", "kind": "prompt", "text": "fix the login redirect"}, {"role": "assistant", "kind": "message", "text": "Done."}]}),
    );
    let s = screen(&app);
    assert!(s.contains("user · prompt"), "{s}");
    assert!(s.contains("fix the login redirect"), "{s}");
    assert!(s.contains("[o] Focus live pane"), "{s}");
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.desk.as_ref().unwrap().sub, Sub::None));
}

#[test]
fn resume_shows_command_and_target() {
    let (mut app, mut rxs) = fleet();
    // The focused pane is the shell (no agent): offered as a target.
    app.machines[0].focus.pane = Some("p2".into());
    open_and_search(&mut app, &mut rxs[0]);
    app.on_key(ch('r')); // live: refused
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("This session is live"));
    app.on_key(ch('j'));
    app.on_key(ch('r'));
    let s = screen(&app);
    assert!(s.contains("Resume native session"), "{s}");
    assert!(s.contains("codex resume sess-old"), "{s}");
    assert!(
        s.contains("[t] a new tab in the session's workspace"),
        "{s}"
    );
    assert!(s.contains("[p] the focused pane"), "{s}");
    app.on_key(ch('p'));
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.resume");
    assert_eq!(
        p,
        json!({"session": "sess-old", "mode": "native", "pane": "p2"})
    );
    reply(
        &mut app,
        0,
        req,
        json!({"mode": "native", "label": "Resume native session", "command": "codex resume sess-old"}),
    );
    assert!(screen(&app).contains("Resume native session: codex resume sess-old"));
    // pane_busy is explained.
    app.on_key(ch('r'));
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.resume");
    assert!(p.get("pane").is_none());
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "session_live"}),
    );
    assert!(screen(&app).contains("The session is live"));
}

#[test]
fn new_agent_with_context_saves_a_draft_and_never_sends() {
    let (mut app, mut rxs) = fleet();
    open_and_search(&mut app, &mut rxs[0]);
    app.on_key(ch('j'));
    app.on_key(ch('c'));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.context");
    assert_eq!(p["session"], "sess-old");
    reply(
        &mut app,
        0,
        req,
        json!({"package": {}, "text": "Objective: fix login"}),
    );
    let s = screen(&app);
    assert!(s.contains("Start new agent with context"), "{s}");
    assert!(s.contains("Objective: fix login"), "{s}");
    // Choose turns: reloads the package.
    app.on_key(ctl('t'));
    typ(&mut app, "1-2");
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.context");
    assert_eq!(p["turns"], "1-2");
    reply(
        &mut app,
        0,
        req,
        json!({"text": "Objective: fix login\nTurn 1"}),
    );
    // Edit, toggle start, save.
    app.on_key(named(NamedKey::Down));
    app.on_key(named(NamedKey::End));
    typ(&mut app, " (edited)");
    app.on_key(ctl('a'));
    assert!(screen(&app).contains("[x] also start codex"));
    app.on_key(ctl('x'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "desk.resume");
    assert_eq!(p["mode"], "new_agent");
    assert_eq!(p["start"], true);
    assert_eq!(p["turns"], "1-2");
    reply(
        &mut app,
        0,
        req,
        json!({"draft": {"id": "d7"}, "package": {}, "sent": false, "run": "r9", "pane": "p9"}),
    );
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "draft.update");
    assert_eq!(p["draft"], "d7");
    assert_eq!(p["text"], "Objective: fix login\nTurn 1 (edited)");
    assert!(
        cmds.iter()
            .all(|c| c.1 != "draft.send" && c.1 != "agent.prompt")
    );
    reply(&mut app, 0, req, json!({"draft": {"id": "d7"}}));
    let s = screen(&app);
    assert!(s.contains("Saved as a draft — nothing was sent"), "{s}");
    assert!(matches!(app.desk.as_ref().unwrap().sub, Sub::None));
}

#[test]
fn forget_needs_confirmation() {
    let (mut app, mut rxs) = fleet();
    open_and_search(&mut app, &mut rxs[0]);
    app.on_key(ch('F'));
    assert!(screen(&app).contains("Forget session sess-live"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('F'));
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rxs[0]), "desk.forget");
    assert_eq!(p, json!({"session": "sess-live"}));
    reply(
        &mut app,
        0,
        req,
        json!({"rows_deleted": 4, "sessions_forgotten": 1}),
    );
    assert!(screen(&app).contains("Forgot 4 row(s)"));
    assert!(commands(&mut rxs[0]).iter().any(|c| c.1 == "desk.search"));
    // esc closes.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(app.desk.is_none());
}

#[test]
fn older_server_is_explained() {
    let (mut app, mut rxs) = fleet();
    app.action("desk", None);
    let (req, _) = only(&commands(&mut rxs[0]), "desk.status");
    reply_err(&mut app, 0, req, "method_not_found", json!({}));
    assert!(screen(&app).contains("no session desk"));
}
