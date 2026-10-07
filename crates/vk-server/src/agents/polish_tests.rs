//! In-process tests for the adapter polish (04 §2.5, §7.7, §10, §12.3, §13) that need a
//! `Server` but no panes or processes: the transcript tailer, structured loss, drift telemetry,
//! pre-tool enforcement, `policy.suggest` over stored approvals and the manifest channel's
//! announcements. Nothing here launches a harness: runs are created directly (so `bound_run`
//! never spawns a version probe).

use super::*;
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use std::io::Write;
use std::sync::Once;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Same values as the other in-process test modules: one consistent root per binary, and
        // a config that does not exist (defaults), never the developer's own.
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
    dir: tempfile::TempDir,
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
    };
    Env {
        server: Server::new(paths, opts).unwrap(),
        dir,
    }
}

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

fn add_run(e: &Env, id: &str, pane: &str, f: impl FnOnce(&mut AgentRun)) -> AgentRun {
    let mut r = harness_tests_run();
    r.id = id.into();
    r.handle = id.into();
    r.pane = pane.into();
    f(&mut r);
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.run(r.clone());
    e.server.commit(&mut c, tx).unwrap();
    r
}

fn run_of(e: &Env, id: &str) -> AgentRun {
    e.server.with_core(|c| c.run(id).cloned()).unwrap()
}

fn claude_lines(model: &str, turn: u32) -> String {
    let u = |i: u64, o: u64| json!({"input_tokens": i, "output_tokens": o, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5});
    let lines = [
        json!({"type": "user", "uuid": format!("u{turn}"), "message": {"role": "user", "content": "go"}}),
        json!({"type": "assistant", "message": {"id": format!("a{turn}"), "model": model, "stop_reason": "tool_use", "usage": u(10, 20), "content": [{"type": "tool_use", "id": format!("tool{turn}"), "name": "Bash", "input": {"command": "cargo test"}}]}}),
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("tool{turn}"), "content": "ok"}]}}),
        json!({"type": "assistant", "message": {"id": format!("b{turn}"), "model": model, "stop_reason": "end_turn", "usage": u(30, 40), "content": [{"type": "text", "text": "done"}]}}),
    ];
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn open_approval(e: &Env, run: &AgentRun, tool_use: &str) -> String {
    let mut it = harness::interaction_from_hook(
        Harness::Claude,
        "PermissionRequest",
        &json!({"tool_name": "Bash", "tool_input": {"command": "cargo test"}, "tool_use_id": tool_use}),
    )
    .unwrap();
    it.run = run.id.clone();
    it.pane = run.pane.clone();
    let id = it.id.clone();
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.interaction(it);
    e.server.commit(&mut c, tx).unwrap();
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn tailer_writes_turn_records_items_totals_and_reconciles_approvals() {
    let e = env();
    let path = e.dir.path().join("session.jsonl");
    std::fs::write(&path, claude_lines("claude-sonnet-4-20250514", 1)).unwrap();
    let run = add_run(&e, "run-tail", "pane-tail", |r| {
        r.transcript_path = Some(path.display().to_string());
    });
    let approval = open_approval(&e, &run, "tool1");
    assert!(tailer::track(&run), "claude transcript is tailed");
    assert!(tailer::poll_all(&e.server) > 0);

    // One compact record per finished turn: usage summed over its messages, priced from the
    // bundled table (no harness-reported cost, no subscription billing configured).
    let recs = tailer::records_of(&e.server, "run-tail");
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!((r.n, r.native_id.as_str()), (1, "b1"));
    assert_eq!((r.input, r.output), (40, 60));
    assert_eq!((r.cache_read, r.cache_write), (200, 10));
    assert_eq!(r.cost_source, "price_table");
    assert!(r.cost_usd.unwrap() > 0.0);
    assert_eq!(r.model.as_deref(), Some("claude-sonnet-4-20250514"));

    // Run totals follow the same numbers.
    let u = run_of(&e, "run-tail").usage;
    assert_eq!((u.input_tokens, u.output_tokens), (40, 60));
    assert_eq!(u.source, "transcript");
    assert!(u.cost_usd.is_some());

    // The tool call became an item and closed with its result; the approval that was waiting
    // on that tool call resolved (the tool ran, so someone answered).
    let item = e.server.with_core(|c| {
        c.store
            .find::<vk_review::checks::ToolRecord>("tool_item", "run-tail:tool1")
            .unwrap()
    });
    let item = item.expect("tool item");
    assert_eq!(item.command.as_deref(), Some("cargo test"));
    assert!(item.ended_at_ms.is_some());
    // A closed interaction leaves the live model; its record stays in the store.
    assert!(
        e.server
            .with_core(|c| c.interaction(&approval).cloned())
            .is_none()
    );
    let it = e
        .server
        .with_core(|c| {
            c.store
                .find::<Interaction>("interaction", &approval)
                .unwrap()
        })
        .expect("closed interaction record");
    assert_eq!(it.status, InteractionStatus::ResolvedElsewhere);

    // Nothing new: a second pass is a no-op, never a duplicate record.
    assert_eq!(tailer::poll_all(&e.server), 0);
    assert_eq!(tailer::records_of(&e.server, "run-tail").len(), 1);

    // A second turn appended to the file extends the same tail from its offset.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(claude_lines("claude-sonnet-4-20250514", 2).as_bytes())
        .unwrap();
    assert!(tailer::poll_all(&e.server) > 0);
    let recs = tailer::records_of(&e.server, "run-tail");
    assert_eq!(recs.iter().map(|r| r.n).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(run_of(&e, "run-tail").usage.output_tokens, 120);

    // Reconcile reads what the transcript says the run is doing now.
    let run = run_of(&e, "run-tail");
    assert_eq!(
        tailer::reconcile_state(&e.server, &run),
        Some(Execution::Idle)
    );
    assert!(
        tailer::tailing()
            .iter()
            .any(|(r, _, off)| r == "run-tail" && *off > 0)
    );
    tailer::untrack("run-tail");
    assert!(!tailer::tailing().iter().any(|(r, _, _)| r == "run-tail"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_known_turn_id_is_never_recorded_twice() {
    let e = env();
    let path = e.dir.path().join("s2.jsonl");
    std::fs::write(&path, claude_lines("claude-opus-4", 1)).unwrap();
    let run = add_run(&e, "run-dedupe", "pane-dedupe", |r| {
        r.transcript_path = Some(path.display().to_string());
    });
    assert!(tailer::track(&run));
    tailer::poll_all(&e.server);
    // A fresh tail over the same file (a restarted server, a replaced path) re-reads it all.
    tailer::untrack("run-dedupe");
    assert!(tailer::track(&run_of(&e, "run-dedupe")));
    tailer::poll_all(&e.server);
    assert_eq!(tailer::records_of(&e.server, "run-dedupe").len(), 1);
    tailer::untrack("run-dedupe");
}

#[tokio::test(flavor = "multi_thread")]
async fn subscription_billing_keeps_tokens_and_no_dollars() {
    let e = env();
    // A claude-family manifest under its own id, so the billing override cannot leak into the
    // other tests' `claude` runs.
    manifests::test_register(
        "id = \"subbot\"\nextends = \"claude\"\n[detect]\npriority = 100\n[[detect.process]]\nexe_basename = [\"subbot\"]\n",
    );
    usage::BILLING_OVERRIDE
        .lock()
        .unwrap()
        .insert("subbot".into(), "subscription".into());
    let path = e.dir.path().join("sub.jsonl");
    std::fs::write(&path, claude_lines("claude-sonnet-4", 1)).unwrap();
    let run = add_run(&e, "run-sub", "pane-sub", |r| {
        r.harness = "subbot".into();
        r.transcript_path = Some(path.display().to_string());
    });
    assert!(tailer::track(&run));
    tailer::poll_all(&e.server);
    let recs = tailer::records_of(&e.server, "run-sub");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].cost_source, "subscription");
    assert!(recs[0].cost_usd.is_none());
    assert_eq!(recs[0].output, 60);
    let u = run_of(&e, "run-sub").usage;
    assert_eq!(u.output_tokens, 60);
    assert!(u.cost_usd.is_none());
    tailer::untrack("run-sub");
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_loss_marks_the_adapter_disconnected_and_reconciles_from_the_transcript() {
    let e = env();
    let path = e.dir.path().join("lost.jsonl");
    std::fs::write(&path, claude_lines("claude-sonnet-4", 1)).unwrap();
    let run = add_run(&e, "run-lost", "pane-lost", |r| {
        r.transcript_path = Some(path.display().to_string());
        r.execution = facet(Execution::Working, StateSource::Structured, 1.0);
    });
    arbiter::structured_lost(&e.server, &run.id, "test");
    let r = run_of(&e, "run-lost");
    assert_eq!(r.health, AdapterHealth::Disconnected);
    // The transcript says the last turn ended: idle, marked inferred, below structured confidence.
    assert_eq!(r.execution.value, Execution::Idle);
    assert!(
        arbiter::is_inferred(r.execution.detail.as_deref()),
        "{:?}",
        r.execution
    );
    assert!(r.execution.confidence < 1.0);
    // Idempotent.
    arbiter::structured_lost(&e.server, &run.id, "again");
    assert_eq!(run_of(&e, "run-lost").health, AdapterHealth::Disconnected);
    // A structured signal heals the health and clears the marker.
    let healed = bound_run(&e.server, "pane-lost", Harness::Claude);
    assert_eq!(healed.health, AdapterHealth::Healthy);
    assert!(!arbiter::is_inferred(
        run_of(&e, "run-lost").execution.detail.as_deref()
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_lost_signal_reaches_the_arbiter() {
    let e = env();
    add_run(&e, "run-tl", "pane-tl", |_| {});
    assert!(arbiter::handle_signal(
        &e.server,
        "pane-tl",
        "TransportLost",
        &json!({"reason": "socket closed"})
    ));
    assert_eq!(run_of(&e, "run-tl").health, AdapterHealth::Disconnected);
    assert!(!arbiter::handle_signal(
        &e.server,
        "pane-tl",
        "Stop",
        &json!({})
    ));
}

#[test]
fn drift_spike_raises_once_per_version() {
    let e = env();
    let run = add_run(&e, "run-drift", "pane-drift", |r| {
        r.harness = "driftbot".into();
        r.harness_version = Some("2.2.0".into());
    });
    for _ in 0..30 {
        arbiter::note(&e.server, &run, arbiter::Kind::Observation);
    }
    let row = |v: &str| {
        arbiter::snapshot()
            .into_iter()
            .find(|r| r["harness"] == "driftbot" && r["version"] == v)
    };
    assert_eq!(row("2.2.0").unwrap()["drifting"], false);
    for _ in 0..10 {
        arbiter::note(&e.server, &run, arbiter::Kind::Disagreement);
    }
    let r = row("2.2.0").unwrap();
    assert_eq!(r["drifting"], true);
    assert_eq!(r["observations"], 30);
    assert_eq!(r["disagreements"], 10);
    // Counters survive a restart through the store.
    let persisted = e
        .server
        .with_core(|c| c.store.kv_get("drift", "driftbot@2.2.0").unwrap())
        .unwrap();
    let counts: arbiter::Counts = serde_json::from_str(&persisted).unwrap();
    assert!(counts.notified);
    // The disagreement helper emits its event once per episode, not once per screen pass.
    let before = arbiter::snapshot()
        .into_iter()
        .find(|r| r["harness"] == "driftbot")
        .unwrap()["disagreements"]
        .as_u64()
        .unwrap();
    for _ in 0..5 {
        arbiter::disagreement(&e.server, &run, "execution", "working", "idle", "screen");
    }
    let after = arbiter::snapshot()
        .into_iter()
        .find(|r| r["harness"] == "driftbot")
        .unwrap()["disagreements"]
        .as_u64()
        .unwrap();
    assert_eq!(after, before + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_tool_enforcement_denies_by_policy_on_yolo_runs_only() {
    let e = env();
    add_run(&e, "run-enf", "pane-enf", |r| r.yolo = true);
    let added = crate::policy_api::api(
        &e.server,
        &user(),
        "policy.add",
        &json!({"tool": "Bash", "command_regex": "rm -rf", "effect": "deny"}),
    )
    .expect("policy.add handled")
    .unwrap();
    let rule = added["rule"]["id"].as_str().unwrap().to_string();
    let payload = |cmd: &str, mode: &str| json!({"tool_name": "Bash", "tool_input": {"command": cmd}, "permission_mode": mode, "tool_use_id": "t"});
    // A matching deny rule: the hook answers `deny` with the rule id.
    let r = enforce::pre_tool(
        &e.server,
        "pane-enf",
        Harness::Claude,
        &payload("rm -rf /tmp/x", "bypassPermissions"),
    )
    .await
    .unwrap();
    let o = &r["decision"]["hookSpecificOutput"];
    assert_eq!(o["hookEventName"], "PreToolUse");
    assert_eq!(o["permissionDecision"], "deny");
    assert!(
        o["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains(&rule)
    );
    // No matching rule: the yolo run is never slowed or prompted.
    let r = enforce::pre_tool(
        &e.server,
        "pane-enf",
        Harness::Claude,
        &payload("ls", "bypassPermissions"),
    )
    .await
    .unwrap();
    assert!(r["decision"].is_null());
    // The same command on a run with its own permission system is the harness's business.
    add_run(&e, "run-enf2", "pane-enf2", |r| r.yolo = false);
    let r = enforce::pre_tool(
        &e.server,
        "pane-enf2",
        Harness::Claude,
        &payload("rm -rf /tmp/x", "default"),
    )
    .await
    .unwrap();
    assert!(r["decision"].is_null());
    // An ask rule hands the decision back to the harness's own prompt.
    crate::policy_api::api(
        &e.server,
        &user(),
        "policy.add",
        &json!({"tool": "Bash", "command_regex": "^terraform apply", "effect": "ask"}),
    )
    .unwrap()
    .unwrap();
    let r = enforce::pre_tool(
        &e.server,
        "pane-enf",
        Harness::Claude,
        &payload("terraform apply", "bypassPermissions"),
    )
    .await
    .unwrap();
    assert_eq!(
        r["decision"]["hookSpecificOutput"]["permissionDecision"],
        "ask"
    );
    // The gate entry point routes pre-tool hooks here; AskUserQuestion keeps the question path.
    let g = gate(
        &e.server,
        "pane-enf",
        Harness::Claude,
        "PreToolUse",
        &payload("rm -rf /", "bypassPermissions"),
    )
    .await
    .unwrap();
    assert_eq!(
        g["decision"]["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
}

fn answered(e: &Env, run: &str, cmd: &str, decision: Decision, by: &str, i: usize) {
    let mut it = harness::interaction_from_hook(
        Harness::Claude,
        "PermissionRequest",
        &json!({"tool_name": "Bash", "tool_input": {"command": cmd}, "tool_use_id": format!("sg{i}")}),
    )
    .unwrap();
    it.run = run.into();
    it.pane = "pane-sg".into();
    it.status = InteractionStatus::Answered;
    it.answer = Some(Answer {
        decision: Some(decision),
        ..Default::default()
    });
    it.answered_by = Some(by.into());
    it.answered_at_ms = Some(1000 + i as i64);
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.interaction(it);
    e.server.commit(&mut c, tx).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_suggest_lists_repeatedly_approved_fingerprints_as_rules() {
    let e = env();
    let ws = e.dir.path().join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    add_run(&e, "run-sg", "pane-sg", |r| {
        r.cwd = Some(ws.display().to_string());
    });
    for (i, cmd) in [
        "pnpm test",
        "pnpm test --filter web",
        "pnpm -r test",
        "pnpm test -u",
    ]
    .iter()
    .enumerate()
    {
        answered(&e, "run-sg", cmd, Decision::Allow, "tui:c1", i);
    }
    // Decisions a rule already made are not evidence; denials block a group.
    answered(&e, "run-sg", "make build", Decision::Allow, "policy", 10);
    answered(&e, "run-sg", "make build", Decision::Allow, "policy", 11);
    answered(&e, "run-sg", "make build", Decision::Allow, "policy", 12);
    for i in 20..23 {
        answered(
            &e,
            "run-sg",
            "curl example.com",
            Decision::Allow,
            "tui:c1",
            i,
        );
    }
    answered(
        &e,
        "run-sg",
        "curl example.com",
        Decision::Deny,
        "tui:c1",
        23,
    );
    let r = polish::api(
        &e.server,
        &user(),
        "policy.suggest",
        &json!({"min_count": 3}),
    )
    .await
    .expect("handled")
    .unwrap();
    let list = r["suggestions"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{r}");
    let s = &list[0];
    assert_eq!(s["subject"], "pnpm test");
    assert_eq!(s["approvals"], 4);
    assert_eq!(s["rule"]["command_regex"], "^pnpm test( |$)");
    assert!(s["toml"].as_str().unwrap().contains("[[policy.rule]]"));
    assert_eq!(s["covered"], false);
    // Adding the rule covers it: the suggestion goes away unless asked for.
    crate::policy_api::api(
        &e.server,
        &user(),
        "policy.add",
        &json!({"tool": "Bash", "command_regex": "^pnpm test( |$)", "effect": "allow"}),
    )
    .unwrap()
    .unwrap();
    let r = polish::api(
        &e.server,
        &user(),
        "policy.suggest",
        &json!({"min_count": 3}),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(r["suggestions"].as_array().unwrap().is_empty(), "{r}");
    let r = polish::api(
        &e.server,
        &user(),
        "policy.suggest",
        &json!({"min_count": 3, "include_covered": true}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r["suggestions"][0]["covered"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_usage_and_limits_api() {
    let e = env();
    let path = e.dir.path().join("api.jsonl");
    std::fs::write(&path, claude_lines("claude-sonnet-4", 1)).unwrap();
    let run = add_run(&e, "run-api", "pane-api", |r| {
        r.transcript_path = Some(path.display().to_string());
        r.rate_limit = Some(RateLimitInfo {
            limited: false,
            resets_at_ms: Some(1_800_000_000_000),
            scope: Some("primary".into()),
            used_percent: Some(81.0),
            message: None,
            observed_at_ms: 5,
        });
    });
    assert!(tailer::track(&run));
    tailer::poll_all(&e.server);
    let r = polish::api(
        &e.server,
        &user(),
        "agent.turn_usage",
        &json!({"run": "run-api"}),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r["turn_count"], 1);
    assert_eq!(r["totals"]["output"], 60);
    assert!(r["totals"]["cost_usd"].as_f64().unwrap() > 0.0);
    let l = polish::api(&e.server, &user(), "agent.limits", &json!({}))
        .await
        .unwrap()
        .unwrap();
    let mine = l["limits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["used_percent"] == 81.0)
        .expect("claude limit listed");
    assert_eq!(mine["harness"], "claude");
    assert_eq!(mine["resets_at_ms"], 1_800_000_000_000i64);
    tailer::untrack("run-api");
}

#[test]
fn manifest_loaded_is_announced_once_per_serial() {
    let e = env();
    let root = e.dir.path().join("manifests");
    std::fs::create_dir_all(&root).unwrap();
    let st = channel::State {
        serial: 5,
        created_at: "2026-10-06".into(),
        verified: "signature".into(),
        applied: vec!["gemini@2".into(), "opencode@7".into()],
        url: "file:///idx".into(),
        sources: Default::default(),
    };
    std::fs::write(
        root.join("remote-state.json"),
        serde_json::to_vec(&st).unwrap(),
    )
    .unwrap();
    assert_eq!(channel::announce_loaded_from(&e.server, &root), 2);
    assert_eq!(
        channel::announce_loaded_from(&e.server, &root),
        0,
        "same serial: once"
    );
    let kv = e
        .server
        .with_core(|c| c.store.kv_get("manifests", "announced_serial").unwrap());
    assert_eq!(kv.as_deref(), Some("5"));
    // A newer serial announces again.
    let st = channel::State {
        serial: 6,
        applied: vec!["gemini@3".into()],
        ..st
    };
    std::fs::write(
        root.join("remote-state.json"),
        serde_json::to_vec(&st).unwrap(),
    )
    .unwrap();
    assert_eq!(channel::announce_loaded_from(&e.server, &root), 1);
}

#[test]
fn snapshot_carries_styles_title_alt_screen_and_cursor_to_the_manifest_dsl() {
    let mut eng = vk_term::Engine::new(60, 6, 100);
    let mut fx = vec![];
    eng.feed(
        b"\x1b]0;styled title\x07plain \x1b[1;38;2;217;119;87mRUN\x1b[0m done\r\n> ",
        &mut fx,
    );
    let snap = screen::snapshot_of(&eng);
    assert_eq!(snap.title, "styled title");
    assert!(!snap.alt_screen);
    assert_eq!(snap.cursor.map(|c| c.0), Some(1));
    let row0 = &snap.styles[0];
    assert!(!row0[0].bold);
    assert!(row0[6].bold, "R of RUN");
    assert_eq!(row0[6].fg, vk_agents::manifest::Col::Rgb(217, 119, 87));
    assert!(!row0[10].bold, "after the reset");
    let h = manifests::test_register(
        "id = \"styledbot\"\n[[detect.process]]\nexe_basename = [\"styledbot\"]\n[[screen.rules]]\nid = \"chip\"\nstate = \"working\"\nany = ['RUN']\nstyle = { fg = \"#d97757\", bold = true }\nconfidence = 0.9\n[[screen.rules]]\nid = \"titled\"\nstate = \"error\"\nwindow_title_regex = '^styled'\nconfidence = 0.5\n",
    );
    let (m, pending) = screen::evaluate_snapshot(h, &snap, 0, None);
    assert_eq!(m.state.map(|s| s.0), Some(Execution::Working));
    assert!(pending.is_none());
    // Alternate screen is visible to `region = "alt_screen_only"` rules.
    eng.feed(b"\x1b[?1049h", &mut fx);
    assert!(screen::snapshot_of(&eng).alt_screen);
}

#[test]
fn unknown_dialogs_are_provisional_and_never_answerable_by_keys() {
    // A picker Vibeke has no rule for, in a Claude pane.
    let screen_text = "╭──────────────────────╮\n│ Select model         │\n│ ❯ 1. Sonnet          │\n│   2. Opus            │\n╰──────────────────────╯";
    let m = screen::evaluate(Harness::Claude, screen_text);
    let d = m.dialog.expect("provisional dialog");
    assert!(d.is_unknown());
    assert_eq!(d.kind, InteractionKind::Question);
    assert_eq!(d.confidence, 0.5);
    assert_eq!(d.title, "Select model");
    // A real Claude approval is still the approval.
    let real = "╭──────────────────────────────────────────────╮\n│ Bash command                                 │\n│   rm -rf build                               │\n│ Do you want to proceed?                      │\n│ ❯ 1. Yes                                     │\n│   2. Yes, and don't ask again for rm commands│\n│   3. No, and tell Claude what to do differently (esc) │\n╰──────────────────────────────────────────────╯";
    let d = screen::evaluate(Harness::Claude, real).dialog.unwrap();
    assert!(!d.is_unknown());
    assert_eq!(d.kind, InteractionKind::Approval);
}
