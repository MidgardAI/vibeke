//! In-process tests of `task.setup_log`, `task.forget`, `task.recreate` refusals, `task.ports`,
//! pane-scope visibility, the PR-merged cleanup hint and `pane.sync_input` (no panes running:
//! the server is built with `/bin/false` as its binary). Port leases are not exercised here
//! (they use the machine-wide state root); `crates/vibeke/tests/it/v1_remainder.rs` covers
//! `task.adopt`, `task.archive`, `task.recreate` and `task.ports.re_lease` in an isolated session.

use super::*;
use crate::ServerOpts;
use crate::api::{Ctx, dispatch};
use crate::paths::Paths;

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
        gateway: None,
    };
    (dir, Server::new(paths, opts).unwrap())
}

fn user() -> Ctx {
    Ctx {
        client_id: "c1".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

fn put(server: &Server, t: &Task) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.task(t.clone());
    server.commit(&mut c, tx).unwrap();
}

fn mk_pane(id: &str, ws: &str) -> Pane {
    Pane {
        id: id.into(),
        handle: format!("{ws}h:{id}"),
        tab: "t1".into(),
        workspace: ws.into(),
        title: None,
        auto_title: String::new(),
        cwd: None,
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
        recovered: None,
        isolation: Default::default(),
        browser: None,
    }
}

fn mk_run(id: &str, pane: &str) -> AgentRun {
    let t = vk_store::now_ms();
    AgentRun {
        id: id.into(),
        handle: format!("h-{id}"),
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
            value: Execution::Idle,
            since_ms: t,
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
        started_at_ms: t,
        ended_at_ms: None,
        capabilities: vec![],
        usage: Default::default(),
        rate_limit: None,
        title: None,
    }
}

fn kind(e: &RpcError) -> &str {
    &e.data.kind
}

fn put_ws_pane(server: &Server, ws: &str, pane: &str) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.ws(Workspace {
        id: ws.into(),
        handle: format!("{ws}h"),
        name: None,
        auto_name: ws.into(),
        root_path: "/tmp".into(),
        task: None,
        order: 0.0,
        branch: None,
    });
    tx.pane(mk_pane(pane, ws));
    server.commit(&mut c, tx).unwrap();
}

fn task(id: &str, wt: &std::path::Path, status: &str) -> Task {
    Task {
        id: id.into(),
        handle: format!("k{id}"),
        title: "t".into(),
        slug: "t".into(),
        workspace: Some("w1".into()),
        repo_root: wt.to_string_lossy().into_owned(),
        worktree_path: Some(wt.to_string_lossy().into_owned()),
        branch: Some("vk/t".into()),
        port_range: Some((21000, 21009)),
        status: status.into(),
        ..Default::default()
    }
}

fn events(server: &Server, ty: &str) -> Vec<Value> {
    server.with_core(|c| {
        c.store
            .events_after(0, 500, &[ty.to_string()])
            .unwrap_or_default()
            .into_iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    })
}

#[tokio::test]
async fn setup_log_tails_the_log_and_respects_pane_scope() {
    let (d, srv) = server();
    let wt = d.path().join("wt");
    std::fs::create_dir_all(wt.join(".vibeke")).unwrap();
    let t = task("a1", &wt, "active");
    put(&srv, &t);
    let none = dispatch(&srv, &user(), "task.setup_log", &json!({"task": "a1"}))
        .await
        .unwrap();
    assert_eq!(none["exists"], false);
    let log: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(wt.join(".vibeke/setup.log"), &log).unwrap();
    let all = dispatch(&srv, &user(), "task.setup_log", &json!({"task": "a1"}))
        .await
        .unwrap();
    assert_eq!(all["truncated"], false);
    assert!(all["text"].as_str().unwrap().ends_with("line 1999\n"));
    let tail = dispatch(
        &srv,
        &user(),
        "task.setup_log",
        &json!({"task": "a1", "max_bytes": 100}),
    )
    .await
    .unwrap();
    assert_eq!(tail["truncated"], true);
    let text = tail["text"].as_str().unwrap();
    assert!(
        text.starts_with("line "),
        "starts at a line boundary: {text:?}"
    );
    assert!(text.len() <= 100);
    // A pane of another workspace does not see the task.
    put_ws_pane(&srv, "w2", "p2");
    let agent = Ctx {
        pane_scope: Some("p2".into()),
        ..user()
    };
    let e = dispatch(&srv, &agent, "task.setup_log", &json!({"task": "a1"}))
        .await
        .unwrap_err();
    assert_eq!(kind(&e), "not_found");
    // Its own workspace's task it does.
    put_ws_pane(&srv, "w1", "p1");
    let own = Ctx {
        pane_scope: Some("p1".into()),
        ..user()
    };
    assert!(
        dispatch(&srv, &own, "task.setup_log", &json!({"task": "a1"}))
            .await
            .is_ok()
    );
    // Lifecycle mutations are refused from a pane.
    for m in [
        "task.archive",
        "task.adopt",
        "task.recreate",
        "task.forget",
        "task.ports.re_lease",
        "pane.sync_input",
        "tab.renumber",
    ] {
        let e = dispatch(&srv, &own, m, &json!({"task": "a1"}))
            .await
            .unwrap_err();
        assert_eq!(kind(&e), "permission_denied", "{m}");
    }
}

#[tokio::test]
async fn forget_and_recreate_follow_the_missing_status() {
    let (d, srv) = server();
    let wt = d.path().join("gone");
    put(&srv, &task("a2", &wt, "active"));
    let e = dispatch(&srv, &user(), "task.forget", &json!({"task": "a2"}))
        .await
        .unwrap_err();
    assert_eq!(kind(&e), "conflict", "an active task needs force");
    let e = dispatch(&srv, &user(), "task.recreate", &json!({"task": "a2"}))
        .await
        .unwrap_err();
    assert_eq!(kind(&e), "conflict", "only missing tasks are recreated");
    put(&srv, &task("a2", &wt, "missing"));
    // Not a repository: recreation fails and nothing is created.
    let e = dispatch(&srv, &user(), "task.recreate", &json!({"task": "a2"}))
        .await
        .unwrap_err();
    assert_eq!(kind(&e), "conflict");
    assert!(!wt.exists());
    let r = dispatch(&srv, &user(), "task.forget", &json!({"task": "a2"}))
        .await
        .unwrap();
    assert_eq!(r["task"]["status"], "forgotten");
    assert_eq!(r["files_touched"], false);
    assert!(srv.with_core(|c| c.task("a2").is_none()), "record closed");
    assert_eq!(events(&srv, "task.forgotten").len(), 1);
}

#[tokio::test]
async fn ports_report_the_lease_env() {
    let (d, srv) = server();
    put(&srv, &task("a3", &d.path().join("x"), "active"));
    let r = dispatch(&srv, &user(), "task.ports", &json!({"task": "a3"}))
        .await
        .unwrap();
    assert_eq!(
        r["lease"],
        json!({"start": 21000, "end": 21009, "count": 10})
    );
    assert_eq!(r["env"]["VIBEKE_PORT_BASE"], "21000");
    assert_eq!(r["env"]["VIBEKE_TASK_SLUG"], "t");
}

#[tokio::test]
async fn merged_pr_suggests_cleanup_once() {
    let (d, srv) = server();
    let t = task("a4", &d.path().join("x"), "active");
    put(&srv, &t);
    let pr = |state: &str| vk_tasks::PrLookup::Pr {
        pr: vk_tasks::PrStatus {
            number: 7,
            state: state.into(),
            is_draft: false,
            review_decision: None,
            checks: vk_tasks::ChecksState::None,
            url: "https://example.invalid/pr/7".into(),
            label: String::new(),
        },
    };
    note_pr(&srv, &t, &pr("OPEN"));
    assert!(events(&srv, "task.cleanup_suggested").is_empty());
    note_pr(&srv, &t, &pr("MERGED"));
    note_pr(&srv, &t, &pr("MERGED"));
    let ev = events(&srv, "task.cleanup_suggested");
    assert_eq!(ev.len(), 1, "once per PR");
    assert_eq!(ev[0]["data"]["reason"], "pr_merged");
    assert!(
        ev[0]["data"]["hint"]
            .as_str()
            .unwrap()
            .contains("--remove-worktree")
    );
}

#[tokio::test]
async fn sync_input_needs_two_eligible_panes_and_excludes_agents() {
    let (_d, srv) = server();
    put_ws_pane(&srv, "w1", "p1");
    // One pane only: refused.
    let e = dispatch(
        &srv,
        &user(),
        "pane.sync_input",
        &json!({"action": "start", "panes": ["p1"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "conflict");
    {
        let mut c = srv.core.lock().unwrap();
        let mut tx = Tx::new();
        for p in ["p2", "p3"] {
            tx.pane(mk_pane(p, "w1"));
        }
        // p3 runs an agent.
        tx.run(mk_run("r3", "p3"));
        srv.commit(&mut c, tx).unwrap();
    }
    let g = dispatch(
        &srv,
        &user(),
        "pane.sync_input",
        &json!({"action": "start", "panes": "p1,p2,p3"}),
    )
    .await
    .unwrap();
    assert_eq!(g["group"]["panes"], json!(["p1", "p2"]));
    assert_eq!(g["excluded"][0]["reason"], "agent");
    let gid = g["group_id"].as_str().unwrap().to_string();
    let st = dispatch(&srv, &user(), "pane.sync_input", &json!({"pane": "p2"}))
        .await
        .unwrap();
    assert_eq!(st["group_id"], gid.as_str());
    assert_eq!(
        crate::sync_input::segment(&srv, Some("p1"))["enabled"],
        true
    );
    // include_agents adds the agent pane explicitly.
    let g2 = dispatch(
        &srv,
        &user(),
        "pane.sync_input",
        &json!({"enabled": true, "panes": ["p1", "p3"], "include_agents": true}),
    )
    .await
    .unwrap();
    assert_eq!(g2["group"]["agents"], json!(["p3"]));
    // p1 moved to the new group; the old one (p2 alone) ended.
    let st = dispatch(
        &srv,
        &user(),
        "pane.sync_input",
        &json!({"action": "status"}),
    )
    .await
    .unwrap();
    assert_eq!(st["groups"].as_array().unwrap().len(), 1);
    let stop = dispatch(
        &srv,
        &user(),
        "pane.sync_input",
        &json!({"enabled": false, "group": g2["group_id"]}),
    )
    .await
    .unwrap();
    assert_eq!(stop["stopped"].as_array().unwrap().len(), 1);
    assert_eq!(
        crate::sync_input::segment(&srv, Some("p1"))["enabled"],
        false
    );
    let ev = events(&srv, "pane.sync_input_changed");
    assert_eq!(ev.len(), 4, "started, superseded, started, stopped: {ev:?}");
}
