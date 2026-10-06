//! In-process tests for the session desk (R2) and drafts composer (R3). Transcripts are fixture
//! JSONL files in temp dirs; no real harness transcripts are read and no harness runs.

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::sync::Once;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-desk-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
            std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
        }
    });
}

struct Env {
    dir: tempfile::TempDir,
    root: PathBuf,
    server: Arc<Server>,
}

fn server_at(root: &Path) -> Arc<Server> {
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
    let s = Server::new(paths, opts).unwrap();
    *s.desk.config_override.lock().unwrap() = Some(DeskConfig::default());
    s
}

impl Env {
    fn new() -> Env {
        init_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let server = server_at(&root);
        let e = Env { dir, root, server };
        e.put_ws("wsA");
        e.put_ws("wsB");
        e.put_pane("pane-a", "wsA");
        e.put_pane("pane-b", "wsB");
        e
    }
    fn restart(self) -> Env {
        let Env { dir, root, server } = self;
        drop(server);
        let server = server_at(&root);
        Env { dir, root, server }
    }
    fn set_config(&self, f: impl FnOnce(&mut DeskConfig)) {
        let mut g = self.server.desk.config_override.lock().unwrap();
        f(g.as_mut().unwrap());
    }
    fn put_ws(&self, id: &str) {
        let w = Workspace {
            id: id.into(),
            handle: id.into(),
            name: Some(id.into()),
            auto_name: id.into(),
            root_path: self.root.to_string_lossy().into_owned(),
            task: None,
            order: 1.0,
            branch: None,
        };
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.ws(w);
        self.server.commit(&mut c, tx).unwrap();
    }
    fn put_pane(&self, id: &str, ws: &str) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: format!("tab-{ws}"),
            workspace: ws.into(),
            title: None,
            auto_title: String::new(),
            cwd: Some(self.root.to_string_lossy().into_owned()),
            cols: 80,
            rows: 24,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: "user".into(),
            isolation: Default::default(),
            recovered: None,
        };
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(p);
        self.server.commit(&mut c, tx).unwrap();
    }
    fn put_run(&self, r: AgentRun) {
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.run(r);
        self.server.commit(&mut c, tx).unwrap();
    }
    fn run(
        &self,
        id: &str,
        pane: &str,
        session: Option<&str>,
        transcript: Option<&Path>,
    ) -> AgentRun {
        let mut r = template_run(
            "claude",
            session.unwrap_or("x"),
            vec![],
            Some(self.root.to_string_lossy().into_owned()),
        );
        r.id = id.into();
        r.handle = id.into();
        r.pane = pane.into();
        r.integration = "hooks".into();
        r.harness_session_id = session.map(str::to_string);
        r.transcript_path = transcript.map(|p| p.to_string_lossy().into_owned());
        r.execution.value = Execution::Idle;
        r.execution.source = StateSource::Structured;
        r
    }
    fn file(&self, rel: &str, lines: &[Value]) -> PathBuf {
        let p = self.root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut s = String::new();
        for l in lines {
            s.push_str(&l.to_string());
            s.push('\n');
        }
        std::fs::write(&p, s).unwrap();
        p
    }
    fn append(&self, p: &Path, raw: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(raw.as_bytes()).unwrap();
    }
    fn pass(&self) -> PassStats {
        index_pass(&self.server, 8 << 20).unwrap()
    }
}

fn user() -> Ctx {
    Ctx {
        client_id: "tester".into(),
        kind: "tui".into(),
        pane_scope: None,
        remote: false,
    }
}

fn pane_ctx(p: &str) -> Ctx {
    Ctx {
        client_id: format!("agent-{p}"),
        kind: "cli".into(),
        pane_scope: Some(p.into()),
        remote: false,
    }
}

async fn call_as(e: &Env, ctx: &Ctx, method: &str, p: Value) -> R {
    crate::api::authorize(&e.server, ctx, method, &p)?;
    if let Some(r) = api(&e.server, ctx, method, &p).await {
        return r;
    }
    crate::drafts::api(&e.server, ctx, method, &p)
        .await
        .expect("desk or draft method")
}

async fn ok(e: &Env, method: &str, p: Value) -> Value {
    match call_as(e, &user(), method, p.clone()).await {
        Ok(v) => v,
        Err(err) => panic!("{method} {p}: {err:?}"),
    }
}

fn reason(r: R) -> String {
    let e = r.expect_err("expected an error");
    e.data
        .details
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or(&e.data.kind)
        .to_string()
}

fn claude_lines(session: &str, cwd: &str) -> Vec<Value> {
    vec![
        json!({"type": "user", "sessionId": session, "cwd": cwd, "timestamp": "2026-10-01T10:00:00.000Z", "message": {"content": "Fix the login redirect loop"}}),
        json!({"type": "assistant", "sessionId": session, "timestamp": "2026-10-01T10:00:05.000Z", "message": {"content": [{"type": "thinking", "thinking": "secret reasoning"}, {"type": "tool_use", "name": "Bash", "id": "t1", "input": {"command": "cargo test auth"}}]}}),
        json!({"type": "user", "sessionId": session, "timestamp": "2026-10-01T10:00:09.000Z", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "test auth::redirect FAILED"}]}}),
        json!({"type": "assistant", "sessionId": session, "timestamp": "2026-10-01T10:01:00.000Z", "message": {"content": [{"type": "text", "text": "We decided to keep SSO cookies.\nNext step: add a regression test for the redirect."}]}}),
    ]
}

fn codex_lines(session: &str, cwd: &str) -> Vec<Value> {
    vec![
        json!({"type": "session_meta", "timestamp": "2026-09-20T08:00:00.000Z", "payload": {"id": session, "cwd": cwd}}),
        json!({"type": "response_item", "timestamp": "2026-09-20T08:00:01.000Z", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Tune the flaky websocket reconnect"}]}}),
        json!({"type": "response_item", "timestamp": "2026-09-20T08:00:02.000Z", "payload": {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":[\"npm\",\"test\"]}", "call_id": "c1"}}),
        json!({"type": "response_item", "timestamp": "2026-09-20T08:00:03.000Z", "payload": {"type": "function_call_output", "call_id": "c1", "output": "reconnect ok"}}),
        json!({"type": "response_item", "timestamp": "2026-09-20T08:00:04.000Z", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "The websocket reconnect backoff is fixed."}]}}),
    ]
}

#[test]
fn parse_matches_transcript_turns() {
    let lines = claude_lines("s1", "/w");
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let p = parse_chunk(text.as_bytes(), 0, 0, 0);
    assert_eq!(p.session.as_deref(), Some("s1"));
    assert_eq!(p.cwd.as_deref(), Some("/w"));
    assert_eq!(p.turns, 1, "a tool result doesn't start a turn");
    let turns = crate::gateway_api::transcript_items(&text);
    assert_eq!(turns.len() as u32, p.turns);
    assert!(
        p.rows.iter().all(|r| !r.text.contains("secret reasoning")),
        "reasoning is not indexed"
    );
    assert!(p.rows.iter().any(|r| r.kind == "tool_call"
        && r.text.starts_with("Bash: ")
        && r.text.contains("cargo test auth")));
    assert_eq!(p.first_ts, Some(1_790_848_800_000));
    // A partial last line is left for the next pass.
    let partial = format!("{text}{{\"type\": \"user\", \"mess");
    let q = parse_chunk(partial.as_bytes(), 0, 0, 0);
    assert_eq!(q.consumed as usize, text.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn indexer_is_incremental_over_run_transcripts() {
    let e = Env::new();
    let cwd = e.root.to_string_lossy().into_owned();
    let t = e.file("transcripts/s1.jsonl", &claude_lines("s1", &cwd));
    e.put_run(e.run("r1", "pane-a", Some("s1"), Some(&t)));
    let st = e.pass();
    assert_eq!(st.registered, 1);
    assert_eq!(st.rows, 4, "{st:?}");
    let src = e
        .server
        .desk
        .index
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .source(&t.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(src.workspace.as_deref(), Some("wsA"));
    assert_eq!(src.session.as_deref(), Some("s1"));
    assert_eq!(src.offset, std::fs::metadata(&t).unwrap().len());
    // Nothing new: nothing read.
    assert_eq!(e.pass().bytes, 0);
    // A new turn plus a half-written line: only the complete line is indexed.
    e.append(&t, &format!("{}\n{{\"type\":\"user\",\"mess", json!({"type": "user", "sessionId": "s1", "message": {"content": "Now handle the logout redirect"}})));
    let st = e.pass();
    assert_eq!(st.rows, 1);
    assert_eq!(st.pending, 1, "the partial line waits for the next pass");
    e.append(&t, "age\":{\"content\":\"third prompt about tokens\"}}\n");
    assert_eq!(e.pass().rows, 1);
    let hits = ok(&e, "desk.search", json!({"text": "tokens"})).await;
    assert_eq!(hits["hits"][0]["turn"], 3, "{hits}");
    let hits = ok(&e, "desk.search", json!({"text": "logout redirect"})).await;
    assert_eq!(hits["hits"][0]["turn"], 2);
    assert_eq!(hits["hits"][0]["session"], "s1");
    // Truncated/replaced file: re-indexed from the start.
    std::fs::write(
        &t,
        format!(
            "{}\n",
            json!({"type": "user", "sessionId": "s1", "message": {"content": "fresh start"}})
        ),
    )
    .unwrap();
    let st = e.pass();
    assert_eq!(st.reset, 1);
    assert_eq!(st.rows, 1);
    let hits = ok(&e, "desk.search", json!({"text": "logout"})).await;
    assert_eq!(hits["hits"].as_array().unwrap().len(), 0);
    // Bounded: a tiny budget leaves work pending for the next pass.
    let t2 = e.file("transcripts/s2.jsonl", &claude_lines("s2", &cwd));
    e.put_run(e.run("r2", "pane-b", Some("s2"), Some(&t2)));
    let st = index_pass(&e.server, 200).unwrap();
    assert!(st.bytes <= 4096 && st.pending >= 1, "{st:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn roots_are_opt_in_and_filters_apply() {
    let e = Env::new();
    let cwd = e.root.to_string_lossy().into_owned();
    let codex_dir = e.root.join("codex-sessions");
    e.file(
        "codex-sessions/2026/09/20/rollout-1.jsonl",
        &codex_lines("cx-1", &cwd),
    );
    let claude_t = e.file("transcripts/s1.jsonl", &claude_lines("s1", &cwd));
    e.put_run(e.run("r1", "pane-a", Some("s1"), Some(&claude_t)));
    e.pass();
    // Off by default: the codex directory is not read.
    let v = ok(&e, "desk.search", json!({"text": "websocket"})).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 0);
    e.set_config(|c| {
        c.roots.insert(
            "codex".into(),
            vec![codex_dir.to_string_lossy().into_owned()],
        );
    });
    e.pass();
    let v = ok(&e, "desk.search", json!({"text": "websocket"})).await;
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{v}");
    assert!(
        hits.iter()
            .all(|h| h["session"] == "cx-1" && h["harness"] == "codex")
    );
    // Filters.
    let n = |v: Value| v["hits"].as_array().unwrap().len();
    assert_eq!(
        n(ok(
            &e,
            "desk.search",
            json!({"text": "redirect", "harness": "codex"})
        )
        .await),
        0
    );
    assert!(
        n(ok(
            &e,
            "desk.search",
            json!({"text": "redirect", "harness": "claude"})
        )
        .await)
            >= 2
    );
    assert_eq!(
        n(ok(
            &e,
            "desk.search",
            json!({"text": "reconnect", "since": "2026-09-25"})
        )
        .await),
        0
    );
    assert!(
        n(ok(
            &e,
            "desk.search",
            json!({"text": "reconnect", "until": "2026-09-25"})
        )
        .await)
            >= 1
    );
    assert_eq!(
        n(ok(
            &e,
            "desk.search",
            json!({"text": "redirect", "repo": "/nonexistent/repo"})
        )
        .await),
        0
    );
    assert!(n(ok(&e, "desk.search", json!({"text": "redirect", "repo": cwd})).await) >= 2);
    let sessions = ok(&e, "desk.sessions", json!({"harness": "codex"})).await;
    assert_eq!(sessions["sessions"][0]["session"], "cx-1");
    assert_eq!(
        sessions["sessions"][0]["status"], "resumable",
        "codex has a native resume"
    );
    // Removing the root from the selection purges its rows.
    e.set_config(|c| c.roots.clear());
    let st = e.pass();
    assert!(st.purged >= 4, "{st:?}");
    assert_eq!(
        n(ok(&e, "desk.search", json!({"text": "websocket"})).await),
        0
    );
    // Exclusions purge and keep out.
    e.set_config(|c| c.exclude = vec![e.root.join("transcripts").to_string_lossy().into_owned()]);
    e.pass();
    assert_eq!(
        n(ok(&e, "desk.search", json!({"text": "redirect"})).await),
        0
    );
    let status = ok(&e, "desk.status", json!({})).await;
    assert_eq!(status["selection"]["runs"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn forget_removes_rows_and_stays_forgotten() {
    let e = Env::new();
    let cwd = e.root.to_string_lossy().into_owned();
    let t = e.file("transcripts/s1.jsonl", &claude_lines("s1", &cwd));
    e.put_run(e.run("r1", "pane-a", Some("s1"), Some(&t)));
    e.pass();
    assert!(
        !ok(&e, "desk.search", json!({"text": "redirect"})).await["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let f = ok(&e, "desk.forget", json!({"session": "s1"})).await;
    assert_eq!(f["rows_deleted"], 4);
    e.append(&t, &format!("{}\n", json!({"type": "user", "sessionId": "s1", "message": {"content": "another redirect question"}})));
    e.pass();
    assert!(
        ok(&e, "desk.search", json!({"text": "redirect"})).await["hits"]
            .as_array()
            .unwrap()
            .is_empty(),
        "never re-indexed"
    );
    // The purge event carries no conversation text.
    let ev = e.server.with_core(|c| {
        c.store
            .events_after(0, 10_000, &["desk.forgotten".into()])
            .unwrap()
    });
    assert_eq!(ev.len(), 1);
    assert!(
        !serde_json::to_string(&ev[0].data)
            .unwrap()
            .contains("redirect")
    );
    // Retention by date.
    let t2 = e.file("transcripts/s2.jsonl", &claude_lines("s2", &cwd));
    e.put_run(e.run("r2", "pane-a", Some("s2"), Some(&t2)));
    e.pass();
    let f = ok(&e, "desk.forget", json!({"before": "2026-10-02"})).await;
    assert_eq!(f["rows_deleted"], 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn open_live_vs_resumable_and_resume_labels() {
    let e = Env::new();
    let cwd = e.root.to_string_lossy().into_owned();
    let t1 = e.file("transcripts/live.jsonl", &claude_lines("live-1", &cwd));
    e.put_run(e.run("r-live", "pane-a", Some("live-1"), Some(&t1)));
    let t2 = e.file("transcripts/old.jsonl", &claude_lines("old-1", &cwd));
    let mut old = e.run("r-old", "pane-b", Some("old-1"), Some(&t2));
    e.put_run(old.clone());
    e.pass();
    old.ended_at_ms = Some(vk_store::now_ms());
    old.resume_argv = vec!["claude".into(), "--resume".into(), "old-1".into()];
    e.put_run(old);

    let live = ok(&e, "desk.open", json!({"session": "live-1", "turn": 1})).await;
    assert_eq!(live["status"], "live");
    assert_eq!(live["action"], "focus_live_pane");
    assert_eq!(live["focused"], false, "focus only on explicit request");
    assert!(live["turn_items"].as_array().unwrap().len() >= 3);
    let f = ok(&e, "desk.open", json!({"session": "live-1", "focus": true})).await;
    assert_eq!(f["focused"], true);
    assert_eq!(
        e.server.client_focus("tester").pane.as_deref(),
        Some("pane-a")
    );

    let res = ok(&e, "desk.open", json!({"session": "old-1"})).await;
    assert_eq!(res["status"], "resumable");
    assert_eq!(res["action"], "resume_native_session");
    assert_eq!(res["actions"][1]["label"], "Resume native session");
    assert_eq!(res["actions"][1]["command"], "claude --resume old-1");
    assert_eq!(res["actions"][2]["label"], "Start new agent with context");
    let hits = ok(&e, "desk.search", json!({"text": "redirect", "limit": 50})).await;
    let st = |sid: &str| {
        hits["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["session"] == sid)
            .unwrap()["status"]
            .clone()
    };
    assert_eq!(st("live-1"), "live");
    assert_eq!(st("old-1"), "resumable");

    // Resuming a live session is refused (focus it instead).
    assert_eq!(
        reason(call_as(&e, &user(), "desk.resume", json!({"session": "live-1"})).await),
        "session_live"
    );
    // Start new agent with context: a reviewable package saved as a draft; nothing sent.
    let n = ok(
        &e,
        "desk.resume",
        json!({"session": "old-1", "mode": "new_agent"}),
    )
    .await;
    assert_eq!(n["label"], "Start new agent with context");
    assert_eq!(n["sent"], false);
    let text = n["draft"]["draft"]["text"].as_str().unwrap();
    assert!(
        text.contains("## Objective\nFix the login redirect loop"),
        "{text}"
    );
    assert!(text.contains("We decided to keep SSO cookies."), "{text}");
    assert!(text.contains("Next step: add a regression test"), "{text}");
    assert!(text.contains("This is not a native resume"), "{text}");
    assert_eq!(n["draft"]["draft"]["workspace"], "wsB");
    let ctxv = ok(
        &e,
        "desk.context",
        json!({"session": "old-1", "turns": "1"}),
    )
    .await;
    assert_eq!(ctxv["package"]["selected_turns"][0]["turn"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn pane_scope_sees_only_its_workspace() {
    let e = Env::new();
    let cwd = e.root.to_string_lossy().into_owned();
    let ta = e.file("transcripts/a.jsonl", &claude_lines("sa", &cwd));
    let tb = e.file("transcripts/b.jsonl", &claude_lines("sb", &cwd));
    e.put_run(e.run("ra", "pane-a", Some("sa"), Some(&ta)));
    e.put_run(e.run("rb", "pane-b", Some("sb"), Some(&tb)));
    e.pass();
    let pa = pane_ctx("pane-a");
    let v = call_as(
        &e,
        &pa,
        "desk.search",
        json!({"text": "redirect", "limit": 50}),
    )
    .await
    .unwrap();
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h["session"] == "sa"), "{v}");
    let v = call_as(&e, &pa, "desk.sessions", json!({})).await.unwrap();
    assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        call_as(&e, &pa, "desk.context", json!({"session": "sb"}))
            .await
            .unwrap_err()
            .data
            .kind,
        "not_found"
    );
    let denied = ErrorKind::PermissionDenied.code();
    for (m, p) in [
        ("desk.resume", json!({"session": "sa"})),
        ("desk.open", json!({"session": "sa", "focus": true})),
        ("desk.forget", json!({"session": "sa"})),
        ("desk.status", json!({})),
        (
            "draft.send",
            json!({"draft": "x", "target_run": "ra", "idempotency_key": "k"}),
        ),
    ] {
        assert_eq!(
            call_as(&e, &pa, m, p).await.unwrap_err().code,
            denied,
            "{m}"
        );
    }
    // Drafts: own workspace yes, other workspace no.
    let own = call_as(&e, &pa, "draft.create", json!({"text": "note to self"}))
        .await
        .unwrap();
    assert_eq!(own["draft"]["workspace"], "wsA");
    let theirs = ok(
        &e,
        "draft.create",
        json!({"id": "wsB", "text": "B private"}),
    )
    .await;
    let bid = theirs["draft"]["id"].as_str().unwrap();
    for (m, p) in [
        ("draft.get", json!({"draft": bid})),
        ("draft.update", json!({"draft": bid, "text": "x"})),
        ("draft.delete", json!({"draft": bid})),
        ("draft.create", json!({"id": "wsB", "text": "x"})),
        ("draft.list", json!({"id": "wsB"})),
        ("notes.get", json!({"workspace": "wsB"})),
        ("notes.set", json!({"workspace": "wsB", "text": "x"})),
    ] {
        assert_eq!(
            call_as(&e, &pa, m, p).await.unwrap_err().code,
            denied,
            "{m}"
        );
    }
    let all = call_as(&e, &pa, "draft.list", json!({"all": true}))
        .await
        .unwrap();
    assert!(!all.to_string().contains("B private"));
}

#[tokio::test(flavor = "multi_thread")]
async fn drafts_crud_reorder_combine_survive_restart() {
    let e = Env::new();
    let img = e.root.join("shot.png");
    std::fs::write(&img, b"png").unwrap();
    let a = ok(
        &e,
        "draft.create",
        json!({"id": "wsA", "text": "first", "idempotency_key": "c1"}),
    )
    .await;
    let again = ok(
        &e,
        "draft.create",
        json!({"id": "wsA", "text": "first", "idempotency_key": "c1"}),
    )
    .await;
    assert_eq!(again["replayed"], true);
    assert_eq!(again["draft"]["id"], a["draft"]["id"]);
    let b = ok(&e, "draft.create", json!({"id": "wsA", "text": "second", "attachments": [{"kind": "screenshot", "path": img}]})).await;
    let c = ok(&e, "draft.create", json!({"id": "wsA", "text": "third"})).await;
    let (ida, idb, idc) = (
        a["draft"]["id"].as_str().unwrap().to_string(),
        b["draft"]["id"].as_str().unwrap().to_string(),
        c["draft"]["id"].as_str().unwrap().to_string(),
    );
    assert_eq!(
        reason(
            call_as(
                &e,
                &user(),
                "draft.create",
                json!({"id": "wsA", "attachments": [{"kind": "file", "path": "relative.txt"}]})
            )
            .await
        ),
        "invalid_params"
    );
    let u = ok(
        &e,
        "draft.update",
        json!({"draft": ida, "text": "first, edited", "expected_rev": 1}),
    )
    .await;
    assert_eq!(u["draft"]["rev"], 2);
    assert_eq!(
        reason(
            call_as(
                &e,
                &user(),
                "draft.update",
                json!({"draft": ida, "text": "x", "expected_rev": 1})
            )
            .await
        ),
        "draft_changed"
    );
    ok(&e, "draft.reorder", json!({"order": [idc, ida]})).await;
    let comb = ok(
        &e,
        "draft.combine",
        json!({"ids": [ida, idb], "title": "both"}),
    )
    .await;
    let idm = comb["draft"]["id"].as_str().unwrap().to_string();
    assert_eq!(comb["draft"]["text"], "first, edited\n\nsecond");
    assert_eq!(comb["draft"]["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(comb["draft"]["combined_from"], json!([ida, idb]));
    ok(
        &e,
        "notes.set",
        json!({"workspace": "wsA", "text": "build with --release"}),
    )
    .await;
    // Events carry metadata only.
    let ev = e
        .server
        .with_core(|c| c.store.events_after(0, 10_000, &[]).unwrap());
    let blob = serde_json::to_string(&ev).unwrap();
    assert!(blob.contains("draft.created") && blob.contains("notes.updated"));
    assert!(
        !blob.contains("first, edited") && !blob.contains("--release"),
        "no content in events"
    );

    let e = e.restart();
    let list = ok(&e, "draft.list", json!({"id": "wsA"})).await;
    let ids: Vec<&str> = list["drafts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![idc.as_str(), ida.as_str(), idb.as_str(), idm.as_str()]
    );
    assert_eq!(list["drafts"][1]["text"], "first, edited");
    let n = ok(&e, "notes.get", json!({"workspace": "wsA"})).await;
    assert_eq!(n["notes"]["text"], "build with --release");
    ok(&e, "draft.delete", json!({"draft": idc})).await;
    assert_eq!(
        ok(&e, "draft.list", json!({"id": "wsA"})).await["drafts"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn draft_send_refuses_with_zero_bytes_and_reconciles_before_retry() {
    let e = Env::new();
    let d = ok(
        &e,
        "draft.create",
        json!({"id": "wsA", "text": "please add the test"}),
    )
    .await;
    let id = d["draft"]["id"].as_str().unwrap().to_string();
    // No native conversation id → no prompt-input path.
    e.put_run(e.run("r-anon", "pane-a", None, None));
    let cap = ok(&e, "draft.check", json!({"target_run": "r-anon"})).await;
    assert_eq!(cap["send_path"], "open_pane_only");
    // A run whose pane has no live screen: unsafe, refused, draft kept, no attempt recorded.
    e.put_run(e.run("r1", "pane-a", Some("s1"), None));
    let cap = ok(&e, "draft.check", json!({"target_run": "r1", "draft": id})).await;
    assert_eq!(cap["send_path"], "open_pane_only", "{cap}");
    assert_eq!(cap["steer"], false);
    let r = call_as(
        &e,
        &user(),
        "draft.send",
        json!({"draft": id, "target_run": "r1", "idempotency_key": "k1"}),
    )
    .await;
    let err = r.unwrap_err();
    let det = err.data.details.clone();
    assert_eq!(det["reason"], "send_unsafe");
    assert_eq!(det["fallback"], "open_pane_to_send");
    assert_eq!(det["draft_kept"], true);
    let got = ok(&e, "draft.get", json!({"draft": id})).await;
    assert!(got["draft"]["sends"].as_array().unwrap().is_empty());
    assert!(crate::review::receipts::lookup(&e.server, &user(), "k1").is_none());

    // An uncertain earlier attempt (e.g. interrupted by a restart): retry needs reconcile.
    {
        let mut c = e.server.core.lock().unwrap();
        let mut dr: crate::drafts::Draft =
            c.store.get(crate::drafts::K_DRAFT, &id).unwrap().unwrap();
        dr.sends.push(crate::drafts::DraftSend {
            id: "send-1".into(),
            run: "r1".into(),
            pane: "pane-a".into(),
            harness: "claude".into(),
            native_conversation_id: "s1".into(),
            text: "please add the test".into(),
            include_notes: false,
            state: crate::tracking::MessageState::Sending,
            detail: None,
            idempotency_key: "k0".into(),
            owner: "user".into(),
            turn_baseline: 1,
            reconciled: false,
            created_at_ms: 0,
            updated_at_ms: 0,
        });
        let mut tx = Tx::new();
        tx.m.put(crate::drafts::K_DRAFT, &id, None, &dr);
        e.server.commit(&mut c, tx).unwrap();
    }
    let got = ok(&e, "draft.get", json!({"draft": id})).await;
    assert_eq!(
        got["draft"]["sends"][0]["state"], "delivery_unknown",
        "not in flight here"
    );
    assert_eq!(
        reason(call_as(&e, &user(), "draft.send", json!({"draft": id, "target_run": "r1", "idempotency_key": "k2", "retry_despite_unknown": true})).await),
        "reconcile_first"
    );
    let rec = ok(&e, "draft.reconcile", json!({"draft": id})).await;
    assert_eq!(rec["send"]["state"], "delivery_unknown");
    assert_eq!(rec["may_retry"], true);
    assert_eq!(
        reason(
            call_as(
                &e,
                &user(),
                "draft.send",
                json!({"draft": id, "target_run": "r1", "idempotency_key": "k2"})
            )
            .await
        ),
        "delivery_unknown"
    );
    // A matching turn that started after the attempt proves delivery on reconcile.
    let run = e.server.with_core(|c| c.run("r1").cloned()).unwrap();
    crate::tracking::observe(
        &e.server,
        &run,
        "UserPromptSubmit",
        &json!({"session_id": "s1", "prompt": "please  add the test"}),
    );
    let rec = ok(&e, "draft.reconcile", json!({"draft": id})).await;
    assert_eq!(rec["send"]["state"], "delivered", "{rec}");
    assert_eq!(rec["may_retry"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn sent_text_references_attachments_and_includes_notes_only_on_request() {
    let e = Env::new();
    let img = e.root.join("shot.png");
    std::fs::write(&img, b"png").unwrap();
    let d = ok(&e, "draft.create", json!({"id": "wsA", "text": "look at this", "attachments": [{"kind": "screenshot", "path": img}, {"kind": "file", "data_b64": "aGk=", "name": "notes.txt"}]})).await;
    ok(
        &e,
        "notes.set",
        json!({"workspace": "wsA", "text": "staging only"}),
    )
    .await;
    let dr: crate::drafts::Draft = serde_json::from_value(d["draft"].clone()).unwrap();
    let uploaded = &dr.attachments[1].path;
    assert!(uploaded.ends_with("/notes.txt") && Path::new(uploaded).exists());
    let plain = crate::drafts::compose(&e.server, &dr, false);
    assert_eq!(
        plain,
        format!(
            "look at this\n\nAttached files:\n- {} (screenshot)\n- {uploaded}",
            img.display()
        )
    );
    let with = crate::drafts::compose(&e.server, &dr, true);
    assert!(with.ends_with("\n\nNotes:\nstaging only"), "{with}");
}
