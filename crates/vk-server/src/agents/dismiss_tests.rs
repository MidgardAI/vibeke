//! In-process tests for automatic dismissal: a new user turn, another tool starting and a quiet
//! screen close interactions the harness never reports as answered; a delivery that failed
//! before typing anything notifies instead of leaving an inbox item. Runs are created directly,
//! so `bound_run` never spawns a version probe.

use super::*;
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use std::sync::Once;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Same values as the other in-process test modules (one root per binary).
        let base = std::env::temp_dir().join(format!("vk-review-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: identical values to the other writers; set before servers read them.
        unsafe {
            std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
        }
    });
}

struct Env {
    server: Arc<Server>,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    init_env();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let opts = ServerOpts {
        session: "t".into(),
        machine: "testbox".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![],
        shims: false,
        gateway: None,
    };
    Env {
        server: Server::new(paths, opts).unwrap(),
        _dir: dir,
    }
}

fn add_run(e: &Env, id: &str, pane: &str, f: impl FnOnce(&mut AgentRun)) -> AgentRun {
    let mut r = harness_tests_run();
    r.id = id.into();
    r.handle = id.into();
    r.pane = pane.into();
    r.harness_session_id = Some(format!("sess-{id}"));
    f(&mut r);
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.run(r.clone());
    e.server.commit(&mut c, tx).unwrap();
    r
}

/// Open an interaction from a Claude hook payload, opened `age_ms` ago.
fn open(e: &Env, run: &AgentRun, event: &str, p: Value, age_ms: i64) -> String {
    let mut it = harness::interaction_from_hook(Harness::Claude, event, &p).unwrap();
    it.run = run.id.clone();
    it.pane = run.pane.clone();
    it.opened_at_ms = now_ms() - age_ms;
    let id = it.id.clone();
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.interaction(it);
    e.server.commit(&mut c, tx).unwrap();
    id
}

fn ask(e: &Env, run: &AgentRun, tool_use: &str) -> String {
    open(
        e,
        run,
        "PreToolUse",
        json!({"tool_name": "AskUserQuestion", "tool_use_id": tool_use, "tool_input": {"questions": [{"question": "Which database?", "options": [{"label": "Postgres"}, {"label": "SQLite"}], "multiSelect": false}]}}),
        1000,
    )
}

fn approval(e: &Env, run: &AgentRun, tool_use: &str) -> String {
    open(
        e,
        run,
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_use_id": tool_use, "tool_input": {"command": "cargo test"}}),
        1000,
    )
}

fn status(e: &Env, id: &str) -> InteractionStatus {
    e.server
        .with_core(|c| {
            c.interaction(id).cloned().or_else(|| {
                c.store
                    .find::<Interaction>("interaction", id)
                    .ok()
                    .flatten()
            })
        })
        .expect("interaction record")
        .status
}

fn reasons(e: &Env, id: &str) -> Vec<String> {
    e.server.with_core(|c| {
        c.store
            .events_after(0, 10_000, &["interaction.resolved_elsewhere".to_string()])
            .unwrap()
            .into_iter()
            .map(|ev| serde_json::to_value(ev).unwrap())
            .filter(|v| v["subject"]["interaction"] == id)
            .filter_map(|v| v["data"]["reason"].as_str().map(str::to_string))
            .collect()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_prompt_closes_an_open_question() {
    let e = env();
    let run = add_run(&e, "r-ask", "p-ask", |_| {});
    let q = ask(&e, &run, "toolu_q");
    let a = approval(&e, &run, "toolu_a");
    // The user pressed Esc on the question and typed a new prompt: no PostToolUse, no Stop.
    on_signal(
        &e.server,
        "p-ask",
        Harness::Claude,
        "UserPromptSubmit",
        &json!({"session_id": "sess-r-ask", "prompt": "use sqlite instead"}),
    );
    assert_eq!(status(&e, &q), InteractionStatus::ResolvedElsewhere);
    assert_eq!(status(&e, &a), InteractionStatus::ResolvedElsewhere);
    assert_eq!(reasons(&e, &q), vec![USER_MOVED_ON.to_string()]);
    assert!(arbiter::resolution_known(USER_MOVED_ON));
    // The turn boundary is recorded for the inbox.
    let mark = e.server.with_core(|c| {
        c.store
            .find::<TurnMark>(K_TURN_MARK, "r-ask")
            .ok()
            .flatten()
    });
    assert!(mark.is_some_and(|m| m.at_ms > 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_prompt_leaves_headless_requests_to_their_protocol() {
    let e = env();
    let run = add_run(&e, "r-hl", "p-hl", |r| {
        r.integration = "headless:claude".into()
    });
    let q = ask(&e, &run, "toolu_q");
    on_signal(
        &e.server,
        "p-hl",
        Harness::Claude,
        "UserPromptSubmit",
        &json!({"session_id": "sess-r-hl", "prompt": "next"}),
    );
    assert_eq!(status(&e, &q), InteractionStatus::Open);
}

#[tokio::test(flavor = "multi_thread")]
async fn another_tool_closes_a_question_but_not_an_approval() {
    let e = env();
    let run = add_run(&e, "r-tool", "p-tool", |_| {});
    let q = ask(&e, &run, "toolu_q");
    let a = approval(&e, &run, "toolu_a");
    // Without a readable screen a parallel tool proves nothing: the question may still show.
    on_signal(
        &e.server,
        "p-tool",
        Harness::Claude,
        "PreToolUse",
        &json!({"session_id": "sess-r-tool", "tool_name": "Read", "tool_use_id": "toolu_b", "tool_input": {"file_path": "/tmp/x"}}),
    );
    assert_eq!(status(&e, &q), InteractionStatus::Open);
    // With no dialog on screen: the question's own tool starting never closes it.
    close_questions_for_tool(&e.server, &run, "toolu_q");
    assert_eq!(status(&e, &q), InteractionStatus::Open);
    close_questions_for_tool(&e.server, &run, "toolu_b");
    assert_eq!(status(&e, &q), InteractionStatus::ResolvedElsewhere);
    assert_eq!(reasons(&e, &q), vec!["tool ran".to_string()]);
    // Parallel tool calls can each wait for a permission: the approval stays open.
    assert_eq!(status(&e, &a), InteractionStatus::Open);
}

#[tokio::test(flavor = "multi_thread")]
async fn another_tool_leaves_a_question_with_a_held_gate() {
    let e = env();
    let run = add_run(&e, "r-gate", "p-gate", |_| {});
    let q = ask(&e, &run, "toolu_q");
    let (tx, _rx) = oneshot::channel();
    e.server.agents.inner.lock().unwrap().gates.insert(
        q.clone(),
        Gate {
            tx,
            pane: "p-gate".into(),
        },
    );
    close_questions_for_tool(&e.server, &run, "toolu_b");
    assert_eq!(status(&e, &q), InteractionStatus::Open);
    // A new prompt leaves a held gate alone too (a sandbox process may still be waiting).
    on_signal(
        &e.server,
        "p-gate",
        Harness::Claude,
        "UserPromptSubmit",
        &json!({"session_id": "sess-r-gate", "prompt": "next"}),
    );
    assert_eq!(status(&e, &q), InteractionStatus::Open);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_request_cannot_be_dismissed() {
    let e = env();
    let run = add_run(&e, "r-hl", "p-hl", |r| {
        r.integration = "headless:acp".into()
    });
    let q = ask(&e, &run, "toolu_q");
    let refusal = dismiss_refusal(&e.server, &q).expect("refused");
    assert_eq!(refusal.data.kind, "unsupported");
    assert_eq!(status(&e, &q), InteractionStatus::Open);
    let plain = add_run(&e, "r-plain", "p-plain", |_| {});
    let q2 = ask(&e, &plain, "toolu_q2");
    assert!(dismiss_refusal(&e.server, &q2).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_working_screen_closes_old_interactions_only() {
    let e = env();
    let run = add_run(&e, "r-quiet", "p-quiet", |r| {
        r.execution = facet(Execution::Working, StateSource::Structured, 1.0);
    });
    let working = screen::ScreenMatch {
        state: Some((Execution::Working, 0.8)),
        dialog: None,
    };
    let old = open(
        &e,
        &run,
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_use_id": "toolu_old", "tool_input": {"command": "ls"}}),
        40_000,
    );
    let young = approval(&e, &run, "toolu_young");
    let held = open(
        &e,
        &run,
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_use_id": "toolu_held", "tool_input": {"command": "pwd"}}),
        40_000,
    );
    let (tx, _rx) = oneshot::channel();
    e.server.agents.inner.lock().unwrap().gates.insert(
        held.clone(),
        Gate {
            tx,
            pane: "p-quiet".into(),
        },
    );
    // A fresh quiet stretch closes nothing, however old the interaction.
    e.server
        .agents
        .close_unseen_dialogs(&e.server, &run, Harness::Claude, &working);
    assert_eq!(status(&e, &old), InteractionStatus::Open);
    // The stretch has lasted past the window (backdated rather than waited for).
    e.server
        .agents
        .inner
        .lock()
        .unwrap()
        .no_dialog_since
        .insert("p-quiet".into(), Instant::now() - Duration::from_secs(31));
    e.server
        .agents
        .close_unseen_dialogs(&e.server, &run, Harness::Claude, &working);
    assert_eq!(status(&e, &old), InteractionStatus::ResolvedElsewhere);
    assert_eq!(reasons(&e, &old), vec!["no dialog on screen".to_string()]);
    assert_eq!(
        status(&e, &young),
        InteractionStatus::Open,
        "opened 1 s ago"
    );
    assert_eq!(status(&e, &held), InteractionStatus::Open, "held gate");
    // A dialog on screen restarts the stretch.
    let dialog = screen::evaluate(
        Harness::Claude,
        "│ Bash command\n│   ls\n│ Do you want to proceed?\n│ ❯ 1. Yes\n│   2. No",
    );
    assert!(dialog.dialog.is_some());
    e.server
        .agents
        .close_unseen_dialogs(&e.server, &run, Harness::Claude, &dialog);
    assert!(
        !e.server
            .agents
            .inner
            .lock()
            .unwrap()
            .no_dialog_since
            .contains_key("p-quiet")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_before_typing_notifies_once() {
    let e = env();
    let run = add_run(&e, "r-fail", "p-fail", |_| {});
    let q = ask(&e, &run, "toolu_q");
    record_decision(
        &e.server,
        &q,
        Answer {
            decision: None,
            choices: vec![("q0".into(), vec!["Postgres".into()])],
            text: None,
        },
        "tui:tester",
        None,
        None,
        None,
    )
    .unwrap();
    set_delivery(&e.server, &q, DeliveryState::Delivering, None);
    set_delivery(
        &e.server,
        &q,
        DeliveryState::Failed,
        Some("dialog_changed: the visible dialog is not this interaction".into()),
    );
    let notes: Vec<Notification> = e.server.with_core(|c| {
        c.notifications
            .iter()
            .filter(|n| n.title == "Answer not sent")
            .cloned()
            .collect()
    });
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].pane.as_deref(), Some("p-fail"));
    assert!(
        notes[0].body.contains("dialog had changed"),
        "{}",
        notes[0].body
    );
    assert!(nothing_typed(Some("selection_mismatch")));
    assert!(!nothing_typed(Some("pane exited")));
    assert!(!nothing_typed(None));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_prompt_leaves_a_sandbox_request_whose_gate_is_not_yet_held() {
    let e = env();
    let run = add_run(&e, "r-sbx", "p-sbx", |_| {});
    let q = approval(&e, &run, "toolu_x");
    {
        let mut c = e.server.core.lock().unwrap();
        let mut it = c.interaction(&q).cloned().unwrap();
        it.native_ref = Some("egress:k:example.com:443".into());
        let mut tx = Tx::new();
        tx.interaction(it);
        e.server.commit(&mut c, tx).unwrap();
    }
    on_signal(
        &e.server,
        "p-sbx",
        Harness::Claude,
        "UserPromptSubmit",
        &json!({"session_id": "sess-r-sbx", "prompt": "next"}),
    );
    assert_eq!(status(&e, &q), InteractionStatus::Open);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gated_question_whose_gate_is_not_yet_registered_stays_open() {
    let e = env();
    let run = add_run(&e, "r-pend", "p-pend", |_| {});
    let q = ask(&e, &run, "toolu_q");
    {
        let mut c = e.server.core.lock().unwrap();
        let mut it = c.interaction(&q).cloned().unwrap();
        it.gate = true;
        let mut tx = Tx::new();
        tx.interaction(it);
        e.server.commit(&mut c, tx).unwrap();
    }
    close_questions_for_tool(&e.server, &run, "toolu_b");
    assert_eq!(status(&e, &q), InteractionStatus::Open);
    on_signal(
        &e.server,
        "p-pend",
        Harness::Claude,
        "UserPromptSubmit",
        &json!({"session_id": "sess-r-pend", "prompt": "next"}),
    );
    assert_eq!(status(&e, &q), InteractionStatus::Open);
}

fn open_of_kind(e: &Env, run: &str, kind: InteractionKind) -> Vec<Interaction> {
    e.server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| i.run == run && i.kind == kind && i.status == InteractionStatus::Open)
            .cloned()
            .collect()
    })
}

fn reported_again(e: &Env) -> usize {
    e.server.with_core(|c| {
        c.store
            .events_after(0, 10_000, &["interaction.updated".to_string()])
            .unwrap()
            .into_iter()
            .map(|ev| serde_json::to_value(ev).unwrap())
            .filter(|v| v["data"]["reason"] == "reported_again")
            .count()
    })
}

fn four_questions() -> Value {
    json!({"questions": [
        {"question": "Which resources should be shareable?", "header": "Scope", "multiSelect": false,
         "options": [{"label": "Playable materials"}, {"label": "Podcasts only"}]},
        {"question": "Who may publish a question?", "header": "Publisher", "multiSelect": false,
         "options": [{"label": "Creator and owner"}, {"label": "Creator only"}]},
        {"question": "What happens to existing questions?", "header": "Existing Qs", "multiSelect": false,
         "options": [{"label": "Keep them public"}, {"label": "Make them private"}]},
        {"question": "Who may ask on a public link?", "header": "Who asks", "multiSelect": false,
         "options": [{"label": "Any logged-in user"}, {"label": "Hub members only"}]}
    ]})
}

/// Live bug: Claude reports one `AskUserQuestion` call through `PreToolUse` (with a
/// `tool_use_id`) and then `PermissionRequest` (without one); the inbox showed it twice.
#[tokio::test(flavor = "multi_thread")]
async fn one_ask_user_question_call_opens_one_question() {
    let e = env();
    let run = add_run(&e, "r-dup", "p-dup", |_| {});
    let pre = json!({"session_id": "sess-r-dup", "tool_name": "AskUserQuestion", "tool_use_id": "toolu_dup", "tool_input": four_questions()});
    let perm = json!({"session_id": "sess-r-dup", "tool_name": "AskUserQuestion", "tool_input": four_questions()});
    let a = gate(&e.server, "p-dup", Harness::Claude, "PreToolUse", &pre)
        .await
        .unwrap();
    assert!(a["decision"].is_null(), "keystroke-answered question: {a}");
    let b = gate(
        &e.server,
        "p-dup",
        Harness::Claude,
        "PermissionRequest",
        &perm,
    )
    .await
    .unwrap();
    // Transparent: no decision, so Claude's own dialog shows as before.
    assert!(b["decision"].is_null(), "{b}");
    let open = open_of_kind(&e, &run.id, InteractionKind::Question);
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].native_ref.as_deref(), Some("toolu_dup"));
    assert_eq!(open[0].questions.len(), 4);
}

/// A question reported again without a matching native ref (a retried hook, a transport that
/// carries no id) reuses the open interaction instead of opening a second one.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_reported_again_without_its_id_reuses_the_open_one() {
    let e = env();
    let run = add_run(&e, "r-again", "p-again", |_| {});
    let with_id = json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_1", "tool_input": four_questions()});
    let without = json!({"tool_name": "AskUserQuestion", "tool_input": four_questions()});
    gate(
        &e.server,
        "p-again",
        Harness::Claude,
        "PreToolUse",
        &without,
    )
    .await
    .unwrap();
    gate(
        &e.server,
        "p-again",
        Harness::Claude,
        "PreToolUse",
        &with_id,
    )
    .await
    .unwrap();
    let open = open_of_kind(&e, &run.id, InteractionKind::Question);
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].native_ref.as_deref(), Some("toolu_1"));
    // Another call asking something else is its own question.
    let other = json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_2", "tool_input": {"questions": [
        {"question": "Ship it?", "options": [{"label": "Yes"}, {"label": "No"}], "multiSelect": false}]}});
    gate(&e.server, "p-again", Harness::Claude, "PreToolUse", &other)
        .await
        .unwrap();
    assert_eq!(
        open_of_kind(&e, &run.id, InteractionKind::Question).len(),
        2
    );
    // The same prompts under a different native ref are a different call too.
    let twin = json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_3", "tool_input": four_questions()});
    gate(&e.server, "p-again", Harness::Claude, "PreToolUse", &twin)
        .await
        .unwrap();
    assert_eq!(
        open_of_kind(&e, &run.id, InteractionKind::Question).len(),
        3
    );
}

/// `ExitPlanMode` arrives through `PermissionRequest`, which carries no `tool_use_id`: the same
/// plan reported twice is one plan review.
#[tokio::test(flavor = "multi_thread")]
async fn the_same_plan_reported_twice_is_one_plan_review() {
    let e = env();
    let run = add_run(&e, "r-plan", "p-plan", |_| {});
    let plan = json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "1. Add the share link\n2. Test it"}});
    // Plan reviews are gated (held until answered): run both hooks in the background.
    let mut held = Vec::new();
    for _ in 0..2 {
        let (srv, p) = (e.server.clone(), plan.clone());
        held.push(tokio::spawn(async move {
            gate(&srv, "p-plan", Harness::Claude, "PermissionRequest", &p).await
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while open_of_kind(&e, &run.id, InteractionKind::PlanReview).is_empty()
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // The second report is recorded as the first one reported again.
    let deadline = Instant::now() + Duration::from_secs(5);
    while reported_again(&e) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(reported_again(&e), 1);
    let open = open_of_kind(&e, &run.id, InteractionKind::PlanReview);
    assert_eq!(open.len(), 1, "{open:?}");
    // A revised plan is a new review.
    let revised = json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "1. Add the share link\n2. Test it\n3. Document it"}});
    let srv = e.server.clone();
    held.push(tokio::spawn(async move {
        gate(
            &srv,
            "p-plan",
            Harness::Claude,
            "PermissionRequest",
            &revised,
        )
        .await
    }));
    let deadline = Instant::now() + Duration::from_secs(5);
    while open_of_kind(&e, &run.id, InteractionKind::PlanReview).len() < 2
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        open_of_kind(&e, &run.id, InteractionKind::PlanReview).len(),
        2
    );
    for h in held {
        h.abort();
    }
}

/// A multi-question answer must name a choice for every question before it is recorded:
/// a partial answer would leave Claude's dialog half answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_multi_question_answer_is_refused() {
    let e = env();
    let run = add_run(&e, "r-part", "p-part", |_| {});
    let pre = json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_p", "tool_input": four_questions()});
    gate(&e.server, "p-part", Harness::Claude, "PreToolUse", &pre)
        .await
        .unwrap();
    let it = open_of_kind(&e, &run.id, InteractionKind::Question).remove(0);
    let first = it.questions[0].id.clone();
    let partial = Answer {
        decision: None,
        choices: vec![(first, vec!["Playable materials".into()])],
        text: None,
    };
    let why = incomplete_answer(&it, &partial).expect("refused");
    assert!(why.contains("1 of 4"), "{why}");
    let full = Answer {
        decision: None,
        choices: it
            .questions
            .iter()
            .map(|q| (q.id.clone(), vec![q.options[0].id.clone()]))
            .collect(),
        text: None,
    };
    assert_eq!(incomplete_answer(&it, &full), None);
}
