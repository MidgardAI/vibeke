//! Connection-level authorization (09 §3.2, review batch 2): an elevated render session keeps
//! its grant, revocation closes event subscriptions, read-only connections can't mutate through
//! JSON-RPC, and a read-only render client never leads geometry. In-process servers over
//! in-memory streams; the "pane process" is this test process (a pane whose child pid is ours).

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use vk_proto::frame::asyncio;
use vk_proto::model::Pane;
use vk_proto::render::{ClientFrame, PaneRect, ServerFrame};

fn server() -> (tempfile::TempDir, Arc<Server>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let opts = ServerOpts {
        session: "t".into(),
        machine: "m".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![],
        shims: false,
    };
    (dir, Server::new(paths, opts).unwrap())
}

fn pane(id: &str, child_pid: Option<u32>) -> Pane {
    Pane {
        id: id.into(),
        handle: id.into(),
        tab: "tab-a".into(),
        workspace: "ws-a".into(),
        title: None,
        auto_title: String::new(),
        cwd: None,
        cols: 80,
        rows: 24,
        child_pid,
        fg_cmdline: vec![],
        exited: false,
        exit_code: None,
        unread: false,
        marked_unread: false,
        pinned: false,
        created_by: "user".into(),
        recovered: None,
        isolation: Default::default(),
        browser: None,
    }
}

fn put_pane(server: &Server, p: Pane) {
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.pane(p);
    server.commit(&mut c, tx).unwrap();
}

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "tui".into(),
        pane_scope: None,
        remote: false,
    }
}

fn pane_ctx(p: &str) -> Ctx {
    Ctx {
        client_id: format!("c-{p}"),
        kind: "cli".into(),
        pane_scope: Some(p.into()),
        remote: false,
    }
}

fn emit(server: &Server, n: u32) {
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.event(
        "notes.updated",
        json!({"workspace": "w"}),
        json!({"rev": n, "bytes": 0}),
    );
    server.commit(&mut c, tx).unwrap();
}

/// An approved elevation for `pane`: the token the pane collects.
async fn elevation(srv: &Arc<Server>, pane: &str) -> String {
    let req = api::dispatch(
        srv,
        &pane_ctx(pane),
        "auth.elevate",
        &json!({"wait": false, "reason": "test"}),
    )
    .await
    .unwrap();
    let id = req["request"].as_str().unwrap().to_string();
    api::dispatch(
        srv,
        &user(),
        "auth.elevate.decide",
        &json!({"request": id, "decision": "approve"}),
    )
    .await
    .unwrap();
    let got = api::dispatch(
        srv,
        &pane_ctx(pane),
        "auth.elevate",
        &json!({"request": id, "timeout_ms": 5000}),
    )
    .await
    .unwrap();
    got["token"].as_str().unwrap().to_string()
}

type Rd = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
type Wr = tokio::io::WriteHalf<tokio::io::DuplexStream>;

/// A control connection whose peer is process `pid` (`None`: outside every pane).
fn connect(srv: &Arc<Server>, pid: Option<i32>) -> (Rd, Wr) {
    let (client, server_end) = tokio::io::duplex(1 << 22);
    tokio::spawn(connection(srv.clone(), server_end, pid));
    let (rd, wr) = tokio::io::split(client);
    (BufReader::new(rd), wr)
}

async fn send(wr: &mut Wr, id: u64, method: &str, params: Value) {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    wr.write_all(format!("{req}\n").as_bytes()).await.unwrap();
}

/// Lines until the response `id`; notifications on the way are collected.
async fn until(rd: &mut Rd, id: u64, notes: &mut Vec<Value>) -> Value {
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), rd.read_line(&mut line))
            .await
            .expect("response in time")
            .unwrap();
        assert!(n > 0, "connection closed");
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == json!(id) {
            return v;
        }
        if v.get("method").is_some() {
            notes.push(v);
        }
    }
}

async fn call(rd: &mut Rd, wr: &mut Wr, id: u64, method: &str, params: Value) -> Value {
    send(wr, id, method, params).await;
    until(rd, id, &mut vec![]).await
}

async fn attach(rd: &mut Rd, wr: &mut Wr, id: u64) {
    let r = call(
        rd,
        wr,
        id,
        "render.attach",
        json!({"protocol": vk_proto::render::PROTOCOL}),
    )
    .await;
    assert!(r.get("result").is_some(), "attach: {r}");
}

/// Render frames until `pick` returns something (bounded).
async fn frame_until<T>(rd: &mut Rd, mut pick: impl FnMut(ServerFrame) -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let f: ServerFrame =
            tokio::time::timeout_at(deadline, asyncio::read_frame::<_, ServerFrame>(rd))
                .await
                .expect("frame in time")
                .expect("frame");
        if let Some(v) = pick(f) {
            return v;
        }
    }
}

async fn command(rd: &mut Rd, wr: &mut Wr, req: u64, method: &str, params: Value) -> Value {
    let json = json!({"jsonrpc": "2.0", "id": req, "method": method, "params": params}).to_string();
    asyncio::write_frame(wr, &ClientFrame::Command { req, json })
        .await
        .unwrap();
    frame_until(rd, |f| match f {
        ServerFrame::CommandResult { req: r, json } if r == req => {
            Some(serde_json::from_str::<Value>(&json).unwrap())
        }
        ServerFrame::Goodbye { reason } => panic!("goodbye before the result: {reason}"),
        _ => None,
    })
    .await
}

fn me() -> (u32, Option<i32>) {
    let pid = std::process::id();
    (pid, Some(pid as i32))
}

/// Finding 1: an elevated pane that attaches a render stream stays elevated there: it can't
/// decide elevation requests, and revocation or expiry ends the session.
#[tokio::test(flavor = "multi_thread")]
async fn elevated_render_session_keeps_grant_identity_expiry_and_revocation() {
    for end in ["revoke", "expire"] {
        let (_d, srv) = server();
        let (pid, peer) = me();
        put_pane(&srv, pane("pane-a", Some(pid)));
        let token = elevation(&srv, "pane-a").await;
        let (mut rd, mut wr) = connect(&srv, peer);
        let h = call(&mut rd, &mut wr, 1, "client.hello", json!({"token": token})).await;
        assert_eq!(h["result"]["capabilities"], json!(["*"]), "elevated: {h}");
        attach(&mut rd, &mut wr, 2).await;
        // Full scope while the grant lives...
        let ok = command(&mut rd, &mut wr, 3, "server.status", json!({})).await;
        assert!(ok.get("result").is_some(), "{ok}");
        // ... but never the elevation decision, on the render stream either.
        let pending = api::dispatch(
            &srv,
            &pane_ctx("pane-a"),
            "auth.elevate",
            &json!({"wait": false}),
        )
        .await
        .unwrap();
        let d = command(
            &mut rd,
            &mut wr,
            4,
            "auth.elevate.decide",
            json!({"request": pending["request"], "decision": "approve"}),
        )
        .await;
        assert_eq!(d["error"]["data"]["kind"], "permission_denied", "{d}");
        if end == "revoke" {
            api::dispatch(
                &srv,
                &user(),
                "auth.revoke_token",
                &json!({"pane": "pane-a"}),
            )
            .await
            .unwrap();
            // The session ends on its own, without waiting for another frame.
            let why = frame_until(&mut rd, |f| match f {
                ServerFrame::Goodbye { reason } => Some(reason),
                _ => None,
            })
            .await;
            assert!(why.starts_with("elevation_expired"), "{why}");
        } else {
            crate::auth::expire_all_for_test(&srv);
            // The next frame (a command) is refused by ending the session.
            let json = json!({"jsonrpc": "2.0", "id": 5, "method": "workspace.create", "params": {"name": "after"}}).to_string();
            asyncio::write_frame(&mut wr, &ClientFrame::Command { req: 5, json })
                .await
                .unwrap();
            let why = frame_until(&mut rd, |f| match f {
                ServerFrame::CommandResult { json, .. } => panic!("ran after expiry: {json}"),
                ServerFrame::Goodbye { reason } => Some(reason),
                _ => None,
            })
            .await;
            assert!(why.starts_with("elevation_expired"), "{why}");
            assert!(
                srv.with_core(|c| c
                    .model
                    .workspaces
                    .iter()
                    .all(|w| w.display_name() != "after")),
                "no side effect after expiry"
            );
        }
        // The pending request is still undecided.
        let l = api::dispatch(&srv, &user(), "auth.list", &json!({}))
            .await
            .unwrap();
        assert!(
            l["pending"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["request"] == pending["request"]),
            "{l}"
        );
    }
}

/// Finding 3: `events.subscribe` goes through revocation checks; revocation closes an open
/// subscription and a new one from the same process is refused.
#[tokio::test(flavor = "multi_thread")]
async fn revocation_closes_subscriptions_and_refuses_new_ones() {
    let (_d, srv) = server();
    let (pid, peer) = me();
    put_pane(&srv, pane("pane-a", Some(pid)));
    let (mut rd, mut wr) = connect(&srv, peer);
    let mut notes = vec![];
    send(
        &mut wr,
        1,
        "events.subscribe",
        json!({"types": ["notes.*"]}),
    )
    .await;
    let r = until(&mut rd, 1, &mut notes).await;
    let sid = r["result"]["subscription_id"].as_str().unwrap().to_string();
    emit(&srv, 1);
    send(&mut wr, 2, "server.status", json!({})).await;
    let _ = until(&mut rd, 2, &mut notes).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&mut wr, 3, "server.status", json!({})).await;
    let _ = until(&mut rd, 3, &mut notes).await;
    assert!(
        notes.iter().any(|n| n["method"] == "events.event"),
        "delivery before revocation: {notes:?}"
    );
    notes.clear();
    api::dispatch(
        &srv,
        &user(),
        "auth.revoke_token",
        &json!({"pane": "pane-a"}),
    )
    .await
    .unwrap();
    for n in 2..20 {
        emit(&srv, n);
    }
    // Ordered after the close: the refusal of a new subscription.
    send(
        &mut wr,
        4,
        "events.subscribe",
        json!({"types": ["notes.*"]}),
    )
    .await;
    let r = until(&mut rd, 4, &mut notes).await;
    assert_eq!(r["error"]["data"]["kind"], "permission_denied", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("token_revoked"),
        "{r}"
    );
    send(
        &mut wr,
        5,
        "events.subscribe",
        json!({"types": ["notes.*"], "after": 0}),
    )
    .await;
    let r = until(&mut rd, 5, &mut notes).await;
    assert_eq!(
        r["error"]["data"]["kind"], "permission_denied",
        "history too: {r}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(&mut wr, 6, "server.status", json!({})).await;
    let _ = until(&mut rd, 6, &mut notes).await;
    assert!(
        notes
            .iter()
            .any(|n| n["method"] == "events.closed" && n["params"]["subscription_id"] == json!(sid)),
        "closed notice: {notes:?}"
    );
    assert!(
        notes.iter().all(|n| n["method"] != "events.event"),
        "no delivery after revocation: {notes:?}"
    );
}

/// Finding 4: after `client.hello {readonly: true}` every mutating JSON-RPC method is refused
/// with `permission_denied` and changes nothing; reads still work.
#[tokio::test(flavor = "multi_thread")]
async fn readonly_hello_refuses_every_mutation_without_side_effects() {
    let (_d, srv) = server();
    let (mut rd, mut wr) = connect(&srv, None);
    let h = call(
        &mut rd,
        &mut wr,
        1,
        "client.hello",
        json!({"readonly": true}),
    )
    .await;
    assert!(h.get("result").is_some(), "{h}");
    let before = srv.with_core(|c| {
        (
            c.model.workspaces.len(),
            c.model.tabs.len(),
            c.store.policy_rules().unwrap_or_default().len(),
        )
    });
    let mutations = [
        ("workspace.create", json!({"name": "ro"})),
        ("tab.create", json!({})),
        ("pane.send_text", json!({"pane": "p", "text": "rm -rf ~\n"})),
        ("notification.send", json!({"title": "x"})),
        (
            "policy.add",
            json!({"match": {"tool": "Bash"}, "effect": "allow"}),
        ),
        ("blob.put", json!({"data_b64": "eA=="})),
        ("no.such.method", json!({})),
    ];
    // Methods with effects outside this process (the user's config, new servers) are covered
    // by the same catalog flag; they are not called here, so a regression can't touch real
    // state.
    for m in [
        "config.set",
        "session.create",
        "server.restart",
        "server.stop",
    ] {
        assert!(
            crate::session_api::is_mutating(m),
            "{m} is flagged mutating"
        );
    }
    for (i, (m, p)) in mutations.iter().enumerate() {
        assert!(
            crate::session_api::is_mutating(m),
            "{m} is flagged mutating"
        );
        let r = call(&mut rd, &mut wr, 10 + i as u64, m, p.clone()).await;
        assert_eq!(r["error"]["data"]["kind"], "permission_denied", "{m}: {r}");
        assert_eq!(
            r["error"]["data"]["details"]["reason"], "readonly",
            "{m}: {r}"
        );
    }
    let after = srv.with_core(|c| {
        (
            c.model.workspaces.len(),
            c.model.tabs.len(),
            c.store.policy_rules().unwrap_or_default().len(),
        )
    });
    assert_eq!(before, after, "no side effects");
    // Reads work; a later hello can't lift read-only.
    let r = call(&mut rd, &mut wr, 50, "workspace.list", json!({})).await;
    assert!(r.get("result").is_some(), "{r}");
    let _ = call(
        &mut rd,
        &mut wr,
        51,
        "client.hello",
        json!({"readonly": false}),
    )
    .await;
    let r = call(
        &mut rd,
        &mut wr,
        52,
        "workspace.create",
        json!({"name": "ro"}),
    )
    .await;
    assert_eq!(r["error"]["data"]["kind"], "permission_denied", "{r}");
}

/// P2: a read-only render client never becomes the geometry leader, whatever it sends.
#[tokio::test(flavor = "multi_thread")]
async fn readonly_attach_never_leads_geometry() {
    let (_d, srv) = server();
    put_pane(&srv, pane("pane-a", None));
    let (mut rd_a, mut wr_a) = connect(&srv, None);
    let _ = call(
        &mut rd_a,
        &mut wr_a,
        1,
        "client.hello",
        json!({"client_id": "rw"}),
    )
    .await;
    attach(&mut rd_a, &mut wr_a, 2).await;
    let ok = command(&mut rd_a, &mut wr_a, 3, "server.status", json!({})).await;
    assert!(ok.get("result").is_some());
    assert_eq!(srv.geometry_leader.lock().unwrap().as_deref(), Some("rw"));
    let (mut rd_b, mut wr_b) = connect(&srv, None);
    let _ = call(
        &mut rd_b,
        &mut wr_b,
        1,
        "client.hello",
        json!({"client_id": "ro", "readonly": true}),
    )
    .await;
    attach(&mut rd_b, &mut wr_b, 2).await;
    for f in [
        ClientFrame::Focus {
            pane: "pane-a".into(),
        },
        ClientFrame::ViewHint {
            panes: vec![PaneRect {
                pane: "pane-a".into(),
                cols: 20,
                rows: 5,
            }],
            active: true,
        },
    ] {
        asyncio::write_frame(&mut wr_b, &f).await.unwrap();
    }
    // A command after them: the frames above were handled when its result arrives.
    let r = command(&mut rd_b, &mut wr_b, 3, "server.status", json!({})).await;
    assert!(r.get("result").is_some(), "{r}");
    assert_eq!(
        srv.geometry_leader.lock().unwrap().as_deref(),
        Some("rw"),
        "the observer did not take the geometry lease"
    );
}

fn agent_run(id: &str, pane: &str) -> vk_proto::model::AgentRun {
    use vk_proto::model::*;
    AgentRun {
        id: id.into(),
        handle: id.into(),
        name: None,
        pane: pane.into(),
        harness: "claude".into(),
        harness_version: None,
        integration: "hooks".into(),
        harness_session_id: None,
        transcript_path: None,
        resume_argv: vec![],
        cwd: None,
        model: None,
        task: None,
        execution: Facet {
            value: Execution::Working,
            since_ms: 0,
            source: StateSource::Structured,
            confidence: 1.0,
            detail: None,
        },
        health: AdapterHealth::Healthy,
        yolo: false,
        permission_mode: None,
        last_message: None,
        last_tool: None,
        turns_completed: 0,
        done_rev: 0,
        started_at_ms: 0,
        ended_at_ms: None,
        capabilities: vec![],
        usage: Default::default(),
        rate_limit: None,
    }
}

/// Input bytes that reached a pane's runtime so far.
fn drained(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::pane::PaneCmd>) -> Vec<u8> {
    let mut out = vec![];
    while let Ok(c) = rx.try_recv() {
        if let crate::pane::PaneCmd::Input { bytes, .. } = c {
            out.extend(bytes);
        }
    }
    out
}

/// Finding 9: the server enforces sync-input agent exclusion. An agent started in a synced pane
/// after the client's model was current (this client never reads the model update) gets no
/// mirrored keys or pastes unless the client explicitly included it.
#[tokio::test(flavor = "multi_thread")]
async fn mirrored_input_never_reaches_a_live_agent_pane_unless_included() {
    use vk_proto::input::{Key, KeyEvent, Mods};
    use vk_proto::render::{AckStatus, SyncPayload};
    let (_d, srv) = server();
    let mut rxs = std::collections::HashMap::new();
    for id in ["shell", "agent"] {
        put_pane(&srv, pane(id, None));
        let (rt, rx) = crate::pane::PaneRt::new(id, 80, 24);
        srv.panes.lock().unwrap().insert(id.into(), rt);
        rxs.insert(id, rx);
    }
    let (mut rd, mut wr) = connect(&srv, None);
    attach(&mut rd, &mut wr, 1).await;
    // The agent starts now; the client's model (never re-read here) still shows a shell.
    {
        let mut c = srv.core.lock().unwrap();
        let mut tx = crate::core::Tx::new();
        tx.run(agent_run("r1", "agent"));
        srv.commit(&mut c, tx).unwrap();
    }
    let key = || SyncPayload::Key(KeyEvent::new(Key::Char('y'), Mods::empty()));
    let frames = [
        (101, "agent", key(), false),
        (102, "agent", SyncPayload::Paste("yes\n".into()), false),
        (103, "shell", key(), false),
    ];
    for (input_id, pane, input, include_agent) in frames {
        let f = ClientFrame::SyncInput {
            input_id,
            pane: pane.into(),
            input,
            include_agent,
        };
        asyncio::write_frame(&mut wr, &f).await.unwrap();
    }
    let ack = frame_until(&mut rd, |f| match f {
        ServerFrame::InputAck {
            input_id: 101,
            status,
        } => Some(status),
        _ => None,
    })
    .await;
    assert_eq!(ack, AckStatus::Rejected);
    // Ordering barrier: a command after the frames above.
    let r = command(&mut rd, &mut wr, 9, "server.status", json!({})).await;
    assert!(r.get("result").is_some());
    assert!(
        drained(rxs.get_mut("agent").unwrap()).is_empty(),
        "the agent received mirrored input"
    );
    assert_eq!(drained(rxs.get_mut("shell").unwrap()), b"y");
    // Explicitly included: delivered.
    let f = ClientFrame::SyncInput {
        input_id: 500,
        pane: "agent".into(),
        input: SyncPayload::Paste("ok".into()),
        include_agent: true,
    };
    asyncio::write_frame(&mut wr, &f).await.unwrap();
    let r = command(&mut rd, &mut wr, 10, "server.status", json!({})).await;
    assert!(r.get("result").is_some());
    assert!(
        String::from_utf8_lossy(&drained(rxs.get_mut("agent").unwrap())).contains("ok"),
        "explicit inclusion delivers"
    );
}
