use super::*;
use crate::app::test_run;
use crate::drafts::tests::{commands, fleet, only, pane, reply, reply_err, screen};
use vk_proto::input::Mods;

fn key(app: &mut App, k: Key) {
    app.on_key(KeyEvent::new(k, Mods::empty()));
}

fn rec_json(id: &str, sev: &str, runs: &[(&str, &str, &str, &str)], path: &str) -> Value {
    let info: Vec<Value> = runs
        .iter()
        .map(|(run, handle, harness, pane)| {
            json!({"run": run, "handle": handle, "name": null, "harness": harness, "pane": pane, "task": null, "state": "working", "alive": true})
        })
        .collect();
    json!({
        "id": id, "root": "/src/api", "severity": sev, "status": "open",
        "runs": runs.iter().map(|r| r.0).collect::<Vec<_>>(), "run_info": info,
        "paths": [{"path": path, "severity": sev, "reason": {"kind": "same_file"}, "runs": [], "ambiguous": false, "first_ms": 1, "last_ms": 2}],
        "ambiguous": false, "first_ms": 1, "last_ms": 2,
        "headline": format!("{} agents editing {path}", runs.len()),
        "cleared_ms": null, "cleared_reason": null,
    })
}

fn list_of(recs: Vec<Value>) -> Value {
    json!({"collisions": recs, "enabled": true})
}

/// p1 (r1 claude, focused) and p3 (r3 codex) in workspace W1.
fn setup() -> (
    App,
    Vec<tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>>,
) {
    let (mut app, rxs) = fleet();
    let m = &mut app.machines[0];
    m.model.panes.push(pane("p3", "T1", "W1"));
    m.model.runs.push(test_run("r3", "p3", "codex"));
    (app, rxs)
}

fn first_list(
    app: &mut App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<vk_proto::render::ClientFrame>,
    recs: Vec<Value>,
) {
    tick(app, Instant::now());
    let cmds = commands(rx);
    let (req, p) = only(&cmds, "collision.list");
    assert_eq!(p, json!({"status": "open"}));
    reply(app, 0, req, list_of(recs));
}

fn high() -> Value {
    rec_json(
        "col_1",
        "high",
        &[("r1", "a1", "claude", "p1"), ("r3", "a3", "codex", "p3")],
        "src/auth.ts",
    )
}

fn detail(extra_steer: bool) -> Value {
    let mut c = high();
    c["timeline"] = json!([
        {"at_ms": now_ms() - 5000, "run": "r1", "candidates": [], "path": "src/auth.ts", "what": "modify", "source": "adapter"},
        {"at_ms": now_ms() - 1000, "run": null, "candidates": ["r1", "r3"], "path": "src/auth.ts", "what": "modify", "source": "watcher"},
    ]);
    json!({
        "collision": c,
        "claims": [],
        "steer": [
            {"run": "r1", "channel": "hook_context"},
            if extra_steer { json!({"run": "r3", "channel": null, "reason": "this harness has no native steer or follow-up channel"}) } else { json!({"run": "r3", "channel": "headless_steer"}) },
        ],
    })
}

#[test]
fn the_list_is_read_once_then_after_events_and_every_thirty_seconds() {
    let (mut app, mut rxs) = setup();
    first_list(&mut app, &mut rxs[0], vec![]);
    // Nothing more until an event or the refresh period.
    tick(&mut app, Instant::now());
    assert!(commands(&mut rxs[0]).is_empty());
    on_event(&mut app, 0, "task.collision_detected");
    tick(&mut app, Instant::now());
    let cmds = commands(&mut rxs[0]);
    assert_eq!(cmds.iter().filter(|c| c.1 == "collision.list").count(), 1);
    // An unrelated event does nothing.
    on_event(&mut app, 0, "agent.started");
    assert!(app.ux.collision.stale.is_empty());
    // The period.
    let (req, _) = only(&cmds, "collision.list");
    reply(&mut app, 0, req, list_of(vec![]));
    tick(&mut app, Instant::now() + REFRESH + Duration::from_secs(1));
    assert_eq!(
        commands(&mut rxs[0])
            .iter()
            .filter(|c| c.1 == "collision.list")
            .count(),
        1
    );
}

#[test]
fn a_server_without_the_method_is_left_alone() {
    let (mut app, mut rxs) = setup();
    tick(&mut app, Instant::now());
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "collision.list");
    reply_err(&mut app, 0, req, "method_not_found", json!({}));
    on_event(&mut app, 0, "task.collision_detected");
    tick(&mut app, Instant::now() + REFRESH * 2);
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn collisions_mark_the_agent_rows_the_sidebar_and_the_pane_frame() {
    let (mut app, mut rxs) = setup();
    assert!(!crate::draw::agent_row_text(&app, 0, "r1").contains('⚠'));
    first_list(&mut app, &mut rxs[0], vec![high()]);
    assert!(crate::draw::agent_row_text(&app, 0, "r1").contains('⚠'));
    assert!(crate::draw::agent_row_text(&app, 0, "r3").contains('⚠'));
    let rows: Vec<String> = crate::draw::sidebar_rows(&app)
        .iter()
        .map(|r| r.segs.iter().map(|(s, _)| s.as_str()).collect::<String>())
        .collect();
    assert!(
        rows.iter()
            .any(|r| r.contains("⚠ 2 agents editing src/auth.ts")),
        "{rows:#?}"
    );
    assert_eq!(pane_badge(&app, 0, "p1"), Some("⚠"));
    assert_eq!(
        pane_badge(&app, 0, "p2"),
        None,
        "the shell pane is not in it"
    );
    let s = screen(&app);
    assert!(s.contains('⚠'), "{s}");
    // A run that is no longer alive does not keep the badge.
    app.ux.collision.recs.get_mut(&0).unwrap()[0].runs[0].alive = false;
    assert_eq!(pane_badge(&app, 0, "p1"), None);
    assert_eq!(pane_badge(&app, 0, "p3"), Some("⚠"));
}

#[test]
fn the_same_directory_level_is_a_dim_hint_without_a_frame_badge() {
    let (mut app, mut rxs) = setup();
    let low = rec_json(
        "col_2",
        "low",
        &[("r1", "a1", "claude", "p1"), ("r3", "a3", "codex", "p3")],
        "src/auth/login.ts",
    );
    first_list(&mut app, &mut rxs[0], vec![low]);
    assert_eq!(pane_badge(&app, 0, "p1"), None);
    assert_eq!(agent_marker(&app, 0, "p1"), Some(("~", 1)));
    let rows: Vec<String> = crate::draw::sidebar_rows(&app)
        .iter()
        .map(|r| r.segs.iter().map(|(s, _)| s.as_str()).collect::<String>())
        .collect();
    assert!(
        rows.iter()
            .any(|r| r.contains("~ 2 agents editing src/auth/login.ts")),
        "{rows:#?}"
    );
    assert!(!crate::draw::agent_row_text(&app, 0, "r1").contains('⚠'));
}

#[test]
fn a_disconnect_forgets_the_list() {
    let (mut app, mut rxs) = setup();
    first_list(&mut app, &mut rxs[0], vec![high()]);
    assert!(pane_rank(&app, 0, "p1") >= 3);
    on_disconnected(&mut app, 0);
    assert_eq!(pane_rank(&app, 0, "p1"), 0);
}

#[test]
fn the_popup_lists_paths_runs_timeline_and_how_each_run_can_be_told() {
    let (mut app, mut rxs) = setup();
    first_list(&mut app, &mut rxs[0], vec![high()]);
    app.action("collisions", None);
    assert!(matches!(app.mode, Mode::Popup(Popup::Collision)));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "collision.get");
    assert_eq!(p, json!({"collision": "col_1"}));
    reply(&mut app, 0, req, detail(true));
    let s = screen(&app);
    assert!(
        s.contains("collision · 2 agents editing src/auth.ts"),
        "{s}"
    );
    assert!(s.contains("advisory"), "{s}");
    assert!(s.contains("claude (a1)") && s.contains("codex (a3)"), "{s}");
    assert!(s.contains("tell: hook_context"), "{s}");
    assert!(
        s.contains("tell: no (this harness has no native steer"),
        "{s}"
    );
    assert!(s.contains("src/auth.ts") && s.contains("same file"), "{s}");
    assert!(
        s.contains("one of r1/r3"),
        "ambiguous timeline entries name the candidates: {s}"
    );
    assert!(s.contains("[watcher]") && s.contains("[adapter]"), "{s}");
    key(&mut app, Key::Named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn pause_tell_and_ignore_send_their_methods() {
    let (mut app, mut rxs) = setup();
    first_list(&mut app, &mut rxs[0], vec![high()]);
    app.action("collisions", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "collision.get");
    reply(&mut app, 0, req, detail(false));
    // Row 0 is the first run (r1): pause it.
    key(&mut app, Key::Char('p'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "collision.pause");
    assert_eq!(p, json!({"collision": "col_1", "run": "r1"}));
    reply(&mut app, 0, req, json!({"run": null, "paused": true}));
    assert!(screen(&app).contains("✓ interrupted r1"));
    // Tell the agents.
    key(&mut app, Key::Char('t'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "collision.tell");
    assert_eq!(p, json!({"collision": "col_1"}));
    reply(
        &mut app,
        0,
        req,
        json!({"results": [
            {"run": "r1", "status": "queued", "channel": "hook_context"},
            {"run": "r3", "status": "unsupported", "channel": null, "reason": "ACP has no steering"}
        ], "text": "Note: x"}),
    );
    let s = screen(&app);
    assert!(
        s.contains("r1 queued") && s.contains("r3 unsupported (ACP has no steering)"),
        "{s}"
    );
    // Pause needs a run row; on a path row it says so. Rows: r1, r3, then the path.
    key(&mut app, Key::Char('j'));
    key(&mut app, Key::Char('j'));
    key(&mut app, Key::Char('p'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("select an agent to pause"));
    // Ignore the selected path.
    key(&mut app, Key::Char('i'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "collision.ignore");
    assert_eq!(p, json!({"collision": "col_1", "path": "src/auth.ts"}));
    reply(
        &mut app,
        0,
        req,
        json!({"ignore": {"id": "ign_1"}, "collision": null}),
    );
    // The view reloads the record (it may have closed) and the list is refreshed.
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().any(|c| c.1 == "collision.get"));
    assert!(app.ux.collision.stale.contains(&0));
    // On a run row, ignore says so.
    key(&mut app, Key::Char('k'));
    key(&mut app, Key::Char('k'));
    key(&mut app, Key::Char('i'));
    assert!(screen(&app).contains("select a path to ignore"));
}

#[test]
fn a_fresh_task_is_confirmed_after_showing_the_hand_off() {
    let (mut app, mut rxs) = setup();
    first_list(&mut app, &mut rxs[0], vec![high()]);
    app.action("collisions", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "collision.get");
    reply(&mut app, 0, req, detail(false));
    key(&mut app, Key::Char('f'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "collision.start_task");
    assert_eq!(p, json!({"collision": "col_1", "dry_run": true}));
    reply(
        &mut app,
        0,
        req,
        json!({"created": false, "base": "0123456789abcdef", "title": "Split auth", "harness": "claude", "prompt": "This task was split off from a shared checkout.\nline two", "source_run": "r1", "result": null}),
    );
    let s = screen(&app);
    assert!(s.contains("start a fresh task \"Split auth\""), "{s}");
    assert!(s.contains("0123456789ab"), "{s}");
    assert!(s.contains("nothing is moved"), "{s}");
    assert!(
        s.contains("This task was split off"),
        "the hand-off is shown first: {s}"
    );
    // Any other key cancels; nothing is created.
    key(&mut app, Key::Char('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("not created"));
    // Again, and confirm: the shown prompt and title are what is sent.
    key(&mut app, Key::Char('f'));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "collision.start_task");
    reply(
        &mut app,
        0,
        req,
        json!({"created": false, "base": "0123456789abcdef", "title": "T", "harness": "claude", "prompt": "P", "source_run": "r1", "result": null}),
    );
    key(&mut app, Key::Char('y'));
    let cmds = commands(&mut rxs[0]);
    let (req, p) = only(&cmds, "collision.start_task");
    assert_eq!(
        p,
        json!({"collision": "col_1", "title": "T", "harness": "claude", "prompt": "P", "run": "r1"})
    );
    reply(
        &mut app,
        0,
        req,
        json!({"created": true, "base": "0123456789abcdef", "title": "T", "harness": "claude", "prompt": "P", "source_run": "r1", "result": {"task": {"handle": "k7"}}}),
    );
    assert!(screen(&app).contains("task #k7 created"));
}

#[test]
fn the_popup_cycles_through_collisions_and_with_none_says_so() {
    let (mut app, mut rxs) = setup();
    app.action("collisions", None);
    assert!(matches!(app.mode, Mode::Normal), "no collisions: no popup");
    assert!(
        app.toasts.iter().any(|t| t.text.contains("no collisions")),
        "a toast says so"
    );
    let second = rec_json(
        "col_2",
        "medium",
        &[("r1", "a1", "claude", "p1"), ("r3", "a3", "codex", "p3")],
        "api/auth.ts",
    );
    first_list(&mut app, &mut rxs[0], vec![second, high()]);
    app.action("collisions", None);
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "collision.get");
    assert_eq!(p["collision"], "col_1", "the most severe first");
    key(&mut app, Key::Char(']'));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "collision.get");
    assert_eq!(p["collision"], "col_2");
    key(&mut app, Key::Char('['));
    let cmds = commands(&mut rxs[0]);
    let (_, p) = only(&cmds, "collision.get");
    assert_eq!(p["collision"], "col_1");
}

#[test]
fn opening_the_pane_of_a_run_closes_the_popup() {
    let (mut app, mut rxs) = setup();
    first_list(&mut app, &mut rxs[0], vec![high()]);
    app.action("collisions", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "collision.get");
    reply(&mut app, 0, req, detail(false));
    key(&mut app, Key::Char('j'));
    key(&mut app, Key::Char('o'));
    assert!(matches!(app.mode, Mode::Normal));
}

#[test]
fn an_error_for_the_record_is_shown_not_hidden() {
    let (mut app, mut rxs) = setup();
    first_list(&mut app, &mut rxs[0], vec![high()]);
    app.action("collisions", None);
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "collision.get");
    reply_err(&mut app, 0, req, "not_found", json!({}));
    let s = screen(&app);
    assert!(s.contains("✗"), "{s}");
}

#[test]
fn records_parse_defensively() {
    assert!(Rec::from_value(&json!({})).is_none());
    let r = Rec::from_value(&json!({"id": "c"})).unwrap();
    assert!(r.runs.is_empty() && r.paths.is_empty() && r.severity.is_empty());
    assert_eq!(
        (rank("high"), rank("medium"), rank("low"), rank("x")),
        (3, 2, 1, 0)
    );
}
