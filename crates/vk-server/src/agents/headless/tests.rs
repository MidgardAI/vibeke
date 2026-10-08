//! Adapter state machines against protocol frames written from the protocol descriptions in
//! spec 04 (§6.1.3 stream-json, §6.2 app-server, §6.3 pi RPC, §6.6 ACP). No processes: the
//! end-to-end runs with fake harness binaries are in `crates/vibeke/tests/headless.rs`.

use super::*;

fn rec(kind: Kind, session: Option<&str>) -> Record {
    Record {
        harness: "x".into(),
        kind,
        run: "r1".into(),
        cwd: "/tmp".into(),
        session: session.map(str::to_string),
        resume: false,
        processed: 0,
        unacked: vec![],
        acp_argv: vec![],
        queued: vec![],
        auto_done: vec![],
        isolated: false,
        terminals: vec![],
        model: None,
    }
}

fn signals(cx: &Cx) -> Vec<String> {
    cx.signals.iter().map(|(e, _)| e.clone()).collect()
}

fn writes(cx: &Cx) -> Vec<Value> {
    cx.writes.iter().map(|(v, _)| v.clone()).collect()
}

/// Feed `a` the frames it wrote back as journal echoes (what the holder's `Stdin` stream does).
fn echo(a: &mut dyn Adapter, cx: &Cx) -> Cx {
    let mut out = Cx::default();
    for (v, _) in &cx.writes {
        a.on_sent(&mut out, v);
    }
    out
}

/// One live frame from the harness.
fn step(a: &mut dyn Adapter, v: Value) -> Cx {
    let mut cx = Cx::live();
    a.on_frame(&mut cx, &v);
    cx
}

fn allow() -> Answer {
    Answer {
        decision: Some(Decision::Allow),
        ..Default::default()
    }
}

#[test]
fn claude_stream_json_turn_tool_approval_and_result() {
    let mut a = claude::Claude::new(&rec(Kind::StreamJson, Some("sess-1")));
    let mut cx = Cx::default();
    a.start(&mut cx);
    assert_eq!(
        signals(&cx),
        ["SessionStart"],
        "pre-assigned id: identified at once"
    );
    assert_eq!(writes(&cx)[0]["request"]["subtype"], "initialize");
    echo(&mut a, &cx);

    let mut cx = Cx::default();
    a.prompt(&mut cx, "fix the bug", PromptMode::Send).unwrap();
    assert_eq!(
        cx.writes[0].1.as_deref(),
        Some("fix the bug"),
        "prompt preview kept"
    );
    let sent = echo(&mut a, &cx);
    assert_eq!(signals(&sent), ["UserPromptSubmit"]);
    assert!(a.busy());

    let cx = step(
        &mut a,
        json!({"type": "system", "subtype": "init", "session_id": "sess-1", "model": "m"}),
    );
    assert!(
        signals(&cx).is_empty(),
        "same id: no re-identification mid-turn"
    );
    let cx = step(
        &mut a,
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Running tests."}, {"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "cargo test"}}]}}),
    );
    assert_eq!(signals(&cx), ["PreToolUse"]);
    assert_eq!(cx.signals[0].1["tool_use_id"], "tu1");

    let cx = step(
        &mut a,
        json!({"type": "control_request", "request_id": "req-7", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "rm -rf build"}, "permission_suggestions": [{"type": "addRules"}]}}),
    );
    let it = &cx.opens[0];
    assert_eq!(it.native_ref.as_deref(), Some("rpc:req-7"));
    assert_eq!(it.kind, InteractionKind::Approval);
    assert_eq!(
        it.action.as_ref().unwrap().command.as_deref(),
        Some("rm -rf build")
    );
    assert_eq!(a.pending().len(), 1);
    let mut cx = Cx::default();
    let always = Answer {
        decision: Some(Decision::AllowAlways),
        ..Default::default()
    };
    assert!(a.answer(&mut cx, "rpc:req-7", it, &always));
    let w = &writes(&cx)[0];
    assert_eq!(w["response"]["request_id"], "req-7");
    assert_eq!(w["response"]["response"]["behavior"], "allow");
    assert_eq!(
        w["response"]["response"]["updatedInput"]["command"],
        "rm -rf build"
    );
    assert_eq!(
        w["response"]["response"]["updatedPermissions"][0]["type"],
        "addRules"
    );
    // Pending until the journal shows the response was written.
    assert_eq!(a.pending().len(), 1);
    echo(&mut a, &cx);
    assert!(a.pending().is_empty());
    assert!(
        !a.answer(&mut Cx::default(), "rpc:req-7", it, &allow()),
        "answered once"
    );

    let cx = step(
        &mut a,
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tu1", "is_error": true}]}}),
    );
    assert_eq!(signals(&cx), ["PostToolUseFailure"]);
    let cx = step(
        &mut a,
        json!({"type": "result", "subtype": "success", "is_error": false, "result": "Fixed.", "total_cost_usd": 0.25, "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 2, "cache_creation_input_tokens": 1}}),
    );
    assert_eq!(signals(&cx), ["Stop"]);
    assert_eq!(cx.signals[0].1["last_assistant_message"], "Fixed.");
    assert_eq!(cx.usage[0].0.input_tokens, 10);
    assert_eq!(cx.usage[0].0.cost_usd, Some(0.25));
    assert!(!a.busy());

    // Withdrawn requests resolve; unknown control requests are refused.
    let cx = step(
        &mut a,
        json!({"type": "control_request", "request_id": "req-8", "request": {"subtype": "can_use_tool", "tool_name": "Write", "input": {"file_path": "/tmp/x"}}}),
    );
    assert_eq!(cx.opens.len(), 1);
    let cx = step(
        &mut a,
        json!({"type": "control_cancel_request", "request_id": "req-8"}),
    );
    assert_eq!(cx.resolved, ["rpc:req-8"]);
    let cx = step(
        &mut a,
        json!({"type": "control_request", "request_id": "req-9", "request": {"subtype": "mcp_message"}}),
    );
    assert_eq!(writes(&cx)[0]["response"]["subtype"], "error");
}

#[test]
fn claude_ask_user_question_and_interrupt() {
    let mut a = claude::Claude::new(&rec(Kind::StreamJson, Some("s")));
    let cx = step(
        &mut a,
        json!({"type": "control_request", "request_id": "q1", "request": {"subtype": "can_use_tool", "tool_name": "AskUserQuestion", "input": {"questions": [{"question": "Which DB?", "header": "DB", "options": [{"label": "Postgres"}, {"label": "SQLite"}], "multiSelect": false}]}}}),
    );
    let it = cx.opens[0].clone();
    assert_eq!(it.kind, InteractionKind::Question);
    assert_eq!(it.questions[0].options.len(), 2);
    let mut cx = Cx::default();
    let ans = Answer {
        decision: None,
        choices: vec![("q0".into(), vec!["SQLite".into()])],
        text: None,
    };
    assert!(a.answer(&mut cx, "rpc:q1", &it, &ans));
    assert_eq!(
        writes(&cx)[0]["response"]["response"]["updatedInput"]["answers"]["Which DB?"],
        "SQLite"
    );
    let mut cx = Cx::default();
    a.interrupt(&mut cx);
    assert_eq!(writes(&cx)[0]["request"]["subtype"], "interrupt");
}

#[test]
fn codex_app_server_handshake_turn_approval_and_reconcile() {
    let mut a = codex::Codex::new(&rec(Kind::AppServer, None));
    let mut cx = Cx::default();
    a.start(&mut cx);
    assert_eq!(writes(&cx)[0]["method"], "initialize");
    assert!(
        writes(&cx)[0].get("jsonrpc").is_none(),
        "app-server omits jsonrpc"
    );
    echo(&mut a, &cx);
    assert!(!a.ready());
    let cx = step(&mut a, json!({"id": 1, "result": {"userAgent": "codex"}}));
    let w = writes(&cx);
    assert_eq!(w[0]["method"], "initialized");
    assert_eq!(w[1]["method"], "thread/start");
    echo(&mut a, &cx);
    let cx = step(
        &mut a,
        json!({"id": 2, "result": {"thread": {"id": "th-1"}}}),
    );
    assert_eq!(signals(&cx), ["SessionStart"]);
    assert_eq!(cx.session.as_deref(), Some("th-1"));
    assert!(a.ready());

    let mut cx = Cx::default();
    a.prompt(&mut cx, "add a test", PromptMode::Send).unwrap();
    assert_eq!(writes(&cx)[0]["method"], "turn/start");
    assert_eq!(writes(&cx)[0]["params"]["threadId"], "th-1");
    let sent = echo(&mut a, &cx);
    assert_eq!(signals(&sent), ["UserPromptSubmit"]);
    step(&mut a, json!({"id": 3, "result": {"turn": {"id": "tu-1"}}}));
    // Mid-turn prompts steer.
    let mut cx = Cx::default();
    a.prompt(&mut cx, "also lint", PromptMode::Steer).unwrap();
    assert_eq!(writes(&cx)[0]["method"], "turn/steer");
    assert_eq!(writes(&cx)[0]["params"]["expectedTurnId"], "tu-1");
    echo(&mut a, &cx);

    let cx = step(
        &mut a,
        json!({"method": "item/started", "params": {"item": {"type": "commandExecution", "id": "it-1", "command": "cargo test"}}}),
    );
    assert_eq!(signals(&cx), ["PreToolUse"]);
    let cx = step(
        &mut a,
        json!({"id": 50, "method": "item/commandExecution/requestApproval", "params": {"itemId": "it-1", "threadId": "th-1", "turnId": "tu-1", "command": "cargo test", "reason": "needs network"}}),
    );
    let it = cx.opens[0].clone();
    assert_eq!(it.native_ref.as_deref(), Some("rpc:50"));
    let mut cx = Cx::default();
    let deny = Answer {
        decision: Some(Decision::Deny),
        ..Default::default()
    };
    assert!(a.answer(&mut cx, "rpc:50", &it, &deny));
    assert_eq!(
        writes(&cx)[0],
        json!({"id": 50, "result": {"decision": "decline"}})
    );
    echo(&mut a, &cx);
    assert!(a.pending().is_empty());
    let cx = step(
        &mut a,
        json!({"method": "item/completed", "params": {"item": {"type": "commandExecution", "id": "it-1", "status": "declined"}}}),
    );
    assert_eq!(signals(&cx), ["PostToolUseFailure"]);
    let cx = step(
        &mut a,
        json!({"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {"total": {"inputTokens": 100, "cachedInputTokens": 40, "outputTokens": 7, "reasoningOutputTokens": 3}}}}),
    );
    assert_eq!(
        cx.usage[0],
        (
            RunUsage {
                input_tokens: 100,
                output_tokens: 10,
                cache_read_tokens: 40,
                cache_write_tokens: 0,
                cost_usd: None,
                model: None,
                source: "app-server".into(),
                updated_at_ms: 0,
            },
            true
        )
    );

    // Server killed mid-turn; a rebuilt adapter replays the journal up to here, then
    // reconciles with thread/read, which reports the thread idle: the turn is completed.
    let mut r = rec(Kind::AppServer, Some("th-1"));
    r.processed = 900;
    let mut b = codex::Codex::new(&r);
    assert!(b.ready(), "established before the restart");
    b.on_sent(
        &mut Cx::default(),
        &json!({"id": 3, "method": "turn/start", "params": {"threadId": "th-1", "input": [{"type": "text", "text": "add a test"}]}}),
    );
    assert!(b.busy());
    let mut cx = Cx::default();
    b.reconcile(&mut cx, true);
    let read = writes(&cx)[0].clone();
    assert_eq!(read["method"], "thread/read");
    echo(&mut b, &cx);
    let id = read["id"].as_u64().unwrap();
    let cx = step(
        &mut b,
        json!({"id": id, "result": {"thread": {"id": "th-1", "status": {"type": "idle"}}}}),
    );
    assert_eq!(signals(&cx), ["Stop"]);
    assert!(!b.busy());
}

#[test]
fn codex_legacy_approvals_questions_and_resolved_elsewhere() {
    let mut a = codex::Codex::new(&rec(Kind::AppServer, None));
    let cx = step(
        &mut a,
        json!({"id": "x1", "method": "execCommandApproval", "params": {"command": ["git", "push"], "cwd": "/r"}}),
    );
    let it = cx.opens[0].clone();
    assert_eq!(
        it.action.as_ref().unwrap().command.as_deref(),
        Some("git push")
    );
    let mut cx = Cx::default();
    assert!(a.answer(&mut cx, "rpc:x1", &it, &allow()));
    assert_eq!(writes(&cx)[0]["result"]["decision"], "approved");
    let cx = step(
        &mut a,
        json!({"id": 9, "method": "item/tool/requestUserInput", "params": {"questions": [{"id": "env", "question": "Which env?", "options": [{"label": "staging"}, {"label": "prod"}]}]}}),
    );
    let q = cx.opens[0].clone();
    let mut cx = Cx::default();
    let ans = Answer {
        decision: None,
        choices: vec![("env".into(), vec!["staging".into()])],
        text: None,
    };
    assert!(a.answer(&mut cx, "rpc:9", &q, &ans));
    assert_eq!(
        writes(&cx)[0]["result"]["answers"]["env"]["answers"][0],
        "staging"
    );
    let cx = step(
        &mut a,
        json!({"method": "serverRequest/resolved", "params": {"requestId": "x1"}}),
    );
    assert_eq!(cx.resolved, ["rpc:x1"]);
    let cx = step(
        &mut a,
        json!({"id": 10, "method": "some/newRequest", "params": {}}),
    );
    assert_eq!(writes(&cx)[0]["error"]["code"], -32601);
}

#[test]
fn pi_rpc_session_tools_dialogs_and_reconcile() {
    let mut a = pi::Pi::new(&rec(Kind::Rpc, Some("pre-1")));
    let mut cx = Cx::default();
    a.start(&mut cx);
    assert_eq!(writes(&cx)[0]["type"], "get_state");
    echo(&mut a, &cx);
    let id = writes(&cx)[0]["id"].clone();
    let cx = step(
        &mut a,
        json!({"type": "response", "id": id, "command": "get_state", "success": true, "data": {"sessionId": "pi-s1", "sessionFile": "/tmp/s.jsonl", "isStreaming": false}}),
    );
    assert_eq!(signals(&cx), ["SessionStart"]);
    assert_eq!(cx.signals[0].1["transcript_path"], "/tmp/s.jsonl");
    assert!(a.ready());

    let mut cx = Cx::default();
    a.prompt(&mut cx, "refactor", PromptMode::Send).unwrap();
    assert_eq!(writes(&cx)[0]["type"], "prompt");
    let sent = echo(&mut a, &cx);
    assert_eq!(signals(&sent), ["UserPromptSubmit"]);
    let mut cx = Cx::default();
    a.prompt(&mut cx, "smaller", PromptMode::Steer).unwrap();
    assert_eq!(writes(&cx)[0]["type"], "steer", "mid-turn steer");

    let cx = step(
        &mut a,
        json!({"type": "tool_execution_start", "toolCallId": "c1", "toolName": "edit", "args": {"path": "src/a.rs"}}),
    );
    assert_eq!(cx.signals[0].1["tool_name"], "Edit");
    assert_eq!(cx.signals[0].1["tool_input"]["file_path"], "src/a.rs");
    let cx = step(
        &mut a,
        json!({"type": "tool_execution_end", "toolCallId": "c1", "toolName": "edit", "result": {}, "isError": false}),
    );
    assert_eq!(signals(&cx), ["PostToolUse"]);
    assert_eq!(
        cx.signals[0].1["tool_input"]["file_path"], "src/a.rs",
        "input cached by call id"
    );

    // A permission extension's confirm dialog → approval, answered natively.
    let cx = step(
        &mut a,
        json!({"type": "extension_ui_request", "id": "ui-1", "method": "confirm", "title": "Allow bash?", "message": "rm -rf target"}),
    );
    let it = cx.opens[0].clone();
    assert_eq!(it.kind, InteractionKind::Approval);
    let mut cx = Cx::default();
    assert!(a.answer(&mut cx, "rpc:ui-1", &it, &allow()));
    assert_eq!(
        writes(&cx)[0],
        json!({"type": "extension_ui_response", "id": "ui-1", "confirmed": true})
    );
    echo(&mut a, &cx);
    assert!(a.pending().is_empty());
    let cx = step(
        &mut a,
        json!({"type": "extension_ui_request", "id": "ui-2", "method": "select", "title": "Model?", "options": ["fast", "smart"]}),
    );
    let q = cx.opens[0].clone();
    let mut cx = Cx::default();
    let ans = Answer {
        decision: None,
        choices: vec![("choice".into(), vec!["smart".into()])],
        text: None,
    };
    assert!(a.answer(&mut cx, "rpc:ui-2", &q, &ans));
    assert_eq!(writes(&cx)[0]["value"], "smart");
    let cx = step(
        &mut a,
        json!({"type": "extension_ui_request", "id": "ui-3", "method": "notify", "message": "hi"}),
    );
    assert!(cx.opens.is_empty(), "fire-and-forget");

    let cx = step(
        &mut a,
        json!({"type": "turn_end", "message": {"usage": {"input": 5, "output": 6, "cacheRead": 1, "cacheWrite": 0, "cost": {"total": 0.01}}}}),
    );
    assert_eq!(cx.usage[0].0.output_tokens, 6);
    let cx = step(&mut a, json!({"type": "agent_end", "messages": []}));
    assert_eq!(signals(&cx), ["Stop"]);

    // Reconcile: get_state reporting not streaming completes a turn the journal shows open.
    let mut r = rec(Kind::Rpc, Some("pi-s1"));
    r.processed = 10;
    let mut b = pi::Pi::new(&r);
    b.on_sent(
        &mut Cx::default(),
        &json!({"id": "vk-4", "type": "prompt", "message": "x"}),
    );
    let mut cx = Cx::default();
    b.reconcile(&mut cx, false);
    let gs = writes(&cx)[0].clone();
    assert_eq!(gs["type"], "get_state");
    assert_eq!(gs["id"], "vk-5", "ids continue after the journal's");
    echo(&mut b, &cx);
    let cx = step(
        &mut b,
        json!({"type": "response", "id": "vk-5", "command": "get_state", "success": true, "data": {"sessionId": "pi-s1", "isStreaming": false}}),
    );
    assert_eq!(signals(&cx), ["Stop"]);
}

#[test]
fn acp_permission_fs_and_session_load_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    let mut r = rec(Kind::Acp, None);
    r.cwd = dir.path().to_string_lossy().into_owned();
    let mut a = acp::Acp::new(&r);
    let mut cx = Cx::default();
    a.start(&mut cx);
    assert_eq!(writes(&cx)[0]["method"], "initialize");
    echo(&mut a, &cx);
    let cx = step(
        &mut a,
        json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": true}}}),
    );
    assert_eq!(writes(&cx)[0]["method"], "session/new");
    echo(&mut a, &cx);
    let cx = step(
        &mut a,
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "acp-1"}}),
    );
    assert_eq!(signals(&cx), ["SessionStart"]);

    let cx = step(
        &mut a,
        json!({"jsonrpc": "2.0", "id": 77, "method": "fs/read_text_file", "params": {"sessionId": "acp-1", "path": "a.txt"}}),
    );
    assert_eq!(writes(&cx)[0]["result"]["content"], "one\ntwo\n");
    let cx = step(
        &mut a,
        json!({"jsonrpc": "2.0", "id": 78, "method": "fs/read_text_file", "params": {"sessionId": "acp-1", "path": "/etc/hosts"}}),
    );
    assert_eq!(writes(&cx)[0]["error"]["code"], -32002, "outside the cwd");
    assert_eq!(a.pending().len(), 2, "auto answers pending until journaled");
    echo(&mut a, &cx);

    let cx = step(
        &mut a,
        json!({"jsonrpc": "2.0", "id": 5, "method": "session/request_permission", "params": {"sessionId": "acp-1", "toolCall": {"toolCallId": "tc1", "title": "Run cargo test", "kind": "execute", "rawInput": {"command": "cargo test"}}, "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"}, {"optionId": "always", "name": "Always", "kind": "allow_always"}, {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}}),
    );
    let it = cx.opens[0].clone();
    assert_eq!(it.native_ref.as_deref(), Some("rpc:5"));
    let mut cx = Cx::default();
    let always = Answer {
        decision: Some(Decision::AllowAlways),
        ..Default::default()
    };
    assert!(a.answer(&mut cx, "rpc:5", &it, &always));
    assert_eq!(writes(&cx)[0]["result"]["outcome"]["optionId"], "always");

    // Ring overflow after a restart: the adapter asks the agent to replay the session, and
    // the replayed updates rebuild the transcript without re-emitting events.
    let mut r2 = rec(Kind::Acp, Some("acp-1"));
    r2.processed = 4096;
    let mut b = acp::Acp::new(&r2);
    assert!(b.ready());
    let mut cx = Cx::default();
    b.reconcile(&mut cx, true);
    let load = writes(&cx)[0].clone();
    assert_eq!(load["method"], "session/load");
    assert_eq!(load["params"]["sessionId"], "acp-1");
    echo(&mut b, &cx);
    let cx = step(
        &mut b,
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "acp-1", "update": {"sessionUpdate": "tool_call", "toolCallId": "old", "title": "Read", "kind": "read", "status": "completed"}}}),
    );
    assert!(cx.signals.is_empty(), "history: no events");
    assert!(!cx.view.is_empty(), "but rendered");
    let id = load["id"].as_u64().unwrap();
    let cx = step(&mut b, json!({"jsonrpc": "2.0", "id": id, "result": {}}));
    assert!(cx.signals.is_empty(), "already identified");
    assert!(b.ready());
}

#[test]
fn in_pane_answers_map_lines_to_decisions_and_options() {
    let ap = approval("rpc:1".into(), "Bash", &json!({"command": "ls"}), None);
    assert_eq!(
        answer_from_line(&ap, "1").unwrap().decision,
        Some(Decision::Allow)
    );
    assert_eq!(
        answer_from_line(&ap, "2").unwrap().decision,
        Some(Decision::AllowAlways)
    );
    assert_eq!(
        answer_from_line(&ap, "n").unwrap().decision,
        Some(Decision::Deny)
    );
    assert!(answer_from_line(&ap, "9").is_none());
    assert!(numbered_prompt(&ap).contains("$ ls"));
    let q = interaction(
        InteractionKind::Question,
        "rpc:2".into(),
        "Pick",
        None,
        vec![Question {
            id: "c".into(),
            prompt: "Pick".into(),
            header: None,
            multi: false,
            options: vec![
                QuestionOption {
                    id: "a".into(),
                    label: "A".into(),
                    description: None,
                    selected: false,
                },
                QuestionOption {
                    id: "b".into(),
                    label: "B".into(),
                    description: None,
                    selected: false,
                },
            ],
            allow_free_text: false,
        }],
    );
    let a = answer_from_line(&q, "2").unwrap();
    assert_eq!(a.choices, vec![("c".to_string(), vec!["b".to_string()])]);
    assert_eq!(choice(&q, &a, "c").as_deref(), Some("B"));
}

#[test]
fn launch_argv_per_protocol() {
    let v = launch_argv(
        Harness::Claude,
        Kind::StreamJson,
        Some("s1"),
        false,
        &[],
        &[],
    );
    assert_eq!(
        v,
        [
            "claude",
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-prompt-tool",
            "stdio",
            "--session-id",
            "s1"
        ]
    );
    let v = launch_argv(
        Harness::Claude,
        Kind::StreamJson,
        Some("s1"),
        true,
        &[],
        &[],
    );
    assert_eq!(&v[v.len() - 2..], ["--resume", "s1"]);
    assert_eq!(
        launch_argv(Harness::Codex, Kind::AppServer, None, false, &[], &[]),
        ["codex", "app-server"]
    );
    assert_eq!(
        launch_argv(Harness::Pi, Kind::Rpc, Some("p"), false, &[], &[]),
        ["pi", "--mode", "rpc", "--session-id", "p"]
    );
    assert_eq!(
        launch_argv(Harness::Omp, Kind::Rpc, Some("o"), true, &[], &[]),
        ["omp", "--mode", "rpc-ui", "--resume", "o"]
    );
    let acp = vec!["agent".to_string(), "--acp".to_string()];
    assert_eq!(
        launch_argv(Harness::Claude, Kind::Acp, None, false, &[], &acp),
        acp
    );
}

// ---- sessions against an in-process server (journal replay, delivery, queued prompts) -------

mod sessions {
    use super::*;
    use crate::ServerOpts;
    use crate::paths::Paths;
    use std::path::PathBuf;

    fn init_env() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Same values as the other in-process test modules: one runtime/state root.
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

    struct T {
        _dir: tempfile::TempDir,
        server: Arc<Server>,
        pane: String,
        rec: Record,
        /// The holder journal: (stream, offset, bytes).
        journal: Vec<(Stream, u64, Vec<u8>)>,
        end: u64,
    }

    impl T {
        /// A server with one headless run on pane `p1` whose session is established.
        fn new(harness: &str, kind: Kind) -> T {
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
                env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                shims: false,
                gateway: None,
            };
            let server = Server::new(paths, opts).unwrap();
            let work = root.join("work");
            std::fs::create_dir_all(&work).unwrap();
            let pane = "p1".to_string();
            let h = Harness::from_id(harness).unwrap();
            let run = {
                let mut c = server.core.lock().unwrap();
                let run = new_run(
                    &mut c,
                    &pane,
                    h,
                    kind.integration(),
                    StateSource::Structured,
                    1.0,
                );
                let mut tx = Tx::new();
                tx.run(run.clone());
                server.commit(&mut c, tx).unwrap();
                run
            };
            let rec = Record {
                harness: harness.into(),
                kind,
                run: run.id,
                cwd: work.to_string_lossy().into_owned(),
                session: Some("sess-1".into()),
                resume: false,
                processed: 1,
                unacked: vec![],
                acp_argv: vec![],
                queued: vec![],
                auto_done: vec![],
                isolated: false,
                terminals: vec![],
                model: None,
            };
            rec.persist(&server, &pane);
            T {
                _dir: dir,
                server,
                pane,
                rec,
                journal: vec![],
                end: 4096,
            }
        }

        fn work(&self) -> PathBuf {
            PathBuf::from(&self.rec.cwd)
        }

        /// A session attached to the stored record (what a fresh server process loads).
        fn session(&self) -> Session {
            Session::load(&self.server, &self.pane).expect("record")
        }

        /// One journal line on `stream`, fed to `s`.
        fn feed(&mut self, s: &mut Session, stream: Stream, v: &Value) -> Vec<Act> {
            let mut b = serde_json::to_vec(v).unwrap();
            b.push(b'\n');
            self.feed_raw(s, stream, b)
        }

        fn feed_raw(&mut self, s: &mut Session, stream: Stream, b: Vec<u8>) -> Vec<Act> {
            let off = self.end;
            self.end += b.len() as u64;
            self.journal.push((stream, off, b.clone()));
            s.on_output(&self.server, stream, off, &b)
        }

        /// The holder writes an input: journaled on `Stdin`, marked written, acked.
        fn land(&mut self, s: &mut Session, id: u64, v: &Value) -> Vec<Act> {
            let mut acts = self.feed(s, Stream::Stdin, v);
            s.on_input_written(id);
            acts.extend(s.on_ack(&self.server, id, InputStatus::Written));
            acts
        }

        /// A restarted server: a new session replays the journal from the start (or from
        /// `gap_from`, the ring having lost what came before).
        fn restart(&self, gap_from: Option<u64>) -> (Session, Vec<Act>) {
            let mut s = self.session();
            s.begin_replay();
            if let Some(g) = gap_from {
                s.on_gap(g);
            }
            for (st, off, b) in &self.journal {
                if gap_from.is_some_and(|g| *off < g) {
                    continue;
                }
                s.on_output(&self.server, *st, *off, b);
            }
            let acts = s.replay_done(&self.server);
            (s, acts)
        }

        fn interaction(&self, native_ref: &str) -> Interaction {
            self.server.with_core(|c| {
                c.model
                    .interactions
                    .iter()
                    .find(|i| i.native_ref.as_deref() == Some(native_ref))
                    .cloned()
                    .expect("interaction")
            })
        }

        /// Delivery-state events of interaction `id`: (type, reason). Settled interactions leave
        /// the live model, so the event log is the record.
        fn delivery_events(&self, id: &str) -> Vec<(String, String)> {
            let evs = self
                .server
                .with_core(|c| c.store.events_after(0, 10_000, &[]).unwrap());
            evs.iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .filter(|e| e["subject"]["interaction"] == id)
                .filter_map(|e| {
                    let ty = e["type"].as_str()?.to_string();
                    ty.starts_with("interaction.deliver")
                        .then(|| (ty, e["data"]["reason"].as_str().unwrap_or("").to_string()))
                })
                .collect()
        }

        fn last_delivery(&self, id: &str) -> String {
            self.delivery_events(id)
                .last()
                .map(|(t, _)| t.clone())
                .unwrap_or_default()
        }

        fn decide(&self, native_ref: &str) -> (String, String) {
            let it = self.interaction(native_ref);
            let (_, key) = record_decision(&self.server, &it.id, allow(), "test", None, None, None)
                .unwrap()
                .unwrap();
            (it.id, key)
        }
    }

    fn act_writes(acts: &[Act]) -> Vec<(u64, Value)> {
        acts.iter()
            .filter_map(|a| match a {
                Act::Write { id, bytes } => Some((*id, serde_json::from_slice(bytes).unwrap())),
                _ => None,
            })
            .collect()
    }

    fn act_text(acts: &[Act]) -> String {
        acts.iter()
            .filter_map(|a| match a {
                Act::Render(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn permission(id: u64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {"sessionId": "sess-1", "toolCall": {"toolCallId": format!("tc{id}"), "title": "Run cargo test", "kind": "execute", "rawInput": {"command": "cargo test"}}, "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"}, {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}})
    }

    /// Review finding 8: an approval response recorded and enqueued but not yet journaled by
    /// the holder when the connection drops is never written a second time: a reconnect sees
    /// it in flight, and a restart re-delivers it under the same (key-derived) input id, which
    /// the holder's dedupe collapses.
    #[tokio::test]
    async fn approval_response_is_delivered_once_across_reconnect_and_restart() {
        let mut t = T::new("acp:fake", Kind::Acp);
        let mut s = t.session();
        t.feed(&mut s, Stream::Stdout, &permission(5));
        let (it, key) = t.decide("rpc:5");
        let answer = |s: &mut Session, t: &T| {
            s.on_cmd(
                &t.server,
                Cmd::Answer {
                    interaction: it.clone(),
                    native_ref: "rpc:5".into(),
                    key: key.clone(),
                },
            )
        };
        let w = act_writes(&answer(&mut s, &t));
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].1["id"], 5);
        let first_id = w[0].0;
        assert_eq!(
            first_id,
            delivery_input_id(&key),
            "id derives from the decision key"
        );

        // The holder connection drops before the response is journaled; the reconnect's
        // replay ends with the request still pending.
        s.begin_reconnect();
        let acts = s.replay_done(&t.server);
        assert!(
            act_writes(&acts).is_empty(),
            "in flight: the ledger resends it, no second response"
        );
        // An `interaction.answer` retry while it is in flight is a no-op too.
        assert!(act_writes(&answer(&mut s, &t)).is_empty());

        // The server dies instead: the restarted server re-delivers under the same input id.
        let (mut s2, acts) = t.restart(None);
        let w2 = act_writes(&acts);
        assert_eq!(w2.len(), 1, "{w2:?}");
        assert_eq!(w2[0].0, first_id, "same input id: the holder dedupes it");

        // The holder journals it once: delivered, and never written again.
        t.land(&mut s2, first_id, &w2[0].1);
        assert_eq!(t.last_delivery(&it), "interaction.delivered");
        let (_s3, acts) = t.restart(None);
        assert!(
            act_writes(&acts).is_empty(),
            "answered: never written again"
        );
    }

    /// Review finding 9: when the journal was truncated (ring overflow) and no longer shows a
    /// request, its recorded-but-unwritten decision is `delivery_unknown`, never `delivered`,
    /// and an unanswered interaction stays open instead of `resolved_elsewhere`.
    #[tokio::test]
    async fn truncated_journal_never_counts_as_delivery_evidence() {
        let mut t = T::new("acp:fake", Kind::Acp);
        let mut s = t.session();
        t.feed(&mut s, Stream::Stdout, &permission(5));
        t.feed(&mut s, Stream::Stdout, &permission(6));
        // A decision for 5 is recorded; the server crashes before writing the response.
        let (id5, _) = t.decide("rpc:5");
        let id6 = t.interaction("rpc:6").id;
        // stderr floods the ring past both requests.
        for i in 0..50 {
            t.feed_raw(&mut s, Stream::Stderr, format!("noise {i}\n").into_bytes());
        }
        let gap = t.journal.last().unwrap().1;
        let (_s, acts) = t.restart(Some(gap));
        let ev5 = t.delivery_events(&id5);
        assert!(
            !ev5.iter().any(|(ty, _)| ty == "interaction.delivered"),
            "{ev5:?}"
        );
        let (ty, reason) = ev5.last().cloned().unwrap();
        assert_eq!(ty, "interaction.delivery_unknown");
        assert!(reason.contains("journal_truncated"), "{reason}");
        let it6 = t
            .server
            .with_core(|c| c.interaction(&id6).cloned())
            .unwrap();
        assert_eq!(
            it6.status,
            InteractionStatus::Open,
            "no evidence: still asks"
        );
        assert!(act_text(&acts).contains("delivery_unknown"));

        // With the whole journal available, the written response is evidence.
        let mut t2 = T::new("acp:fake", Kind::Acp);
        let mut s = t2.session();
        t2.feed(&mut s, Stream::Stdout, &permission(7));
        let (id7, _) = t2.decide("rpc:7");
        t2.feed(
            &mut s,
            Stream::Stdin,
            &json!({"jsonrpc": "2.0", "id": 7, "result": {"outcome": {"outcome": "selected", "optionId": "once"}}}),
        );
        t2.restart(None);
        assert_eq!(t2.last_delivery(&id7), "interaction.delivered");
    }

    /// Review finding 10: a follow-up acknowledged while a Codex turn runs is persisted before
    /// the ack; a server killed before dispatch delivers it after the restart. Prompts
    /// acknowledged before the handshake finished are persisted the same way.
    #[tokio::test]
    async fn acknowledged_queued_prompts_survive_a_restart() {
        let mut t = T::new("codex", Kind::AppServer);
        let mut s = t.session();
        let acts = s.on_cmd(
            &t.server,
            Cmd::Prompt {
                text: "first".into(),
                mode: PromptMode::Send,
                ack: None,
            },
        );
        let w = act_writes(&acts);
        assert_eq!(w[0].1["method"], "turn/start");
        t.land(&mut s, w[0].0, &w[0].1);
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"id": w[0].1["id"], "result": {"turn": {"id": "turn-1", "status": "inProgress"}}}),
        );
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"method": "turn/started", "params": {"threadId": "sess-1", "turn": {"id": "turn-1"}}}),
        );
        assert!(s.adapter.busy());

        let (tx, rx) = oneshot::channel();
        let acts = s.on_cmd(
            &t.server,
            Cmd::Prompt {
                text: "and then the docs".into(),
                mode: PromptMode::FollowUp,
                ack: Some(tx),
            },
        );
        assert!(act_writes(&acts).is_empty(), "waits for the turn");
        assert_eq!(rx.await.unwrap(), Ok(()));
        let stored = Record::load(&t.server, &t.pane).unwrap();
        assert_eq!(stored.queued.len(), 1, "persisted before the ack");
        assert_eq!(stored.queued[0].text, "and then the docs");

        // The server is killed before dispatch; meanwhile the turn completes.
        drop(s);
        let (stream, off) = (Stream::Stdout, t.end);
        let mut done = serde_json::to_vec(&json!({"method": "turn/completed", "params": {"threadId": "sess-1", "turn": {"id": "turn-1", "status": "completed"}}})).unwrap();
        done.push(b'\n');
        t.end += done.len() as u64;
        t.journal.push((stream, off, done));
        let (_s, acts) = t.restart(None);
        let w: Vec<_> = act_writes(&acts)
            .into_iter()
            .filter(|(_, v)| v["method"] == "turn/start")
            .collect();
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0].1["params"]["input"][0]["text"], "and then the docs");
        let stored = Record::load(&t.server, &t.pane).unwrap();
        assert!(stored.queued.is_empty());
        assert_eq!(
            stored.unacked.len(),
            1,
            "now an ordinary unacked input (input_unconfirmed if it is lost)"
        );

        // Before the handshake: an ACP session that is not open yet.
        let t2 = T::new("acp:fake", Kind::Acp);
        let mut r = t2.rec.clone();
        r.session = None;
        r.processed = 0;
        r.persist(&t2.server, &t2.pane);
        let mut s = t2.session();
        let (tx, rx) = oneshot::channel();
        s.on_cmd(
            &t2.server,
            Cmd::Prompt {
                text: "hello early".into(),
                mode: PromptMode::Send,
                ack: Some(tx),
            },
        );
        assert_eq!(rx.await.unwrap(), Ok(()));
        let stored = Record::load(&t2.server, &t2.pane).unwrap();
        assert_eq!(stored.queued[0].text, "hello early");
    }

    /// Review finding 4: replaying the journal of a completed ACP write never repeats it, and
    /// a write carried out just before a crash (response not yet journaled) is answered after
    /// the restart without writing again.
    #[tokio::test]
    async fn acp_journal_replay_never_repeats_filesystem_writes() {
        let mut t = T::new("acp:fake", Kind::Acp);
        let file = t.work().join("notes.txt");
        let mut s = t.session();
        let req = json!({"jsonrpc": "2.0", "id": 9, "method": "fs/write_text_file", "params": {"sessionId": "sess-1", "path": "notes.txt", "content": "old"}});
        let acts = t.feed(&mut s, Stream::Stdout, &req);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
        let w = act_writes(&acts);
        assert_eq!(w.len(), 1);
        assert!(w[0].1.get("error").is_none(), "{w:?}");
        t.land(&mut s, w[0].0, &w[0].1);
        // The user edits the file afterwards; the server restarts and replays.
        std::fs::write(&file, "newer").unwrap();
        let (_s, acts) = t.restart(None);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "newer");
        assert!(act_writes(&acts).is_empty());

        // Crash between the write and its response: answered, not repeated.
        let mut t2 = T::new("acp:fake", Kind::Acp);
        let file = t2.work().join("notes.txt");
        let mut s = t2.session();
        let req = json!({"jsonrpc": "2.0", "id": 10, "method": "fs/write_text_file", "params": {"sessionId": "sess-1", "path": "notes.txt", "content": "old"}});
        t2.feed(&mut s, Stream::Stdout, &req);
        assert_eq!(
            Record::load(&t2.server, &t2.pane).unwrap().auto_done,
            ["rpc:10"],
            "recorded before the response is written"
        );
        std::fs::write(&file, "newer").unwrap();
        let (_s, acts) = t2.restart(None);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "newer");
        let w = act_writes(&acts);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].1["id"], 10);
        assert!(w[0].1.get("error").is_none());
    }

    fn terminal_req(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    /// ACP `terminal/*` at the session: a cwd outside the session cwd is refused, unknown ids
    /// are errors, `wait_for_exit` waits until the terminal's watcher reports the exit, and
    /// `output` returns the newest `outputByteLimit` bytes with the exit status.
    #[tokio::test]
    async fn acp_terminals_are_confined_and_answer_exit_and_output() {
        let mut t = T::new("acp:fake", Kind::Acp);
        let mut s = t.session();
        let acts = t.feed(
            &mut s,
            Stream::Stdout,
            &terminal_req(
                1,
                "terminal/create",
                json!({"sessionId": "sess-1", "command": "ls", "cwd": "/"}),
            ),
        );
        let w = act_writes(&acts);
        assert_eq!(w[0].1["error"]["code"], -32002, "{w:?}");
        assert!(s.rec.terminals.is_empty());
        let acts = t.feed(
            &mut s,
            Stream::Stdout,
            &terminal_req(
                2,
                "terminal/output",
                json!({"sessionId": "sess-1", "terminalId": "nope"}),
            ),
        );
        assert_eq!(act_writes(&acts)[0].1["error"]["code"], -32002);

        // A terminal as `terminal/create` leaves it (its pane is not needed here).
        s.terms.lock().unwrap().insert(
            "term_1".into(),
            acp_term::Term {
                pane: "tp".into(),
                output: "line one\nline two\n".into(),
                exit: None,
                limit: Some(9),
            },
        );
        let acts = t.feed(
            &mut s,
            Stream::Stdout,
            &terminal_req(
                3,
                "terminal/wait_for_exit",
                json!({"sessionId": "sess-1", "terminalId": "term_1"}),
            ),
        );
        assert!(act_writes(&acts).is_empty(), "waits for the exit");
        s.terms.lock().unwrap().get_mut("term_1").unwrap().exit = Some(acp_term::Exit {
            code: Some(3),
            signal: None,
        });
        let acts = s.on_cmd(&t.server, Cmd::TerminalExited("term_1".into()));
        let w = act_writes(&acts);
        assert_eq!(w[0].1["id"], 3);
        assert_eq!(w[0].1["result"], json!({"exitCode": 3, "signal": null}));
        let acts = t.feed(
            &mut s,
            Stream::Stdout,
            &terminal_req(
                4,
                "terminal/output",
                json!({"sessionId": "sess-1", "terminalId": "term_1"}),
            ),
        );
        let r = &act_writes(&acts)[0].1["result"];
        assert_eq!(r["output"], "line two\n");
        assert_eq!(r["truncated"], true);
        assert_eq!(r["exitStatus"]["exitCode"], 3);
        let acts = t.feed(
            &mut s,
            Stream::Stdout,
            &terminal_req(
                5,
                "terminal/release",
                json!({"sessionId": "sess-1", "terminalId": "term_1"}),
            ),
        );
        assert!(act_writes(&acts)[0].1.get("error").is_none());
        assert!(s.terms.lock().unwrap().is_empty());
    }

    /// A fake terminal starter that runs a non-idempotent "command" (appends to `log`).
    fn appending_spawn(log: PathBuf) -> TestSpawn {
        Box::new(move |_dir, _argv, _title| {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .unwrap();
            writeln!(f, "ran").unwrap();
            Ok("tp1".into())
        })
    }

    fn runs(log: &std::path::Path) -> usize {
        std::fs::read_to_string(log)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Final review P1 11: the server crashes after a terminal's command started but before its
    /// pane was recorded. The restarted server replays and reconciles the request: the command
    /// is reported as unknown and runs exactly once overall. A persistence failure before the
    /// start refuses the request without running anything.
    #[tokio::test]
    async fn acp_terminal_create_runs_at_most_once_across_a_crash() {
        let mut t = T::new("acp:fake", Kind::Acp);
        let log = t.work().join("ran.log");
        let mut s = t.session();
        s.term_spawn = Some(appending_spawn(log.clone()));
        s.crash_after_spawn = true;
        let acts = t.feed(
            &mut s,
            Stream::Stdout,
            &terminal_req(
                7,
                "terminal/create",
                json!({"sessionId": "sess-1", "command": "echo once >> counter"}),
            ),
        );
        assert!(act_writes(&acts).is_empty(), "crashed before answering");
        assert_eq!(runs(&log), 1);
        // The intent was durable before the command ran.
        let saved = Record::load(&t.server, &t.pane).unwrap().terminals;
        assert_eq!(saved.len(), 1);
        assert!(saved[0].pending && saved[0].pane.is_empty(), "{saved:?}");
        drop(s);

        // Restart: replay the journal and reconcile with a starter that would run it again.
        let mut s = t.session();
        s.term_spawn = Some(appending_spawn(log.clone()));
        s.begin_replay();
        for (st, off, b) in &t.journal {
            s.on_output(&t.server, *st, *off, b);
        }
        let acts = s.replay_done(&t.server);
        let w = act_writes(&acts);
        let answer = w
            .iter()
            .find(|(_, v)| v["id"] == 7)
            .map(|(_, v)| v.clone())
            .expect("the create is answered after the restart");
        assert_eq!(answer["error"]["code"], -32603, "{answer}");
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown")
        );
        assert_eq!(runs(&log), 1, "the command ran exactly once");
        // Asked again live (same request): still unknown, still not run.
        let again = t.feed(
            &mut s,
            Stream::Stdout,
            &terminal_req(
                7,
                "terminal/create",
                json!({"sessionId": "sess-1", "command": "echo once >> counter"}),
            ),
        );
        if let Some((_, v)) = act_writes(&again).first() {
            assert!(v.get("result").is_none(), "{v}");
        }
        assert_eq!(runs(&log), 1);

        // Persistence fails before the start: refused, nothing runs.
        let mut t2 = T::new("acp:fake", Kind::Acp);
        let log2 = t2.work().join("ran.log");
        let mut s2 = t2.session();
        s2.term_spawn = Some(appending_spawn(log2.clone()));
        FAIL_PERSIST.with(|f| f.set(true));
        let acts = t2.feed(
            &mut s2,
            Stream::Stdout,
            &terminal_req(
                8,
                "terminal/create",
                json!({"sessionId": "sess-1", "command": "echo x"}),
            ),
        );
        FAIL_PERSIST.with(|f| f.set(false));
        let w = act_writes(&acts);
        assert_eq!(w[0].1["error"]["code"], -32603, "{w:?}");
        assert!(
            w[0].1["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not started")
        );
        assert_eq!(runs(&log2), 0);
        assert!(s2.rec.terminals.is_empty());
        // The normal path records the pane and answers the terminal id.
        let acts = t2.feed(
            &mut s2,
            Stream::Stdout,
            &terminal_req(
                9,
                "terminal/create",
                json!({"sessionId": "sess-1", "command": "echo x"}),
            ),
        );
        let w = act_writes(&acts);
        assert!(w[0].1["result"]["terminalId"].is_string(), "{w:?}");
        let saved = Record::load(&t2.server, &t2.pane).unwrap().terminals;
        assert_eq!(saved.len(), 1);
        assert!(!saved[0].pending);
        assert_eq!(saved[0].pane, "tp1");
        assert_eq!(runs(&log2), 1);
    }

    /// An isolated run's ACP agent gets no host-side terminals or fs: refused even if it asks.
    #[tokio::test]
    async fn isolated_acp_runs_refuse_host_terminals_and_fs() {
        let mut t = T::new("acp:fake", Kind::Acp);
        t.rec.isolated = true;
        t.rec.persist(&t.server, &t.pane);
        std::fs::write(t.work().join("a.txt"), "x").unwrap();
        let mut s = t.session();
        for (i, (m, p)) in [
            (
                "terminal/create",
                json!({"sessionId": "sess-1", "command": "ls"}),
            ),
            (
                "fs/read_text_file",
                json!({"sessionId": "sess-1", "path": "a.txt"}),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let acts = t.feed(&mut s, Stream::Stdout, &terminal_req(i as u64 + 1, m, p));
            let w = act_writes(&acts);
            assert_eq!(w[0].1["error"]["code"], -32002, "{m}: {w:?}");
        }
        assert!(s.rec.terminals.is_empty());
    }

    /// `ctrl+o` in a headless pane redraws the transcript with tool calls expanded.
    #[tokio::test]
    async fn ctrl_o_toggles_the_transcript_view() {
        let mut t = T::new("codex", Kind::AppServer);
        let mut s = t.session();
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"method": "item/started", "params": {"item": {"type": "fileChange", "id": "fc1", "changes": [{"path": "a.rs", "kind": "update", "diff": "@@ -1 +1 @@\n-old\n+new\n"}]}}}),
        );
        let acts = t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"method": "item/completed", "params": {"item": {"type": "fileChange", "id": "fc1", "status": "completed", "changes": [{"path": "a.rs", "kind": "update", "diff": "@@ -1 +1 @@\n-old\n+new\n"}]}}}),
        );
        assert!(
            act_text(&acts).contains("ctrl+o to expand"),
            "{}",
            act_text(&acts)
        );
        let acts = s.on_keys(&t.server, b"\x0f");
        let text = act_text(&acts);
        assert!(text.starts_with("\x1b[H\x1b[2J\x1b[3J"), "{text:?}");
        assert!(text.contains("+new"), "{text:?}");
        let acts = s.on_keys(&t.server, b"\x0f");
        assert!(!act_text(&acts).contains("+new"));
    }

    // ---- structured model control (agent.models / agent.set_model) ---------------------------

    fn model_cmd(
        t: &T,
        s: &mut Session,
        op: ModelOp,
    ) -> (Vec<(u64, Value)>, oneshot::Receiver<Result<Value, String>>) {
        let (tx, rx) = oneshot::channel();
        let acts = s.on_cmd(&t.server, Cmd::Model { op, ack: tx });
        (act_writes(&acts), rx)
    }

    /// Codex: `model/list` lists, `agent.set_model` rides on every later `turn/start {model}`
    /// (persisted, so a restarted server keeps it) and `scope: default` writes the config key.
    #[tokio::test]
    async fn codex_models_list_and_switch_over_app_server() {
        let mut t = T::new("codex", Kind::AppServer);
        let mut s = t.session();

        let (w, mut rx) = model_cmd(&t, &mut s, ModelOp::List);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].1["method"], "model/list");
        let rid = w[0].1["id"].clone();
        t.land(&mut s, w[0].0, &w[0].1);
        assert!(rx.try_recv().is_err(), "waits for the response");
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"id": rid, "result": {"data": [
                {"id": "gpt-a", "model": "gpt-a", "displayName": "GPT A", "description": "Fast", "isDefault": true, "hidden": false},
                {"id": "gpt-b", "model": "gpt-b", "displayName": "GPT B", "description": "", "isDefault": false, "hidden": false},
                {"id": "old", "model": "old", "displayName": "Old", "description": "", "isDefault": false, "hidden": true}
            ], "nextCursor": null}}),
        );
        assert_eq!(
            rx.try_recv().unwrap().unwrap(),
            json!({"models": [
                {"id": "gpt-a", "label": "GPT A", "description": "Fast", "current": true},
                {"id": "gpt-b", "label": "GPT B", "current": false}
            ]})
        );

        // Session scope: nothing to write now; the next turn carries the model.
        let (w, mut rx) = model_cmd(
            &t,
            &mut s,
            ModelOp::Set {
                model: "gpt-b".into(),
                default: false,
            },
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            rx.try_recv().unwrap().unwrap(),
            json!({"default_changed": false})
        );
        assert_eq!(
            Record::load(&t.server, &t.pane).unwrap().model.as_deref(),
            Some("gpt-b")
        );
        let acts = s.on_cmd(
            &t.server,
            Cmd::Prompt {
                text: "go".into(),
                mode: PromptMode::Send,
                ack: None,
            },
        );
        let w = act_writes(&acts);
        assert_eq!(w[0].1["method"], "turn/start");
        assert_eq!(w[0].1["params"]["model"], "gpt-b");
        t.land(&mut s, w[0].0, &w[0].1);
        // The current model follows the turn's override.
        let (w, mut rx) = model_cmd(&t, &mut s, ModelOp::List);
        let rid = w[0].1["id"].clone();
        t.land(&mut s, w[0].0, &w[0].1);
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"id": rid, "result": {"data": [
                {"id": "gpt-a", "model": "gpt-a", "displayName": "GPT A", "description": "", "isDefault": true, "hidden": false},
                {"id": "gpt-b", "model": "gpt-b", "displayName": "GPT B", "description": "", "isDefault": false, "hidden": false}
            ]}}),
        );
        let v = rx.try_recv().unwrap().unwrap();
        assert_eq!(v["models"][0]["current"], false);
        assert_eq!(v["models"][1]["current"], true);

        // Default scope: `config/value/write {keyPath: "model"}`.
        let (w, mut rx) = model_cmd(
            &t,
            &mut s,
            ModelOp::Set {
                model: "gpt-a".into(),
                default: true,
            },
        );
        assert_eq!(w[0].1["method"], "config/value/write");
        assert_eq!(
            w[0].1["params"],
            json!({"keyPath": "model", "value": "gpt-a", "mergeStrategy": "replace"})
        );
        let rid = w[0].1["id"].clone();
        t.land(&mut s, w[0].0, &w[0].1);
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"id": rid, "result": {"status": "ok", "version": "1", "filePath": "/x/config.toml"}}),
        );
        assert_eq!(
            rx.try_recv().unwrap().unwrap(),
            json!({"default_changed": true})
        );

        // A protocol error reaches the caller.
        let (w, mut rx) = model_cmd(&t, &mut s, ModelOp::List);
        let rid = w[0].1["id"].clone();
        t.land(&mut s, w[0].0, &w[0].1);
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"id": rid, "error": {"code": -32603, "message": "catalog unavailable"}}),
        );
        let e = rx.try_recv().unwrap().unwrap_err();
        assert!(e.contains("catalog unavailable"), "{e}");

        // A restarted server keeps sending the chosen model.
        let (mut s2, _) = t.restart(None);
        let acts = s2.on_cmd(
            &t.server,
            Cmd::Prompt {
                text: "again".into(),
                mode: PromptMode::Send,
                ack: None,
            },
        );
        let w = act_writes(&acts);
        let turn = w.iter().find(|(_, v)| v["method"] == "turn/start").unwrap();
        assert_eq!(turn.1["params"]["model"], "gpt-a");
    }

    /// pi (`--mode rpc`): `get_available_models` / `set_model {provider, modelId}`; pi saves the
    /// switch as its default (so it refuses a session-only switch), omp keeps it to the session
    /// and refuses `default`.
    #[tokio::test]
    async fn pi_models_list_and_switch_over_rpc() {
        let mut t = T::new("pi", Kind::Rpc);
        let mut s = t.session();
        // Review finding: a session-only switch would still save pi's default; refused unsent.
        let (w, mut rx) = model_cmd(
            &t,
            &mut s,
            ModelOp::Set {
                model: "openrouter/vendor/m-2".into(),
                default: false,
            },
        );
        assert!(w.is_empty(), "{w:?}");
        assert!(rx.try_recv().unwrap().is_err());
        let (w, mut rx) = model_cmd(
            &t,
            &mut s,
            ModelOp::Set {
                model: "openrouter/vendor/m-2".into(),
                default: true,
            },
        );
        assert_eq!(w[0].1["type"], "set_model");
        assert_eq!(w[0].1["provider"], "openrouter");
        assert_eq!(w[0].1["modelId"], "vendor/m-2");
        let rid = w[0].1["id"].clone();
        t.land(&mut s, w[0].0, &w[0].1);
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"type": "response", "id": rid, "command": "set_model", "success": true,
                    "data": {"id": "vendor/m-2", "name": "M 2", "provider": "openrouter"}}),
        );
        assert_eq!(
            rx.try_recv().unwrap().unwrap(),
            json!({"default_changed": true})
        );

        let (w, mut rx) = model_cmd(&t, &mut s, ModelOp::List);
        assert_eq!(w[0].1["type"], "get_available_models");
        let rid = w[0].1["id"].clone();
        t.land(&mut s, w[0].0, &w[0].1);
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"type": "response", "id": rid, "command": "get_available_models", "success": true,
                    "data": {"models": [
                        {"id": "m-1", "name": "M 1", "provider": "anthropic"},
                        {"id": "vendor/m-2", "name": "M 2", "provider": "openrouter"}
                    ]}}),
        );
        assert_eq!(
            rx.try_recv().unwrap().unwrap(),
            json!({"models": [
                {"id": "anthropic/m-1", "label": "M 1", "description": "anthropic", "current": false},
                {"id": "openrouter/vendor/m-2", "label": "M 2", "description": "openrouter", "current": true}
            ]})
        );

        let (w, mut rx) = model_cmd(
            &t,
            &mut s,
            ModelOp::Set {
                model: "nope/x".into(),
                default: true,
            },
        );
        let rid = w[0].1["id"].clone();
        t.land(&mut s, w[0].0, &w[0].1);
        t.feed(
            &mut s,
            Stream::Stdout,
            &json!({"type": "response", "id": rid, "command": "set_model", "success": false, "error": "Model not found: nope/x"}),
        );
        assert!(
            rx.try_recv()
                .unwrap()
                .unwrap_err()
                .contains("Model not found")
        );

        let t2 = T::new("omp", Kind::Rpc);
        let mut s2 = t2.session();
        let (w, mut rx) = model_cmd(
            &t2,
            &mut s2,
            ModelOp::Set {
                model: "anthropic/m-1".into(),
                default: true,
            },
        );
        assert!(w.is_empty());
        assert!(rx.try_recv().unwrap().is_err());
        // omp keeps session switching.
        let (w, _rx) = model_cmd(
            &t2,
            &mut s2,
            ModelOp::Set {
                model: "anthropic/m-1".into(),
                default: false,
            },
        );
        assert_eq!(w[0].1["type"], "set_model");
    }

    /// Adapters without structured model control answer `unsupported` at once.
    #[tokio::test]
    async fn stream_json_has_no_model_control() {
        let t = T::new("claude", Kind::StreamJson);
        let mut s = t.session();
        let (w, mut rx) = model_cmd(&t, &mut s, ModelOp::List);
        assert!(w.is_empty());
        assert_eq!(rx.try_recv().unwrap().unwrap_err(), UNSUPPORTED);
    }
}

/// Review finding 3: ACP fs requests through an in-checkout symlink (file or directory) to an
/// external sentinel are refused for reads and writes; the sentinel is neither read nor changed.
#[test]
fn acp_fs_requests_never_follow_symlinks_out_of_the_cwd() {
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("secret.txt");
    std::fs::write(&sentinel, "TOP SECRET").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    std::os::unix::fs::symlink(&sentinel, cwd.join("link")).unwrap();
    std::os::unix::fs::symlink(outside.path(), cwd.join("dirlink")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("new.txt"), cwd.join("dangling")).unwrap();
    std::fs::write(cwd.join("ok.txt"), "fine").unwrap();
    std::os::unix::fs::symlink(cwd.join("ok.txt"), cwd.join("inner")).unwrap();
    let mut r = rec(Kind::Acp, Some("acp-1"));
    r.processed = 1;
    r.cwd = cwd.to_string_lossy().into_owned();
    let mut a = acp::Acp::new(&r);
    let mut n = 100;
    let mut req = |a: &mut acp::Acp, method: &str, params: Value| {
        n += 1;
        let cx = step(
            a,
            json!({"jsonrpc": "2.0", "id": n, "method": method, "params": params}),
        );
        writes(&cx)[0].clone()
    };
    for path in [
        "link".to_string(),
        "dirlink/secret.txt".into(),
        cwd.join("link").to_string_lossy().into_owned(),
        "../".to_string() + &outside.path().join("secret.txt").to_string_lossy(),
    ] {
        let r = req(&mut a, "fs/read_text_file", json!({"path": path}));
        assert_eq!(r["error"]["code"], -32002, "read {path}: {r}");
        let r = req(
            &mut a,
            "fs/write_text_file",
            json!({"path": path, "content": "pwned"}),
        );
        assert_eq!(r["error"]["code"], -32002, "write {path}: {r}");
    }
    let r = req(
        &mut a,
        "fs/write_text_file",
        json!({"path": "dangling", "content": "pwned"}),
    );
    assert_eq!(r["error"]["code"], -32002, "dangling: {r}");
    assert!(!outside.path().join("new.txt").exists());
    let r = req(
        &mut a,
        "fs/write_text_file",
        json!({"path": "dirlink/new.txt", "content": "pwned"}),
    );
    assert_eq!(r["error"]["code"], -32002);
    assert!(!outside.path().join("new.txt").exists());
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "TOP SECRET");

    // Inside the cwd: plain files and in-cwd symlinks still work.
    let r = req(&mut a, "fs/read_text_file", json!({"path": "inner"}));
    assert_eq!(r["result"]["content"], "fine");
    let r = req(
        &mut a,
        "fs/write_text_file",
        json!({"path": "sub-new.txt", "content": "made"}),
    );
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("sub-new.txt")).unwrap(),
        "made"
    );
}

fn views(cx: &Cx) -> Vec<View> {
    cx.view.clone()
}

/// omp `rpc-ui` (04 §6.3): a tool approval arrives as `tool_approval_requested` plus a dialog;
/// the dialog becomes a risk-scored approval for that tool call, answered `confirmed` or with
/// the option reading as the decision; `tool_approval_resolved` withdraws a dialog answered
/// elsewhere. pi keeps treating confirm dialogs as extension dialogs.
#[test]
fn omp_rpc_ui_tool_approvals() {
    let mut r = rec(Kind::Rpc, Some("o1"));
    r.harness = "omp".into();
    let mut a = pi::Pi::new(&r);
    let cx = step(
        &mut a,
        json!({"type": "tool_approval_requested", "toolCallId": "tc1", "toolName": "bash", "args": {"command": "rm -rf build"}, "reason": "destructive"}),
    );
    assert!(cx.opens.is_empty(), "the dialog carries the decision");
    let cx = step(
        &mut a,
        json!({"type": "extension_ui_request", "id": "d1", "method": "confirm", "title": "Allow bash?"}),
    );
    let it = cx.opens[0].clone();
    assert_eq!(it.kind, InteractionKind::Approval);
    let act = it.action.as_ref().unwrap();
    assert_eq!(act.tool, "Bash");
    assert_eq!(act.command.as_deref(), Some("rm -rf build"));
    let mut cx = Cx::default();
    assert!(a.answer(&mut cx, "rpc:d1", &it, &allow()));
    assert_eq!(writes(&cx)[0]["confirmed"], true);
    echo(&mut a, &cx);

    // A select-style approval naming its tool call.
    a.on_frame(
        &mut Cx::live(),
        &json!({"type": "tool_execution_start", "toolCallId": "tc2", "toolName": "edit", "args": {"path": "src/x.rs"}}),
    );
    let cx = step(
        &mut a,
        json!({"type": "extension_ui_request", "id": "d2", "method": "select", "toolCallId": "tc2", "title": "Edit src/x.rs?", "options": ["Allow once", "Allow always", "Deny"]}),
    );
    let it = cx.opens[0].clone();
    assert_eq!(it.kind, InteractionKind::Approval);
    assert_eq!(it.action.as_ref().unwrap().tool, "Edit");
    let pick = |a: &mut pi::Pi, d: Decision| {
        let mut cx = Cx::default();
        let ans = Answer {
            decision: Some(d),
            ..Default::default()
        };
        assert!(a.answer(&mut cx, "rpc:d2", &it, &ans));
        writes(&cx)[0]["value"].clone()
    };
    assert_eq!(pick(&mut a, Decision::AllowAlways), "Allow always");
    assert_eq!(pick(&mut a, Decision::Allow), "Allow once");
    assert_eq!(pick(&mut a, Decision::Deny), "Deny");

    // Answered in omp itself: the dialog is withdrawn.
    let cx = step(
        &mut a,
        json!({"type": "tool_approval_resolved", "toolCallId": "tc2", "approved": true}),
    );
    assert_eq!(cx.resolved, ["rpc:d2"]);
    assert!(a.pending().is_empty());

    // pi: no approval binding.
    let mut p = pi::Pi::new(&rec(Kind::Rpc, Some("p1")));
    step(
        &mut p,
        json!({"type": "tool_approval_requested", "toolCallId": "tc1", "toolName": "bash"}),
    );
    let cx = step(
        &mut p,
        json!({"type": "extension_ui_request", "id": "d1", "method": "confirm", "title": "Allow?"}),
    );
    assert_eq!(cx.opens[0].action.as_ref().unwrap().tool, "extension");
}

/// pi `get_session_stats` (04 §6.3): asked after every turn and on reconcile; its token totals
/// replace the per-turn deltas. A pi without it stays quiet.
#[test]
fn pi_session_stats_are_the_usage_total() {
    let mut a = pi::Pi::new(&rec(Kind::Rpc, Some("pi-s1")));
    let cx = step(&mut a, json!({"type": "agent_end", "messages": []}));
    let w = writes(&cx);
    let stats = w.iter().find(|v| v["type"] == "get_session_stats").unwrap();
    echo(&mut a, &cx);
    let cx = step(
        &mut a,
        json!({"type": "response", "id": stats["id"], "command": "get_session_stats", "success": true, "data": {"sessionId": "pi-s1", "tokens": {"input": 120, "output": 30, "cacheRead": 7, "cacheWrite": 2, "total": 159}, "cost": 0.42}}),
    );
    let (u, total) = &cx.usage[0];
    assert!(*total, "a session total");
    assert_eq!(
        (
            u.input_tokens,
            u.output_tokens,
            u.cache_read_tokens,
            u.cache_write_tokens
        ),
        (120, 30, 7, 2)
    );
    assert_eq!(u.cost_usd, Some(0.42));
    let cx = step(&mut a, json!({"type": "agent_end", "messages": []}));
    let id = writes(&cx)[0]["id"].clone();
    echo(&mut a, &cx);
    let cx = step(
        &mut a,
        json!({"type": "response", "id": id, "command": "get_session_stats", "success": false, "error": "unknown command"}),
    );
    assert!(
        cx.view.is_empty() && cx.usage.is_empty(),
        "quiet without it"
    );
}

/// Codex `account/rateLimits/updated` (04 §6.2): the most constrained window becomes the run's
/// rate limit; a window at 100 % reports rate limited (once).
#[test]
fn codex_rate_limit_notifications() {
    let mut a = codex::Codex::new(&rec(Kind::AppServer, Some("t")));
    let cx = step(
        &mut a,
        json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {"primary": {"usedPercent": 42, "windowDurationMins": 300, "resetsAt": 1791374400}, "secondary": {"usedPercent": 10, "windowDurationMins": 10080}}}}),
    );
    let l = &cx.rate_limits[0];
    assert!(!l.limited);
    assert_eq!(l.scope.as_deref(), Some("primary"));
    assert_eq!(l.used_percent, Some(42.0));
    assert_eq!(l.resets_at_ms, Some(1_791_374_400_000));
    let cx = step(
        &mut a,
        json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {"primary": {"usedPercent": 60}, "secondary": {"usedPercent": 100, "resetsAt": 1791374400}}}}),
    );
    assert!(cx.rate_limits[0].limited);
    assert_eq!(cx.rate_limits[0].scope.as_deref(), Some("secondary"));
    assert!(matches!(&cx.view[0], View::Text(t) if t.contains("rate limited")));
    let cx = step(
        &mut a,
        json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {"secondary": {"usedPercent": 100}}}}),
    );
    assert!(cx.view.is_empty(), "reported once");
    assert_eq!(
        usage::app_server_rate_limit(
            &json!({"rate_limits": {"primary": {"used_percent": 5, "resets_in_seconds": 60}}}),
            1000
        )
        .unwrap()
        .resets_at_ms,
        Some(61_000),
        "snake_case accepted"
    );
}

/// The transcript items adapters report: tool calls with status, diff and output.
#[test]
fn adapters_report_tool_calls_with_diffs_outputs_and_statuses() {
    let mut a = codex::Codex::new(&rec(Kind::AppServer, Some("t")));
    let cx = step(
        &mut a,
        json!({"method": "item/started", "params": {"item": {"type": "commandExecution", "id": "c1", "command": "cargo test"}}}),
    );
    assert_eq!(
        views(&cx),
        [View::ToolStart {
            id: "c1".into(),
            label: "Bash: cargo test".into()
        }]
    );
    let cx = step(
        &mut a,
        json!({"method": "item/completed", "params": {"item": {"type": "commandExecution", "id": "c1", "status": "failed", "exitCode": 101, "aggregatedOutput": "test result: FAILED"}}}),
    );
    assert_eq!(
        views(&cx),
        [View::ToolEnd {
            id: "c1".into(),
            status: ToolStatus::Failed,
            output: Some("test result: FAILED".into()),
            diff: None,
            exit_code: Some(101)
        }]
    );
    step(
        &mut a,
        json!({"method": "item/started", "params": {"item": {"type": "commandExecution", "id": "c2", "command": "rm -rf /"}}}),
    );
    let cx = step(
        &mut a,
        json!({"method": "item/completed", "params": {"item": {"type": "commandExecution", "id": "c2", "status": "declined"}}}),
    );
    assert!(matches!(
        &cx.view[0],
        View::ToolEnd {
            status: ToolStatus::Declined,
            ..
        }
    ));

    // Claude: Edit input becomes the diff, tool_result content the output.
    let mut c = claude::Claude::new(&rec(Kind::StreamJson, Some("s")));
    step(
        &mut c,
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "e1", "name": "Edit", "input": {"file_path": "a.rs", "old_string": "x = 1", "new_string": "x = 2"}}]}}),
    );
    let cx = step(
        &mut c,
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "e1", "content": [{"type": "text", "text": "edited"}]}]}}),
    );
    match &cx.view[0] {
        View::ToolEnd {
            status,
            diff,
            output,
            ..
        } => {
            assert_eq!(*status, ToolStatus::Done);
            assert_eq!(diff.as_deref(), Some("@@ -1,1 +1,1 @@\n-x = 1\n+x = 2\n"));
            assert_eq!(output.as_deref(), Some("edited"));
        }
        v => panic!("{v:?}"),
    }

    // pi: the edit tool's own diff.
    let mut p = pi::Pi::new(&rec(Kind::Rpc, Some("p")));
    step(
        &mut p,
        json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "edit", "args": {"path": "a.rs", "oldText": "a", "newText": "b"}}),
    );
    let cx = step(
        &mut p,
        json!({"type": "tool_execution_end", "toolCallId": "t1", "toolName": "edit", "result": {"content": [{"type": "text", "text": "ok"}], "details": {"diff": "-a\n+b\n"}}, "isError": false}),
    );
    assert!(matches!(&cx.view[0], View::ToolEnd { diff: Some(d), .. } if d == "-a\n+b\n"));

    // ACP: diff content.
    let mut r = rec(Kind::Acp, Some("s"));
    r.processed = 1;
    let mut acp = acp::Acp::new(&r);
    step(
        &mut acp,
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": {"sessionUpdate": "tool_call", "toolCallId": "k1", "title": "Edit a.rs", "kind": "edit"}}}),
    );
    let cx = step(
        &mut acp,
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": {"sessionUpdate": "tool_call_update", "toolCallId": "k1", "status": "completed", "content": [{"type": "diff", "path": "a.rs", "oldText": "1", "newText": "2"}]}}}),
    );
    assert!(matches!(&cx.view[0], View::ToolEnd { diff: Some(d), .. } if d.contains("+2")));
}

/// ACP capabilities and `terminal/*` routing: terminals are offered (and handed to the session)
/// unless the run is isolated, in which case neither fs nor terminals are offered and requests
/// are refused on the spot.
#[test]
fn acp_terminal_capability_and_isolation() {
    let mut a = acp::Acp::new(&rec(Kind::Acp, None));
    let mut cx = Cx::default();
    a.start(&mut cx);
    let caps = &writes(&cx)[0]["params"]["clientCapabilities"];
    assert_eq!(caps["terminal"], true);
    assert_eq!(caps["fs"]["readTextFile"], true);
    let cx = step(
        &mut a,
        json!({"jsonrpc": "2.0", "id": 4, "method": "terminal/create", "params": {"sessionId": "s", "command": "ls"}}),
    );
    assert!(writes(&cx).is_empty(), "the session answers terminals");
    assert_eq!(cx.terminal[0].0, "rpc:4");
    assert_eq!(cx.terminal[0].1.method, "terminal/create");
    // Unanswered after a restart: reconcile hands it to the session again.
    let mut cx = Cx::live();
    a.reconcile(&mut cx, false);
    assert_eq!(cx.terminal.len(), 1);

    let mut r = rec(Kind::Acp, None);
    r.isolated = true;
    let mut b = acp::Acp::new(&r);
    let mut cx = Cx::default();
    b.start(&mut cx);
    let caps = &writes(&cx)[0]["params"]["clientCapabilities"];
    assert_eq!(caps["terminal"], false);
    assert_eq!(caps["fs"]["writeTextFile"], false);
    let cx = step(
        &mut b,
        json!({"jsonrpc": "2.0", "id": 5, "method": "terminal/create", "params": {"sessionId": "s", "command": "ls"}}),
    );
    assert!(cx.terminal.is_empty());
    assert_eq!(writes(&cx)[0]["error"]["code"], -32002);
}

/// Final review P2: resuming a headless run into a split's destination pane continues it in
/// that pane's cwd (the new worktree) and workspace, not the original cwd.
#[test]
fn headless_resume_into_a_pane_uses_its_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let s = crate::hardening::testkit::server(dir.path(), "resume");
    let mut pane = crate::hardening::testkit::sample_pane("dest", "wdest");
    pane.cwd = Some("/work/new-worktree".into());
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(pane);
        s.commit(&mut c, tx).unwrap();
    }
    let mut run = crate::hardening::testkit::sample_run("r1", "src");
    run.harness = "codex".into();
    run.harness_session_id = Some("th-1".into());
    run.cwd = Some("/work/shared".into());
    let p = resume_params(&s, &run, Some("dest")).unwrap();
    assert_eq!(p["cwd"], "/work/new-worktree");
    assert_eq!(p["pane"], "dest");
    assert_eq!(p["resume"], "th-1");
    // Without a pane: where it ran.
    let p = resume_params(&s, &run, None).unwrap();
    assert_eq!(p["cwd"], "/work/shared");
    assert!(p.get("pane").is_none());
}

/// Codex's own sandbox is switched off only at the sandbox level, with the configured args
/// inserted after the binary (13 §3, 04 §6.2); the relay argv of a shared app-server.
#[test]
fn codex_isolated_args_and_relay_argv() {
    let cfg = vk_config::Config::default();
    let argv = launch_argv(Harness::Codex, Kind::AppServer, None, false, &[], &[]);
    assert_eq!(
        isolated_argv(
            Harness::Codex,
            Some(IsolationLevel::Sandbox),
            argv.clone(),
            &cfg
        ),
        [
            "codex",
            "-c",
            "sandbox_mode=\"danger-full-access\"",
            "app-server"
        ]
    );
    assert_eq!(
        isolated_argv(
            Harness::Codex,
            Some(IsolationLevel::Container),
            argv.clone(),
            &cfg
        ),
        argv
    );
    assert_eq!(
        isolated_argv(Harness::Codex, None, argv.clone(), &cfg),
        argv
    );
    let claude = launch_argv(Harness::Claude, Kind::StreamJson, None, false, &[], &[]);
    assert_eq!(
        isolated_argv(
            Harness::Claude,
            Some(IsolationLevel::Sandbox),
            claude.clone(),
            &cfg
        ),
        claude
    );
    let relay = codex_mux::relay_argv(
        std::path::Path::new("/bin/vibeke"),
        std::path::Path::new("/run/codex-mux.sock"),
        "pane1",
        argv,
    );
    assert_eq!(
        relay,
        [
            "/bin/vibeke",
            "codex-mux",
            "--socket",
            "/run/codex-mux.sock",
            "--key",
            "pane1",
            "--",
            "codex",
            "app-server"
        ]
    );
}
