//! The Turn/Item stream (02 §1.1; 3D): hook events become turns and items, payloads go to the
//! unified blob store, usage lands per turn, and the stream is pruned and readable.

use super::*;
use crate::api::dispatch;
use crate::core::Tx;
use crate::hardening::testkit::{pane_ctx, sample_pane, sample_run, server, user};
use std::sync::Arc;

const AWS: &str = "AKIAABCD1234EFGH5678";

fn setup(session: &str) -> (tempfile::TempDir, Arc<Server>, AgentRun) {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), session);
    let run = sample_run("run1", "pa");
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(sample_pane("pa", "wa"));
        tx.run(run.clone());
        s.commit(&mut c, tx).unwrap();
    }
    (dir, s, run)
}

fn turns(s: &Server) -> Vec<Turn> {
    let mut t = s.with_core(|c| turns_of(c, "run1"));
    t.sort_by_key(|t| t.seq);
    t
}

fn items_of(s: &Server, turn: &str) -> Vec<Item> {
    let mut v: Vec<Item> = s.with_core(|c| {
        c.store
            .load_by_field(K_ITEM, "$.turn_id", turn)
            .unwrap_or_default()
    });
    v.sort_by_key(|i| i.seq);
    v
}

fn events(s: &Server, kind: &str) -> Vec<vk_store::Event> {
    s.with_core(|c| {
        c.store
            .events_after(0, 10_000, &[kind.to_string()])
            .unwrap()
    })
}

#[test]
fn a_whole_turn_is_recorded_as_turn_and_items() {
    let (_d, s, run) = setup("whole");
    observe_hook(
        &s,
        &run,
        "UserPromptSubmit",
        &json!({"prompt": "fix the   login\nredirect"}),
    );
    observe_hook(
        &s,
        &run,
        "PreToolUse",
        &json!({"tool_name": "Bash", "tool_use_id": "t1", "tool_input": {"command": "cargo test\nsecond line"}}),
    );
    observe_hook(
        &s,
        &run,
        "PostToolUse",
        &json!({"tool_name": "Bash", "tool_use_id": "t1", "tool_input": {"command": "cargo test"}, "tool_response": "test result: ok. 4 passed"}),
    );
    observe_hook(
        &s,
        &run,
        "PreToolUse",
        &json!({"tool_name": "Edit", "tool_use_id": "t2", "tool_input": {"file_path": "/w/a.rs"}}),
    );
    observe_hook(
        &s,
        &run,
        "PostToolUse",
        &json!({"tool_name": "Edit", "tool_use_id": "t2", "tool_input": {"file_path": "/w/a.rs", "old_string": "a\nb", "new_string": "a\nb\nc\nd"}, "tool_response": {"ok": true}}),
    );
    observe_hook(
        &s,
        &run,
        "Stop",
        &json!({"last_assistant_message": "Done: redirects after login."}),
    );

    let ts = turns(&s);
    assert_eq!(ts.len(), 1);
    let t = &ts[0];
    assert_eq!(t.seq, 1);
    assert_eq!(t.status, TurnStatus::Completed);
    assert!(t.ended_at_ms.is_some());
    assert_eq!(
        t.input_summary, "fix the login redirect",
        "whitespace collapsed"
    );
    let items = items_of(&s, &t.id);
    let kinds: Vec<ItemKind> = items.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [
            ItemKind::UserMessage,
            ItemKind::Command,
            ItemKind::ToolResult,
            ItemKind::ToolCall,
            ItemKind::ToolResult,
            ItemKind::FileChange,
            ItemKind::AssistantMessage,
        ]
    );
    assert_eq!(
        items.iter().map(|i| i.seq).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6, 7],
        "gapless per-turn sequence"
    );
    assert_eq!(t.item_count, 7);
    // Calls were closed by their results.
    assert!(items[1].ended_at_ms.is_some() && items[3].ended_at_ms.is_some());
    assert_eq!(items[1].summary, "Bash: cargo test");
    assert_eq!(items[1].native_id.as_deref(), Some("t1"));
    assert!(items[2].summary.contains("test result: ok"));
    let fc = items[5].file_change.as_ref().unwrap();
    assert_eq!(fc.path, "/w/a.rs");
    assert_eq!(fc.op, FileOp::Modify);
    assert_eq!((fc.lines_added, fc.lines_removed), (Some(4), Some(2)));
    // One agent.item event per item, summary only.
    let ev = events(&s, "agent.item");
    assert_eq!(ev.len(), 7);
    assert_eq!(ev[1].data["kind"], "command");
    assert_eq!(ev[1].data["turn"], t.id);
    assert_eq!(ev[0].subject["run"], "run1");
    assert_eq!(ev[0].subject["pane"], "pa");
    assert_eq!(ev[0].tier, "sync");
}

#[test]
fn write_items_are_creates_with_line_counts() {
    let fc = file_change_of(
        "Write",
        &json!({"file_path": "/w/n.txt", "content": "1\n2\n3"}),
    )
    .unwrap();
    assert_eq!(fc.op, FileOp::Create);
    assert_eq!(fc.lines_added, Some(3));
    assert!(file_change_of("Bash", &json!({"file_path": "/x"})).is_none());
    assert!(file_change_of("Edit", &json!({})).is_none());
}

#[test]
fn secrets_never_reach_summaries_or_payloads() {
    let (_d, s, run) = setup("redact");
    let prompt = format!("use key {AWS} please");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": prompt}));
    let t = &turns(&s)[0];
    assert!(!t.input_summary.contains(AWS), "{}", t.input_summary);
    let big = format!("{AWS} {}", "log line\n".repeat(400));
    observe_hook(
        &s,
        &run,
        "PostToolUse",
        &json!({"tool_name": "Bash", "tool_use_id": "t9", "tool_input": {"command": "cat log"}, "tool_response": big}),
    );
    let items = items_of(&s, &t.id);
    let result = items
        .iter()
        .find(|i| i.kind == ItemKind::ToolResult)
        .unwrap();
    assert!(!result.summary.contains(AWS));
    let hash = result
        .payload_ref
        .clone()
        .expect("large output is a payload");
    let stored = crate::blob_store::store(&s).find(&hash).unwrap();
    assert_eq!(stored.source(), "payload");
    let path = crate::blob_store::store(&s).path_of(&hash, "txt");
    let text = std::fs::read_to_string(path).unwrap();
    assert!(
        !text.contains(AWS),
        "payload is redacted before it is stored"
    );
    assert!(text.contains("log line"));
    // The event names the blob, never its content.
    let ev = events(&s, "agent.item");
    assert!(ev.iter().all(|e| !e.data.to_string().contains(AWS)));
    assert!(ev.iter().any(|e| e.data["payload_ref"] == hash.as_str()));
}

#[test]
fn small_payloads_stay_in_the_summary() {
    assert!(text_payload("short").is_none());
    assert!(text_payload(&"x".repeat(INLINE_MAX)).is_none());
    assert!(text_payload(&"x".repeat(INLINE_MAX + 1)).is_some());
    let big = "é".repeat(PAYLOAD_MAX);
    let p = text_payload(&big).unwrap();
    assert!(p.data.len() <= PAYLOAD_MAX);
    assert!(
        std::str::from_utf8(&p.data).is_ok(),
        "cut on a character boundary"
    );
    assert_eq!(summarize(&"a ".repeat(400), 10).chars().count(), 10);
}

#[test]
fn a_new_prompt_interrupts_a_turn_that_never_stopped() {
    let (_d, s, run) = setup("interrupt");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "one"}));
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "two"}));
    let ts = turns(&s);
    assert_eq!(ts.len(), 2);
    assert_eq!(ts[0].status, TurnStatus::Interrupted);
    assert_eq!(ts[1].status, TurnStatus::Running);
    assert_eq!(ts[1].seq, 2);
    // Items land in the open turn.
    observe_hook(&s, &run, "Stop", &json!({}));
    observe_hook(&s, &run, "Interrupt", &json!({}));
    assert_eq!(turns(&s)[1].status, TurnStatus::Completed);
}

#[test]
fn interrupt_and_failure_set_the_turn_status() {
    let (_d, s, run) = setup("status");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "one"}));
    observe_hook(&s, &run, "Interrupt", &json!({}));
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "two"}));
    observe_hook(
        &s,
        &run,
        "StopFailure",
        &json!({"error_type": "rate_limit", "message": "slow down"}),
    );
    let ts = turns(&s);
    assert_eq!(ts[0].status, TurnStatus::Interrupted);
    assert_eq!(ts[1].status, TurnStatus::Failed);
    let err = items_of(&s, &ts[1].id)
        .into_iter()
        .find(|i| i.kind == ItemKind::Error)
        .unwrap();
    assert_eq!(err.summary, "rate_limit: slow down");
}

#[test]
fn a_failed_tool_is_an_error_item_without_a_file_change() {
    let (_d, s, run) = setup("toolfail");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "go"}));
    observe_hook(
        &s,
        &run,
        "PostToolUseFailure",
        &json!({"tool_name": "Edit", "tool_use_id": "t3", "tool_input": {"file_path": "/w/a.rs"}, "error": "file not found"}),
    );
    let items = items_of(&s, &turns(&s)[0].id);
    assert!(
        items
            .iter()
            .any(|i| i.kind == ItemKind::Error && i.summary == "Edit failed: file not found")
    );
    assert!(items.iter().all(|i| i.kind != ItemKind::FileChange));
}

#[test]
fn items_without_a_prompt_open_an_implicit_turn() {
    let (_d, s, run) = setup("implicit");
    observe_hook(
        &s,
        &run,
        "PreToolUse",
        &json!({"tool_name": "Read", "tool_use_id": "t1", "tool_input": {"file_path": "/x"}}),
    );
    let ts = turns(&s);
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].status, TurnStatus::Running);
    assert!(ts[0].input_summary.contains("before Vibeke saw it"));
    assert_eq!(ts[0].item_count, 1);
}

#[test]
fn a_run_ending_closes_its_open_turn_in_the_same_transaction() {
    let (_d, s, run) = setup("end");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "go"}));
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        end_run_tx(&mut c, &mut tx, "run1");
        s.commit(&mut c, tx).unwrap();
    }
    let t = &turns(&s)[0];
    assert_eq!(t.status, TurnStatus::Interrupted);
    assert!(t.ended_at_ms.is_some());
    // A late hook does not resurrect it: a new implicit turn starts instead.
    observe_hook(
        &s,
        &run,
        "PreToolUse",
        &json!({"tool_name": "Read", "tool_use_id": "z"}),
    );
    assert_eq!(turns(&s).len(), 2);
}

#[test]
fn per_turn_usage_is_the_delta_since_the_turn_began() {
    let (_d, s, _) = setup("usage");
    let mk = |i: u64, o: u64, cost: f64| RunUsage {
        input_tokens: i,
        output_tokens: o,
        cache_read_tokens: i / 2,
        cache_write_tokens: 0,
        cost_usd: Some(cost),
        model: None,
        source: "transcript".into(),
        updated_at_ms: 1,
    };
    // Session totals so far: 1000 in / 200 out.
    let run = {
        let mut c = s.core.lock().unwrap();
        let mut r = c.run("run1").cloned().unwrap();
        r.usage = mk(1000, 200, 0.5);
        let mut tx = Tx::new();
        tx.run(r.clone());
        s.commit(&mut c, tx).unwrap();
        r
    };
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "go"}));
    observe_hook(&s, &run, "Stop", &json!({}));
    assert!(
        turns(&s)[0].usage.is_none(),
        "the transcript is parsed after Stop"
    );
    // The usage observer reports new session totals afterwards.
    usage_updated(&s, "run1", &mk(1800, 450, 0.75));
    let u = turns(&s)[0].usage.clone().unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (800, 250));
    assert_eq!(u.cache_read, 400);
    assert!((u.cost_usd.unwrap() - 0.25).abs() < 1e-9);
    // A repeat of the same totals changes nothing; a later total refines the same turn.
    usage_updated(&s, "run1", &mk(1900, 450, 0.75));
    assert_eq!(turns(&s)[0].usage.clone().unwrap().input_tokens, 900);
    // The next turn starts from the new totals.
    let run = {
        let mut c = s.core.lock().unwrap();
        let mut r = c.run("run1").cloned().unwrap();
        r.usage = mk(1900, 450, 0.75);
        let mut tx = Tx::new();
        tx.run(r.clone());
        s.commit(&mut c, tx).unwrap();
        r
    };
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "again"}));
    usage_updated(&s, "run1", &mk(2000, 500, 0.8));
    assert_eq!(turns(&s)[1].usage.clone().unwrap().input_tokens, 100);
    assert_eq!(turns(&s)[0].usage.clone().unwrap().input_tokens, 900);
}

#[test]
fn a_turn_without_a_usage_baseline_gets_no_usage() {
    let (_d, s, run) = setup("nousage");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "go"}));
    usage_updated(
        &s,
        "run1",
        &RunUsage {
            input_tokens: 5,
            source: "hook".into(),
            ..Default::default()
        },
    );
    assert!(turns(&s)[0].usage.is_none());
}

#[test]
fn subagents_are_items_and_events() {
    let (_d, s, run) = setup("sub");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "go"}));
    observe_hook(
        &s,
        &run,
        "SubagentStart",
        &json!({"agent_id": "ag1", "agent_type": "Explore"}),
    );
    let sub = items_of(&s, &turns(&s)[0].id)
        .into_iter()
        .find(|i| i.kind == ItemKind::Subagent)
        .unwrap();
    assert!(sub.ended_at_ms.is_none());
    assert_eq!(sub.summary, "subagent Explore");
    observe_hook(
        &s,
        &run,
        "SubagentStop",
        &json!({"agent_id": "ag1", "agent_type": "Explore"}),
    );
    let sub = items_of(&s, &turns(&s)[0].id)
        .into_iter()
        .find(|i| i.kind == ItemKind::Subagent)
        .unwrap();
    assert!(sub.ended_at_ms.is_some());
    assert_eq!(
        events(&s, "agent.subagent_started")[0].data["agent_id"],
        "ag1"
    );
    assert_eq!(
        events(&s, "agent.subagent_finished")[0].data["agent_type"],
        "Explore"
    );
    assert_eq!(
        events(&s, "agent.subagent_started")[0].subject["run"],
        "run1"
    );
}

#[test]
fn unknown_hook_events_record_nothing() {
    let (_d, s, run) = setup("ignore");
    for e in ["SessionStart", "Notification", "PreCompact", "SessionEnd"] {
        observe_hook(&s, &run, e, &json!({}));
    }
    assert!(turns(&s).is_empty());
    assert!(events(&s, "agent.item").is_empty());
}

#[test]
fn prune_removes_old_ended_turns_with_their_items_and_keeps_the_rest() {
    let (_d, s, run) = setup("prune");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "old"}));
    observe_hook(
        &s,
        &run,
        "Stop",
        &json!({"last_assistant_message": "x".repeat(INLINE_MAX + 5)}),
    );
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "running"}));
    let old = turns(&s)[0].clone();
    // Nothing ended before the epoch; the running turn never goes.
    assert_eq!(prune(&s, 0), 0);
    let removed = prune(&s, now_ms() + 1000);
    assert_eq!(
        removed,
        1 + 2,
        "the old turn, its user message and its assistant message"
    );
    let left = turns(&s);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].input_summary, "running");
    assert!(items_of(&s, &old.id).is_empty());
    assert_eq!(items_of(&s, &left[0].id).len(), 1);
}

#[test]
fn payloads_of_live_items_survive_gc_and_orphans_go() {
    let (_d, s, run) = setup("gc");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "go"}));
    observe_hook(
        &s,
        &run,
        "Stop",
        &json!({"last_assistant_message": "y".repeat(INLINE_MAX + 5)}),
    );
    let refs = payload_refs(&s);
    assert_eq!(refs.len(), 1);
    let kept = refs.iter().next().unwrap().clone();
    let orphan =
        crate::blob_store::put_payload(&s, b"orphaned output", "txt", "text/plain", Some("pa"))
            .unwrap();
    // Everything is younger than a day: nothing goes.
    let r = crate::blob_store::gc(&s, 1, false);
    assert_eq!(r.removed, 0);
    // With no age limit the orphan goes and the referenced payload stays.
    let r = crate::blob_store::gc(&s, 0, false);
    assert_eq!(r.removed, 1);
    assert_eq!(r.hashes, vec![orphan.clone()]);
    let bs = crate::blob_store::store(&s);
    assert!(bs.find(&kept).is_some());
    assert!(bs.find(&orphan).is_none());
    // After the item is pruned the payload is unreferenced and collectable.
    prune(&s, now_ms() + 1000);
    assert!(payload_refs(&s).is_empty());
    assert_eq!(crate::blob_store::gc(&s, 0, false).removed, 1);
}

#[tokio::test]
async fn the_stream_is_readable_through_the_api() {
    let (_d, s, run) = setup("api");
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "first"}));
    observe_hook(
        &s,
        &run,
        "PreToolUse",
        &json!({"tool_name": "Bash", "tool_use_id": "a", "tool_input": {"command": "ls"}}),
    );
    observe_hook(
        &s,
        &run,
        "PostToolUse",
        &json!({"tool_name": "Bash", "tool_use_id": "a", "tool_input": {"command": "ls"}, "tool_response": "x"}),
    );
    observe_hook(&s, &run, "Stop", &json!({"last_assistant_message": "ok"}));
    observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "second"}));

    let t = dispatch(&s, &user(), "agent.turns", &json!({"run": "run1"}))
        .await
        .unwrap();
    assert_eq!(t["turns"].as_array().unwrap().len(), 2);
    assert_eq!(t["turns"][0]["status"], "completed");
    assert_eq!(t["turns"][1]["status"], "running");
    assert_eq!(t["next_after_seq"], Value::Null);
    // The handle resolves to the run id.
    let by_handle = dispatch(
        &s,
        &user(),
        "agent.turns",
        &json!({"run": "run1", "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(by_handle["turns"].as_array().unwrap().len(), 1);
    assert_eq!(by_handle["next_after_seq"], 1);
    let page2 = dispatch(
        &s,
        &user(),
        "agent.turns",
        &json!({"run": "run1", "after_seq": 1}),
    )
    .await
    .unwrap();
    assert_eq!(page2["turns"][0]["seq"], 2);

    let turn1 = t["turns"][0]["id"].as_str().unwrap();
    let i = dispatch(&s, &user(), "agent.items", &json!({"turn": turn1}))
        .await
        .unwrap();
    assert_eq!(i["items"].as_array().unwrap().len(), 5);
    let only_cmd = dispatch(
        &s,
        &user(),
        "agent.items",
        &json!({"turn": turn1, "kind": "command"}),
    )
    .await
    .unwrap();
    assert_eq!(only_cmd["items"].as_array().unwrap().len(), 1);
    let page = dispatch(
        &s,
        &user(),
        "agent.items",
        &json!({"turn": turn1, "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_after_seq"], 2);
    let rest = dispatch(
        &s,
        &user(),
        "agent.items",
        &json!({"turn": turn1, "after_seq": 2}),
    )
    .await
    .unwrap();
    assert_eq!(rest["items"][0]["seq"], 3);
    let all = dispatch(&s, &user(), "agent.items", &json!({"run": "run1"}))
        .await
        .unwrap();
    assert_eq!(all["items"].as_array().unwrap().len(), 6);

    for (p, kind) in [
        (json!({}), "invalid_params"),
        (json!({"turn": turn1, "kind": "nope"}), "invalid_params"),
        (json!({"run": "run1", "after_seq": 1}), "invalid_params"),
    ] {
        let e = dispatch(&s, &user(), "agent.items", &p).await.unwrap_err();
        assert_eq!(e.data.kind, kind, "{p}");
    }
    let e = dispatch(&s, &user(), "agent.turns", &json!({"run": "ghost"}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "not_found");
    for m in ["agent.turns", "agent.items"] {
        let e = dispatch(&s, &pane_ctx("pa"), m, &json!({"run": "run1"}))
            .await
            .unwrap_err();
        assert_eq!(e.data.kind, "permission_denied", "{m}");
    }
}

#[tokio::test]
async fn the_hook_path_feeds_the_stream_through_adapter_signal() {
    // End to end through the real hook handler: the events Claude's shim posts with the pane's
    // token (`adapter.signal`), including the per-turn usage hand-off.
    let (_d, s, _run) = setup("signal");
    let ctx = pane_ctx("pa");
    for (event, payload) in [
        ("UserPromptSubmit", json!({"prompt": "wire it"})),
        (
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "u1", "tool_input": {"command": "ls"}}),
        ),
        (
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "u1", "tool_input": {"command": "ls"}, "tool_response": "a b"}),
        ),
        ("Stop", json!({"last_assistant_message": "done"})),
    ] {
        dispatch(
            &s,
            &ctx,
            "adapter.signal",
            &json!({"harness": "claude", "event": event, "payload": payload}),
        )
        .await
        .unwrap();
    }
    let ts = turns(&s);
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].status, TurnStatus::Completed);
    assert_eq!(ts[0].input_summary, "wire it");
    let kinds: Vec<ItemKind> = items_of(&s, &ts[0].id).iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [
            ItemKind::UserMessage,
            ItemKind::Command,
            ItemKind::ToolResult,
            ItemKind::AssistantMessage
        ]
    );
}
