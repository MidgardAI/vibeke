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
    // Older than `SCREEN_GRACE`: the question had time to show.
    open(
        e,
        run,
        "PreToolUse",
        json!({"tool_name": "AskUserQuestion", "tool_use_id": tool_use, "tool_input": {"questions": [{"question": "Which database?", "options": [{"label": "Postgres"}, {"label": "SQLite"}], "multiSelect": false}]}}),
        5000,
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

fn closed_by(e: &Env, id: &str) -> Vec<String> {
    e.server.with_core(|c| {
        c.store
            .events_after(0, 10_000, &["interaction.cancelled".to_string()])
            .unwrap()
            .into_iter()
            .map(|ev| serde_json::to_value(ev).unwrap())
            .filter(|v| v["subject"]["interaction"] == id)
            .filter_map(|v| v["data"]["closed_by"].as_str().map(str::to_string))
            .collect()
    })
}

/// Close reasons of `id`: resolved elsewhere, or cancelled (dismissals and heuristics).
fn reasons(e: &Env, id: &str) -> Vec<String> {
    e.server.with_core(|c| {
        c.store
            .events_after(
                0,
                10_000,
                &[
                    "interaction.resolved_elsewhere".to_string(),
                    "interaction.cancelled".to_string(),
                ],
            )
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
    // A question opened a moment ago may not be drawn yet: a parallel tool leaves it.
    let fresh = open(
        &e,
        &run,
        "PreToolUse",
        json!({"tool_name": "AskUserQuestion", "tool_use_id": "toolu_fresh", "tool_input": {"questions": [{"question": "Which port?", "options": [{"label": "8080"}, {"label": "3000"}], "multiSelect": false}]}}),
        100,
    );
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
    // A guess, recorded as one: cancelled by a heuristic, not answered elsewhere.
    assert_eq!(status(&e, &q), InteractionStatus::Cancelled);
    assert_eq!(reasons(&e, &q), vec![OTHER_TOOL_STARTED.to_string()]);
    assert_eq!(closed_by(&e, &q), vec!["heuristic".to_string()]);
    assert_eq!(status(&e, &fresh), InteractionStatus::Open, "not drawn yet");
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
        r.harness_version = Some("2.1.5".into());
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
    assert_eq!(status(&e, &old), InteractionStatus::Cancelled);
    assert_eq!(reasons(&e, &old), vec![NO_DIALOG_SEEN.to_string()]);
    assert_eq!(closed_by(&e, &old), vec!["heuristic".to_string()]);
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

/// Run `gate` for a Claude permission request as if it came on a hook connection, and wait
/// until its gate is held.
async fn held_gate(
    e: &Env,
    pane: &str,
    tool_use: &str,
) -> (crate::run::ConnCancel, tokio::task::JoinHandle<R>, String) {
    let cancel = crate::run::ConnCancel::default();
    let srv = e.server.clone();
    let p = json!({"tool_name": "Bash", "tool_use_id": tool_use, "tool_input": {"command": "cargo test"}});
    let pane_s = pane.to_string();
    let task =
        tokio::spawn(cancel.scope(async move {
            gate(&srv, &pane_s, Harness::Claude, "PermissionRequest", &p).await
        }));
    let deadline = Instant::now() + Duration::from_secs(10);
    let id = loop {
        let held = e.server.with_core(|c| {
            c.model
                .interactions
                .iter()
                .find(|i| i.pane == pane && i.native_ref.as_deref() == Some(tool_use))
                .map(|i| i.id.clone())
        });
        if let Some(id) = held
            && e.server
                .agents
                .inner
                .lock()
                .unwrap()
                .gates
                .contains_key(&id)
        {
            break id;
        }
        assert!(Instant::now() < deadline, "gate was never held");
        tokio::task::yield_now().await;
    };
    (cancel, task, id)
}

fn interaction(e: &Env, id: &str) -> Interaction {
    e.server
        .with_core(|c| c.interaction(id).cloned())
        .expect("open interaction")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_hook_connection_releases_its_gate() {
    let e = env();
    let _run = add_run(&e, "r-hook", "p-hook", |_| {});
    let (cancel, task, id) = held_gate(&e, "p-hook", "toolu_g").await;
    assert!(interaction(&e, &id).gate);
    // The hook process was killed: its connection closes.
    cancel.cancel();
    let reply = task.await.unwrap().unwrap();
    assert!(reply["decision"].is_null());
    assert!(
        !e.server
            .agents
            .inner
            .lock()
            .unwrap()
            .gates
            .contains_key(&id),
        "gate released"
    );
    let it = interaction(&e, &id);
    assert!(!it.gate, "a later answer is typed, not sent to a dead hook");
    assert_eq!(it.status, InteractionStatus::Open);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_that_dies_before_its_ack_leaves_the_delivery_unknown() {
    let e = env();
    let _run = add_run(&e, "r-ack", "p-ack", |_| {});
    let (cancel, task, id) = held_gate(&e, "p-ack", "toolu_k").await;
    // What `interaction.answer` does for a held gate.
    let (native, key) = record_decision(
        &e.server,
        &id,
        Answer {
            decision: Some(Decision::Allow),
            choices: vec![],
            text: None,
        },
        "tui:tester",
        None,
        None,
        None,
    )
    .unwrap()
    .unwrap();
    let g = e
        .server
        .agents
        .inner
        .lock()
        .unwrap()
        .gates
        .remove(&id)
        .unwrap();
    set_delivery(&e.server, &id, DeliveryState::Delivering, None);
    assert!(g.tx.send(GateReply::Decision { json: native, key }).is_ok());
    let reply = task.await.unwrap().unwrap();
    assert!(reply["decision"].is_object());
    // No ack arrives; the connection closes.
    cancel.cancel();
    let deadline = Instant::now() + Duration::from_secs(10);
    while interaction(&e, &id).delivery != DeliveryState::DeliveryUnknown {
        assert!(Instant::now() < deadline, "delivery stayed Delivering");
        tokio::task::yield_now().await;
    }
    // The harness moving on later does not turn it into a confirmed delivery.
    resolve(
        &e.server,
        &id,
        InteractionStatus::ResolvedElsewhere,
        "tool ran",
    );
    assert_eq!(
        interaction_any(&e, &id).delivery,
        DeliveryState::DeliveryUnknown
    );
}

fn interaction_any(e: &Env, id: &str) -> Interaction {
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
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_an_answer_in_flight_never_reports_it_delivered() {
    let e = env();
    let run = add_run(&e, "r-fly", "p-fly", |_| {});
    let q = approval(&e, &run, "toolu_f");
    record_decision(
        &e.server,
        &q,
        Answer {
            decision: Some(Decision::Allow),
            choices: vec![],
            text: None,
        },
        "tui:tester",
        None,
        None,
        None,
    )
    .unwrap();
    set_delivery(&e.server, &q, DeliveryState::Delivering, None);
    // `interaction.cancel` / an inbox dismissal while the answer is being delivered.
    resolve(&e.server, &q, InteractionStatus::Cancelled, "dismissed");
    assert_eq!(
        interaction_any(&e, &q).delivery,
        DeliveryState::DeliveryUnknown
    );
    // The harness moving on is still not taken as proof for an unknown delivery.
    resolve(
        &e.server,
        &q,
        InteractionStatus::ResolvedElsewhere,
        "tool ran",
    );
    assert_eq!(
        interaction_any(&e, &q).delivery,
        DeliveryState::DeliveryUnknown
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dismissed_dialog_does_not_reopen_while_it_shows() {
    let e = env();
    let run = add_run(&e, "r-dis", "p-dis", |_| {});
    let a = open(
        &e,
        &run,
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_use_id": "toolu_d", "tool_input": {"command": "cargo test --all"}}),
        1000,
    );
    // Dismiss in Vibeke: the gate is released and the harness draws its own dialog.
    resolve(&e.server, &a, InteractionStatus::Cancelled, DISMISSED);
    let same = screen::evaluate(
        Harness::Claude,
        "│ Bash command\n│   cargo test --all\n│ Do you want to proceed?\n│ ❯ 1. Yes\n│   2. No",
    );
    assert!(same.dialog.is_some());
    let agents = &e.server.agents;
    assert!(agents.dialog_dismissed(&e.server, &run, &same));
    // Seen once, it stays dismissed by its fingerprint.
    assert!(agents.dialog_dismissed(&e.server, &run, &same));
    // Another dialog is a new request.
    let other = screen::evaluate(
        Harness::Claude,
        "│ Bash command\n│   rm -rf build\n│ Do you want to proceed?\n│ ❯ 1. Yes\n│   2. No",
    );
    assert!(other.dialog.is_some());
    assert!(!agents.dialog_dismissed(&e.server, &run, &other));
    // A new turn forgets the dismissal: the same dialog then is a new request.
    on_signal(
        &e.server,
        "p-dis",
        Harness::Claude,
        "UserPromptSubmit",
        &json!({"session_id": "sess-r-dis", "prompt": "run the tests again"}),
    );
    // Turn marks have millisecond resolution: make sure the mark moved.
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(
            K_TURN_MARK,
            "r-dis",
            None,
            &TurnMark {
                run: "r-dis".into(),
                at_ms: now_ms() + 1,
            },
        );
        e.server.commit(&mut c, tx).unwrap();
    }
    assert!(!agents.dialog_dismissed(&e.server, &run, &same));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dismissed_screen_dialog_is_forgotten_once_the_agent_works_again() {
    let e = env();
    let run = add_run(&e, "r-scr", "p-scr", |_| {});
    let shown = screen::evaluate(
        Harness::Claude,
        "│ Bash command\n│   make deploy\n│ Do you want to proceed?\n│ ❯ 1. Yes\n│   2. No",
    );
    let d = shown.dialog.clone().unwrap();
    let id = open(
        &e,
        &run,
        "PermissionRequest",
        json!({"tool_name": "Bash", "tool_use_id": "toolu_s", "tool_input": {"command": "make deploy"}}),
        1000,
    );
    {
        let mut c = e.server.core.lock().unwrap();
        let mut it = c.interaction(&id).cloned().unwrap();
        it.source = StateSource::Screen;
        it.native_ref = Some(format!("screen:{}", d.fingerprint));
        let mut tx = Tx::new();
        tx.interaction(it);
        e.server.commit(&mut c, tx).unwrap();
    }
    resolve(&e.server, &id, InteractionStatus::Cancelled, DISMISSED);
    let agents = &e.server.agents;
    assert!(agents.dialog_dismissed(&e.server, &run, &shown));
    let working = screen::ScreenMatch {
        state: Some((Execution::Working, 0.8)),
        dialog: None,
    };
    assert!(!agents.dialog_dismissed(&e.server, &run, &working));
    assert!(!agents.dialog_dismissed(&e.server, &run, &shown));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_screen_closes_nothing_for_an_unvalidated_harness_version() {
    let e = env();
    for (n, version) in [(1, None), (2, Some("9.0.0"))] {
        let (rid, pid) = (format!("r-unval{n}"), format!("p-unval{n}"));
        let run = add_run(&e, &rid, &pid, |r| {
            r.execution = facet(Execution::Working, StateSource::Structured, 1.0);
            r.harness_version = version.map(str::to_string);
        });
        let old = open(
            &e,
            &run,
            "PermissionRequest",
            json!({"tool_name": "Bash", "tool_use_id": "toolu_old", "tool_input": {"command": "ls"}}),
            40_000,
        );
        let working = screen::ScreenMatch {
            state: Some((Execution::Working, 0.8)),
            dialog: None,
        };
        e.server
            .agents
            .inner
            .lock()
            .unwrap()
            .no_dialog_since
            .insert(pid.clone(), Instant::now() - Duration::from_secs(31));
        e.server
            .agents
            .close_unseen_dialogs(&e.server, &run, Harness::Claude, &working);
        assert_eq!(status(&e, &old), InteractionStatus::Open, "{version:?}");
    }
}
