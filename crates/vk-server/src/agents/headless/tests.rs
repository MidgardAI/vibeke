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

fn step(a: &mut dyn Adapter, v: Value) -> Cx {
    let mut cx = Cx::default();
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
    assert!(!cx.render.is_empty(), "but rendered");
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
                },
                QuestionOption {
                    id: "b".into(),
                    label: "B".into(),
                    description: None,
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
        ["omp", "--mode", "rpc", "--resume", "o"]
    );
    let acp = vec!["agent".to_string(), "--acp".to_string()];
    assert_eq!(
        launch_argv(Harness::Claude, Kind::Acp, None, false, &[], &acp),
        acp
    );
}
