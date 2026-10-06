//! T4 task-details tests (fake control-stream replies only): snapshot + workspace changing,
//! snapshot candidate labels, reviewer prompt review/edit/confirm with its digest, note
//! classification with a required dismissal reason, dependency picker/cycle/remove, effort
//! display with sources and explicit application, Estimate effort through the assist flow, and
//! inbox "blocks N linked tasks".

use super::*;
use crate::app::{App, Mode, Popup};
use crate::drafts::tests::{ch, commands, ctl, named, only, reply, reply_err, screen, typ};
use tokio::sync::mpsc::UnboundedReceiver;
use vk_proto::render::ClientFrame;

type Rx = UnboundedReceiver<ClientFrame>;

fn model_task(id: &str, handle: &str, title: &str) -> vk_proto::model::Task {
    serde_json::from_value(json!({
        "id": id, "handle": handle, "title": title, "slug": title.to_lowercase().replace(' ', "-"),
        "workspace": null, "repo_root": "/src/api", "worktree_path": null, "branch": null,
        "base_ref": null, "port_range": null, "status": "active", "setup_status": null,
        "created_at_ms": 1
    }))
    .unwrap()
}

fn detail() -> Value {
    json!({
        "task": {"id": "t1", "handle": "7", "title": "Fix login redirect", "ownership": "attached",
                 "status": "active", "review_label": "review_available"},
        "intent": {"revision": 2, "title": "Fix login redirect", "objective": "Return users",
                   "criteria": [], "constraints": [], "stop_at": "draft_pr"},
        "bindings": [], "runs": [], "messages": []
    })
}

fn committed_subject() -> Value {
    json!({"id": "subj-1", "kind": "committed", "head_sha": "abc12345ffff", "base_sha": "0000"})
}

/// A T4 package; `subject` replaces the selected subject.
fn package_with(subject: Value, current: bool) -> Value {
    json!({
        "task": "t1", "package_revision": 3, "intent_revision": 2,
        "subject": subject, "subject_current": current, "accept_capable": current,
        "label": "review_available", "criteria": [], "checks": [], "check_runs": [],
        "review_notes": [
            {"id": "n1", "task": "t1", "severity": "blocking", "text": "Token is logged in plain text",
             "category": "agent_claim", "classification": "unassessed", "open_concern": true,
             "run": "rv-run-1", "turn": 2, "author": {"kind": "agent", "id": "rv-run-1"}},
            {"id": "n2", "task": "t1", "severity": "nit", "text": "Rename helper",
             "category": "agent_claim", "classification": "unassessed", "open_concern": false,
             "run": "rv-run-1", "turn": 2, "author": {"kind": "agent", "id": "rv-run-1"}}
        ],
        "reviewer_runs": [{"id": "rv_1", "harness": "claude", "state": "started"}],
        "dependencies": {"depends_on": [], "dependents": [], "blocks_open_tasks": 2},
        "effort": {"set": null, "heuristic": {"effort": "few_minutes", "source": "heuristic",
                   "label": "Heuristic estimate (diff size, files, checks) — not set",
                   "reasons": ["6 file(s), 240 changed line(s)"]}},
        "snapshot": {"available": true, "method": "task.review.snapshot"},
        "actions": {"accept": {"available": current}}
    })
}

fn t4_package() -> Value {
    package_with(committed_subject(), true)
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
    app.machines[0].model.tasks = vec![
        model_task("t1", "7", "Fix login redirect"),
        model_task("t2", "8", "Migrate sessions table"),
        model_task("t3", "9", "Update SSO docs"),
    ];
    (app, rxs)
}

fn sub(app: &App) -> TaskSub {
    app.task_view.as_ref().unwrap().sub.clone()
}

fn view_notice(app: &App) -> String {
    app.task_view
        .as_ref()
        .unwrap()
        .notice
        .clone()
        .unwrap_or_default()
}

// ---- snapshot -------------------------------------------------------------------------------------

#[test]
fn snapshot_success_and_workspace_changing() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    let s = screen(&app);
    assert!(s.contains("s snapshot"), "{s}");
    app.on_key(ch('s'));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.snapshot");
    assert_eq!(p["task"], "t1");
    assert!(
        p["idempotency_key"]
            .as_str()
            .unwrap()
            .contains("-snapshot-")
    );
    assert!(view_notice(&app).contains("Capturing a snapshot"));
    // A second press while in flight sends nothing.
    app.on_key(ch('s'));
    assert!(commands(&mut rxs[0]).is_empty());
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "workspace_changing", "label": WORKSPACE_CHANGING}),
    );
    assert!(matches!(
        sub(&app),
        TaskSub::T4(T4Sub::WorkspaceChanging { .. })
    ));
    let s = screen(&app);
    assert!(s.contains(WORKSPACE_CHANGING), "{s}");
    assert!(s.contains("Nothing was recorded"), "{s}");
    assert!(s.contains("s snapshot again"), "{s}");
    // Try again: a new request (new idempotency key).
    app.on_key(ch('s'));
    let (req, p2) = only(&commands(&mut rxs[0]), "task.review.snapshot");
    assert_ne!(p2["idempotency_key"], p["idempotency_key"]);
    reply(
        &mut app,
        0,
        req,
        json!({"subject": {"id": "subj-snap", "kind": "dirty_snapshot"}, "label": SNAPSHOT_CANDIDATE,
               "snapshot": {"commit": "c0ffee"}, "note": "Stored"}),
    );
    assert_eq!(sub(&app), TaskSub::None);
    assert_eq!(view_notice(&app), format!("✓ {SNAPSHOT_CANDIDATE}"));
    // The view refreshes.
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().any(|c| c.1 == "task.review.get"), "{cmds:?}");
}

#[test]
fn snapshot_unavailable_and_nothing_to_snapshot() {
    let (mut app, mut rxs) = setup();
    let mut pkg = t4_package();
    pkg["snapshot"]["available"] = json!(false);
    open(&mut app, &mut rxs[0], pkg);
    app.on_key(ch('s'));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(view_notice(&app).contains("Nothing to snapshot"));

    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    app.on_key(ch('s'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.review.snapshot");
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "nothing_to_snapshot"}),
    );
    assert!(view_notice(&app).contains("no uncommitted changes"));
}

#[test]
fn snapshot_candidate_labels_and_acceptance() {
    let snap = json!({"id": "subj-snap", "kind": "dirty_snapshot", "head_sha": "abc12345ffff",
                      "snapshot": {"commit": "c0ffee1234567", "tree": "t", "ref_name": "refs/vibeke/snapshots/c0ffee"}});
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], package_with(snap.clone(), true));
    let s = screen(&app);
    assert!(s.contains(SNAPSHOT_CANDIDATE), "{s}");
    assert!(
        s.contains("snapshot of uncommitted work on abc12345"),
        "{s}"
    );
    assert!(s.contains("stored as commit c0ffee1234"), "{s}");
    // A current snapshot is accept-capable: Mark reviewed opens the form.
    app.on_key(ch('m'));
    assert!(matches!(sub(&app), TaskSub::Exceptions(_)));

    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], package_with(snap, false));
    let s = screen(&app);
    assert!(s.contains("Earlier snapshot"), "{s}");
    app.on_key(ch('m'));
    assert_eq!(sub(&app), TaskSub::None);
}

#[test]
fn older_server_without_t4_says_so() {
    let (mut app, mut rxs) = setup();
    let pkg = json!({"task": "t1", "subject": committed_subject(), "label": "review_available",
                     "criteria": [], "checks": []});
    open(&mut app, &mut rxs[0], pkg);
    assert!(!screen(&app).contains("R request reviewer"));
    for k in ['s', 'R', 'N', 'd', 'f'] {
        app.on_key(ch(k));
        assert_eq!(view_notice(&app), NEWER_SERVER, "{k}");
        assert!(commands(&mut rxs[0]).is_empty());
    }
}

// ---- reviewer -------------------------------------------------------------------------------------

fn prepared(prompt: &str, id: &str, digest: &str) -> Value {
    json!({"request": {"id": id, "state": "prepared"}, "prompt": prompt, "prompt_digest": digest,
           "harness": "claude", "subject": "subj-1", "requires_confirmation": true,
           "label": "Nothing is launched or sent until you confirm this exact prompt.",
           "uses_provider": "Runs claude with your own account; it consumes that provider's usage.",
           "confirm_with": {"method": "task.review.start_reviewer", "params": {"request": id, "prompt_digest": digest}}})
}

#[test]
fn reviewer_prompt_is_shown_and_started_with_its_digest() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    app.on_key(ch('R'));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.request_reviewer");
    assert_eq!(p["task"], "t1");
    assert!(p.get("prompt").is_none());
    assert!(screen(&app).contains("Preparing the reviewer prompt"));
    reply(
        &mut app,
        0,
        req,
        prepared("Review this diff.\nReply FINDING lines.", "rv_1", "dg-1"),
    );
    let s = screen(&app);
    for want in [
        "Request reviewer",
        "Exact prompt",
        "Review this diff.",
        "Reply FINDING lines.",
        "consumes that provider's usage",
        "ctrl+s start the reviewer with this exact prompt",
    ] {
        assert!(s.contains(want), "missing {want:?}\n{s}");
    }
    app.on_key(ctl('s'));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.start_reviewer");
    assert_eq!(p["request"], "rv_1");
    assert_eq!(p["prompt_digest"], "dg-1");
    reply(
        &mut app,
        0,
        req,
        json!({"request": {"id": "rv_1"}, "run": "run-rev-1", "binding": {"id": "b9"}}),
    );
    assert_eq!(sub(&app), TaskSub::None);
    assert!(view_notice(&app).contains("Reviewer started"));
}

#[test]
fn edited_prompt_is_recorded_then_started_only_if_exact() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    app.on_key(ch('R'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.review.request_reviewer");
    reply(&mut app, 0, req, prepared("Review.", "rv_1", "dg-1"));
    typ(&mut app, " Focus on auth.");
    assert!(screen(&app).contains("edited"));
    app.on_key(ctl('s'));
    // Nothing starts with the old digest: the edited text is recorded first.
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().all(|c| c.1 != "task.review.start_reviewer"));
    let (req, p) = only(&cmds, "task.review.request_reviewer");
    assert_eq!(p["prompt"], "Review. Focus on auth.");
    // The server recorded exactly that text: start with the new digest.
    reply(
        &mut app,
        0,
        req,
        prepared("Review. Focus on auth.", "rv_2", "dg-2"),
    );
    let (_, p) = only(&commands(&mut rxs[0]), "task.review.start_reviewer");
    assert_eq!(p["request"], "rv_2");
    assert_eq!(p["prompt_digest"], "dg-2");

    // A server that recorded something else: shown again, not started.
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    app.on_key(ch('R'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.review.request_reviewer");
    reply(&mut app, 0, req, prepared("Review.", "rv_1", "dg-1"));
    typ(&mut app, "!");
    app.on_key(ctl('s'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.review.request_reviewer");
    reply(&mut app, 0, req, prepared("Review.", "rv_3", "dg-3"));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains("recorded a different text"));
    // Esc: nothing launched.
    app.on_key(named(NamedKey::Escape));
    assert_eq!(sub(&app), TaskSub::None);
    assert!(view_notice(&app).contains("nothing was launched"));
    assert!(commands(&mut rxs[0]).is_empty());
}

#[test]
fn reviewer_refusals() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    app.on_key(ch('R'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.review.request_reviewer");
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "verification_unbound"}),
    );
    assert_eq!(sub(&app), TaskSub::None);
    assert!(view_notice(&app).starts_with("Reviewer not prepared"));

    app.on_key(ch('R'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.review.request_reviewer");
    reply(&mut app, 0, req, prepared("Review.", "rv_1", "dg-1"));
    app.on_key(ctl('s'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.review.start_reviewer");
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "prompt_mismatch"}),
    );
    assert!(view_notice(&app).contains("nothing was launched"));
}

// ---- notes ----------------------------------------------------------------------------------------

#[test]
fn notes_classify_with_required_dismissal_reason() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    // Detail view summary.
    let s = screen(&app);
    assert!(
        s.contains("Review notes · 2 notes · 1 need your decision"),
        "{s}"
    );
    app.on_key(ch('N'));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.notes");
    assert_eq!(p["task"], "t1");
    let pkg = t4_package();
    reply(
        &mut app,
        0,
        req,
        json!({"task": "t1", "notes": pkg["review_notes"], "reviewer_runs": pkg["reviewer_runs"]}),
    );
    let s = screen(&app);
    for want in [
        NOTE_LABEL,
        "BLOCKING",
        "Token is logged in plain text",
        "unassessed · needs your decision",
        "NIT",
        "x dismiss (reason required)",
    ] {
        assert!(s.contains(want), "missing {want:?}\n{s}");
    }
    // Dismiss without a reason: refused locally, nothing sent.
    app.on_key(ch('x'));
    assert!(screen(&app).contains("reason (required)"));
    app.on_key(named(NamedKey::Enter));
    assert!(commands(&mut rxs[0]).is_empty());
    assert!(screen(&app).contains(DISMISS_NEEDS_REASON));
    typ(&mut app, "false positive: test fixture");
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.note.classify");
    assert_eq!(p["note"], "n1");
    assert_eq!(p["classification"], "dismissed");
    assert_eq!(p["reason"], "false positive: test fixture");
    assert!(p["idempotency_key"].is_string());
    reply(
        &mut app,
        0,
        req,
        json!({"note": {"id": "n1", "classification": "dismissed"}}),
    );
    assert_eq!(view_notice(&app), "Note marked dismissed");
    let cmds = commands(&mut rxs[0]);
    assert!(cmds.iter().any(|c| c.1 == "task.review.notes"));

    // Blocking on the second note: reason optional.
    app.on_key(ch('j'));
    app.on_key(ch('b'));
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.review.note.classify");
    assert_eq!(p["note"], "n2");
    assert_eq!(p["classification"], "blocking");
    assert!(p.get("reason").is_none());
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "classification_refused"}),
    );
    assert!(screen(&app).contains("conflict!"));
}

// ---- dependencies ---------------------------------------------------------------------------------

#[test]
fn dependencies_add_cycle_and_remove() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    assert!(screen(&app).contains("Dependencies · blocks 2 linked tasks"));
    app.on_key(ch('d'));
    let (req, _) = only(&commands(&mut rxs[0]), "task.dependency.list");
    reply(
        &mut app,
        0,
        req,
        json!({"task": "t1", "dependencies": {
            "depends_on": [{"edge": {"id": "e1", "task": "t1", "depends_on": "t2", "kind": "blocks"}, "title": "Migrate sessions table"}],
            "dependents": [{"edge": {"id": "e2", "task": "t3", "depends_on": "t1", "kind": "related"}, "title": "Update SSO docs"}],
            "blocks_open_tasks": 1}}),
    );
    let s = screen(&app);
    assert!(
        s.contains("waits for #8 Migrate sessions table  (blocks)"),
        "{s}"
    );
    assert!(
        s.contains("#9 Update SSO docs waits for this  (related)"),
        "{s}"
    );
    assert!(s.contains("blocks 1 linked task "), "{s}");
    // Add through the picker: this task waits for #9.
    app.on_key(ch('a'));
    let s = screen(&app);
    assert!(s.contains("This task waits for…"), "{s}");
    assert!(s.contains("#8 Migrate sessions table"), "{s}");
    assert!(
        !s.contains("  #7 Fix login redirect"),
        "self is not offered\n{s}"
    );
    typ(&mut app, "sso");
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.dependency.add");
    assert_eq!(p["task"], "t1");
    assert_eq!(p["depends_on"], "t3");
    assert_eq!(p["kind"], "blocks");
    reply_err(
        &mut app,
        0,
        req,
        "conflict",
        json!({"reason": "dependency_cycle", "path": ["t1", "t3", "t1"]}),
    );
    let s = screen(&app);
    assert!(
        s.contains("dependency cycle: #7 Fix login redirect → #9 Update SSO docs →"),
        "{s}"
    );
    // Reverse direction + related kind.
    app.on_key(ch('A'));
    app.on_key(named(NamedKey::Tab));
    assert!(screen(&app).contains("…waits for this task · kind ‹ related ›"));
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.dependency.add");
    assert_eq!(p["task"], "t2");
    assert_eq!(p["depends_on"], "t1");
    assert_eq!(p["kind"], "related");
    reply(&mut app, 0, req, json!({"edge": {"id": "e3"}}));
    assert!(view_notice(&app).contains("Link confirmed"));
    let cmds = commands(&mut rxs[0]);
    let (req, _) = only(&cmds, "task.dependency.list");
    reply(
        &mut app,
        0,
        req,
        json!({"task": "t1", "dependencies": {
            "depends_on": [{"edge": {"id": "e1", "task": "t1", "depends_on": "t2", "kind": "blocks"}}],
            "dependents": [], "blocks_open_tasks": 0}}),
    );
    // Remove the selected link after y.
    app.on_key(ch('x'));
    assert!(screen(&app).contains("Remove “waits for #8"));
    app.on_key(ch('n'));
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('x'));
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rxs[0]), "task.dependency.remove");
    assert_eq!(p["edge"], "e1");
    reply(&mut app, 0, req, json!({"removed": []}));
    assert!(view_notice(&app).contains("Link removed"));
}

#[test]
fn dependency_errors_in_words() {
    let (app, _) = setup();
    let e = |reason: &str| crate::app::RpcErr {
        message: "x".into(),
        details: json!({"reason": reason}),
        kind: "conflict".into(),
    };
    assert!(dep_error_text(&app, 0, &e("self_dependency")).contains("itself"));
    assert!(dep_error_text(&app, 0, &e("duplicate")).contains("already exists"));
    assert!(dep_error_text(&app, 0, &e("dependency_cycle")).contains("cycle"));
}

// ---- effort ---------------------------------------------------------------------------------------

#[test]
fn effort_shows_sources_and_applies_only_on_action() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    let s = screen(&app);
    assert!(s.contains("set: not set"), "{s}");
    assert!(
        s.contains("estimate: a few minutes · source: heuristic — not applied"),
        "{s}"
    );
    app.on_key(ch('f'));
    let s = screen(&app);
    assert!(
        s.contains("Heuristic estimate: a few minutes · source: heuristic"),
        "{s}"
    );
    assert!(s.contains("6 file(s), 240 changed line(s)"), "{s}");
    assert!(s.contains("Model estimate: none"), "{s}");
    assert!(commands(&mut rxs[0]).is_empty(), "opening applies nothing");
    // m without a model estimate: nothing sent.
    app.on_key(ch('m'));
    assert!(commands(&mut rxs[0]).is_empty());
    // Apply the heuristic.
    app.on_key(ch('h'));
    let (req, p) = only(&commands(&mut rxs[0]), "task.set");
    assert_eq!(p["task"], "t1");
    assert_eq!(p["effort"], "minutes");
    assert_eq!(p["effort_source"], "heuristic");
    reply(&mut app, 0, req, json!({"task": {"id": "t1"}}));
    assert!(
        view_notice(&app).contains("Effort set to a few minutes (from the heuristic estimate)")
    );
    assert_eq!(sub(&app), TaskSub::None);
    // Manual choice.
    app.on_key(ch('f'));
    app.on_key(ch('j'));
    app.on_key(ch('j'));
    app.on_key(named(NamedKey::Enter));
    let (_, p) = only(&commands(&mut rxs[0]), "task.set");
    assert_eq!(p["effort"], "deep");
    assert_eq!(p["effort_source"], "user");
}

#[test]
fn estimate_effort_runs_the_assist_flow_and_applies_on_enter() {
    let (mut app, mut rxs) = setup();
    open(&mut app, &mut rxs[0], t4_package());
    app.on_key(ch('E'));
    assert!(matches!(app.mode, Mode::Popup(Popup::Assist)));
    let (req, p) = only(&commands(&mut rxs[0]), "assistant.generate");
    assert_eq!(p["operation"], "effort_estimate");
    assert_eq!(p["task"], "t1");
    reply(
        &mut app,
        0,
        req,
        json!({"request": {"id": "as_9"}, "requires_confirmation": true,
               "preview": {"digest": "dg-9", "system": "S", "user": "U", "model": "m",
                           "endpoint_host": "h", "bytes": 10, "sources": [], "omitted": []}}),
    );
    // Nothing is sent before the preview is confirmed.
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(ch('y'));
    let (req, p) = only(&commands(&mut rxs[0]), "assistant.confirm");
    assert_eq!(p["preview_digest"], "dg-9");
    reply(&mut app, 0, req, json!({"request": {"id": "as_9"}}));
    app.assist.as_mut().unwrap().last_poll = None;
    crate::assist::tick(&mut app);
    let (req, _) = only(&commands(&mut rxs[0]), "assistant.get");
    reply(
        &mut app,
        0,
        req,
        json!({"request": {"id": "as_9", "state": "done",
               "output": {"effort": "deep", "rationale": "Auth code and a failing check", "applied": false,
                          "estimate_source": "assistant"}}}),
    );
    let s = screen(&app);
    assert!(
        s.contains("Estimated effort: deep review (source: assistant — not applied)"),
        "{s}"
    );
    assert!(s.contains("enter apply this estimate"), "{s}");
    // The estimate is kept on the task view with its source, unapplied.
    let m = app
        .task_view
        .as_ref()
        .unwrap()
        .t4
        .model_estimate
        .clone()
        .unwrap();
    assert_eq!(m["effort"], "deep");
    assert_eq!(m["request"], "as_9");
    assert!(commands(&mut rxs[0]).is_empty());
    app.on_key(named(NamedKey::Enter));
    let (req, p) = only(&commands(&mut rxs[0]), "task.set");
    assert_eq!(p["effort"], "deep");
    assert_eq!(p["effort_source"], "assistant:as_9");
    reply(&mut app, 0, req, json!({"task": {}}));
    assert!(screen(&app).contains("Effort set from the assistant's estimate"));
    // Back in task details, the model estimate shows with its source; `m` applies it again.
    app.on_key(named(NamedKey::Escape));
    assert!(matches!(app.mode, Mode::Popup(Popup::Task)));
    assert!(screen(&app).contains("estimate: deep review · source: assistant — not applied"));
    app.on_key(ch('f'));
    app.on_key(ch('m'));
    let (_, p) = only(&commands(&mut rxs[0]), "task.set");
    assert_eq!(p["effort_source"], "assistant:as_9");
}

#[test]
fn effort_param_maps_estimate_values() {
    assert_eq!(effort_param("few_minutes"), "minutes");
    assert_eq!(effort_param("deep_review"), "deep");
    assert_eq!(effort_param("quick"), "quick");
    assert_eq!(effort_param("bogus"), "unknown");
    assert_eq!(effort_label("minutes"), "a few minutes");
    assert_eq!(blocks_text(1), "blocks 1 linked task");
    assert_eq!(blocks_text(3), "blocks 3 linked tasks");
}

// ---- inbox ----------------------------------------------------------------------------------------

#[test]
fn inbox_rows_show_blocks_n_linked_tasks_and_estimates() {
    let v = json!({"key": {"kind": "review", "id": "t2"}, "class": 4, "title": "Migrate sessions",
                   "task": "t2", "explanation": "Review available", "age_ms": 1000,
                   "effort": "unknown", "effort_estimate": {"effort": "quick", "source": "heuristic"},
                   "blocks_tasks": 2});
    let it = crate::inbox::parse_item(0, &v).unwrap();
    assert_eq!(it.blocks_tasks, 2);
    assert_eq!(
        it.effort_estimate,
        Some(("quick".to_string(), "heuristic".to_string()))
    );
    assert_eq!(crate::inbox::blocks_suffix(&it), " · blocks 2 linked tasks");
    // The server's explanation already saying it isn't repeated.
    let mut v2 = v.clone();
    v2["explanation"] = json!("Review available · blocks 2 linked tasks");
    let it2 = crate::inbox::parse_item(0, &v2).unwrap();
    assert_eq!(crate::inbox::blocks_suffix(&it2), "");
}
