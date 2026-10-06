//! Lane 2C task-details tests (fake control-stream replies only): human review of a human
//! criterion (note required for a failure, expected subject sent, refusal shown), selecting
//! changes for a selected-patch snapshot, review lines, and the Link run step of Track.

use super::*;
use crate::app::{App, Mode, Popup};
use crate::drafts::tests::{ch, commands, named, only, reply, reply_err, screen, typ};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::input::NamedKey;
use vk_proto::render::ClientFrame;

type Rx = UnboundedReceiver<ClientFrame>;

fn detail() -> Value {
    json!({
        "task": {"id": "t1", "handle": "7", "title": "Fix login redirect", "ownership": "attached",
                 "status": "active", "review_label": "review_available",
                 "repo_root": "/src/api", "worktree_path": "/src/api-wt"},
        "intent": {"revision": 2, "title": "Fix login redirect", "objective": "Return users",
                   "criteria": [], "constraints": [], "stop_at": "draft_pr"},
        "bindings": [], "runs": [], "messages": []
    })
}

fn package() -> Value {
    json!({
        "task": "t1", "package_revision": 3, "intent_revision": 2,
        "intent": {"criteria": [
            {"id": "c-h", "text": "Looks right", "evaluation": "human", "required": true},
            {"id": "c-c", "text": "Tests pass", "evaluation": "check", "required": true}
        ]},
        "subject": {"id": "subj-1", "kind": "selected_patch", "head_sha": "abc12345ffff",
                    "selection": {"mode": "paths", "paths": ["src/login.rs"], "excludes_other_changes": true}},
        "subject_current": true, "accept_capable": true,
        "label": "review_available", "criteria": [], "checks": [], "check_runs": [],
        "review_notes": [], "reviewer_runs": [],
        "dependencies": {"depends_on": [], "dependents": [], "blocks_open_tasks": 0},
        "effort": {"set": null, "heuristic": null},
        "snapshot": {"available": true, "method": "task.review.snapshot"},
        "human_reviews": [
            {"id": "hr1", "criterion_id": "c-h", "subject_id": "subj-1", "verdict": "supported",
             "label": "Reviewed by you · supports this criterion on this revision", "note": "Redirect works"}
        ],
        "purged": null,
        "actions": {"accept": {"available": true}}
    })
}

fn open(app: &mut App, rx: &mut Rx, pkg: Value) {
    crate::tasks::open_task(app, 0, "t1");
    for (req, m, _) in commands(rx) {
        match m.as_str() {
            "task.detail" => reply(app, 0, req, detail()),
            "task.review.get" => reply(app, 0, req, pkg.clone()),
            "task.check.list" => reply(app, 0, req, json!({"checks": []})),
            m => panic!("unexpected {m}"),
        }
    }
}

fn setup() -> (App, Vec<Rx>) {
    let (mut app, rxs) = crate::app::test_app(1);
    app.size = (150, 60);
    (app, rxs)
}

fn sub(app: &App) -> TaskSub {
    app.task_view.as_ref().unwrap().sub.clone()
}

fn notice(app: &App) -> String {
    app.task_view
        .as_ref()
        .unwrap()
        .notice
        .clone()
        .unwrap_or_default()
}

#[test]
fn review_lines_show_selection_and_human_reviews() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], package());
    let s = screen(&app);
    assert!(s.contains("Selected changes"), "{s}");
    assert!(s.contains("src/login.rs"), "{s}");
    assert!(s.contains("Your reviews"), "{s}");
    assert!(s.contains("Looks right"), "{s}");
    assert!(s.contains("H human review"), "{s}");
}

#[test]
fn human_review_needs_a_note_for_a_failure_and_sends_the_subject() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], package());
    app.on_key(ch('H'));
    let TaskSub::Lane2c(Sub::Human(f)) = sub(&app) else {
        panic!("human review form expected");
    };
    // Only human criteria are offered.
    assert_eq!(
        f.criteria,
        vec![("c-h".to_string(), "Looks right".to_string())]
    );
    assert!(screen(&app).contains(HUMAN_NOTE));
    // `n` (does not meet it) needs a note: enter without one is refused locally.
    app.on_key(ch('n'));
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("needs a note"));
    typ(&mut app, "Loses the query string");
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.human_review");
    assert_eq!(p["criterion"], "c-h");
    assert_eq!(p["verdict"], "failed");
    assert_eq!(p["note"], "Loses the query string");
    assert_eq!(p["expected_subject"], "subj-1");
    assert!(
        p["idempotency_key"]
            .as_str()
            .unwrap()
            .contains("-human-review-")
    );
    // A moved subject: the form says so and records nothing.
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "review_changed"}),
    );
    let TaskSub::Lane2c(Sub::Human(f)) = sub(&app) else {
        panic!("form stays open");
    };
    assert!(f.error.unwrap().contains("changed"));
    // Supported, no note: sent; success refreshes the view.
    app.on_key(ch('y'));
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.human_review");
    assert_eq!(p["verdict"], "supported");
    assert!(p.get("note").is_none());
    reply(&mut app, 0, req, json!({"review": {"id": "hr2"}}));
    assert!(matches!(sub(&app), TaskSub::None));
    assert!(notice(&app).contains("Review recorded"));
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().any(|(_, m, _)| m == "task.review.get"));
}

#[test]
fn human_review_needs_an_immutable_subject_and_human_criteria() {
    let (mut app, mut rxs) = setup();
    let mut p = package();
    p["subject"] = json!({"id": "live", "kind": "checkout_live"});
    open(&mut app, &mut rxs[0], p);
    app.on_key(ch('H'));
    assert!(matches!(sub(&app), TaskSub::None));
    assert!(notice(&app).contains("committed revision or snapshot"));
    let (mut app, mut rxs) = setup();
    let mut p = package();
    p["intent"]["criteria"] = json!([{"id": "c-c", "text": "Tests pass", "evaluation": "check"}]);
    open(&mut app, &mut rxs[0], p);
    app.on_key(ch('H'));
    assert!(notice(&app).contains("no human criteria"));
}

#[test]
fn select_changes_captures_only_ticked_files() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], package());
    app.on_key(ch('P'));
    let (req, p) = only(&commands(&mut rxs[0]), "git.status");
    assert_eq!(p["path"], "/src/api-wt");
    reply(
        &mut app,
        0,
        req,
        json!({"files": [
            {"path": "src/login.rs", "secret": false},
            {"path": ".env", "secret": true},
            {"path": "src/other.rs", "secret": false}
        ]}),
    );
    let TaskSub::Lane2c(Sub::Patch(pk)) = sub(&app) else {
        panic!("picker expected");
    };
    assert_eq!(pk.files.len(), 2, "secret files are not offered");
    // Nothing ticked: refused locally.
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch(' '));
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.snapshot");
    assert_eq!(p["paths"], json!(["src/login.rs"]));
    assert!(screen(&app).contains("Capturing the selection"));
    reply(
        &mut app,
        0,
        req,
        json!({"label": "Selected changes (1 selected file(s)) · the rest of the checkout is not part of this review"}),
    );
    assert!(matches!(sub(&app), TaskSub::None));
    assert!(notice(&app).contains("not part of this review"));
}

#[test]
fn link_run_step_lists_verified_runs_and_tracks_the_chosen_one() {
    let (mut app, mut rxs) = crate::app::test_app(1);
    app.size = (120, 40);
    app.machines[0]
        .model
        .runs
        .push(crate::app::test_run("r1", "p1", "claude"));
    crate::tasks::open_track(&mut app, 0, "p1");
    let (req, _) = only(&commands(&mut rxs[0]), "task.sources");
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "binding_unverified"}),
    );
    let (req, p) = only(&commands(&mut rxs[0]), "task.link.status");
    assert_eq!(p["run"], "r1");
    reply(
        &mut app,
        0,
        req,
        json!({
            "verified": false,
            "reasons": ["Detected from the process only: no tested integration is reporting for this agent."],
            "remedies": [{"action": "install_integration", "label": "Install the integration, then restart or resume the agent", "command": "vibeke setup"}],
            "candidates": [{"run": "r2", "pane": "p2", "harness": "claude", "name": "claude", "integration": "hooks", "turns": 3, "same_pane": false}]
        }),
    );
    let s = screen(&app);
    assert!(s.contains("Link run first"), "{s}");
    assert!(s.contains("Detected from the process only"), "{s}");
    assert!(s.contains("vibeke setup"), "{s}");
    assert!(s.contains("Verified runs you can track instead"), "{s}");
    // Choosing the verified run opens Track for it (nothing bound yet).
    app.on_key(named(NamedKey::Enter));
    let f = app.track.as_ref().unwrap();
    assert_eq!(f.run, "r2");
    assert!(matches!(app.mode, Mode::Popup(Popup::Track)));
    let (_, p) = only(&commands(&mut rxs[0]), "task.sources");
    assert_eq!(p["run"], "r2");
}
