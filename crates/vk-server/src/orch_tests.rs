//! In-process tests of the Batch 4 orchestration API (`orch*.rs`): gating, pane scope, claims,
//! conflict prediction, the merge queue (real git repositories in temp dirs), learned policy,
//! quota scheduling, the goal planner and the VM API on the fake backend. The server is built
//! with `/bin/false` as its binary, so no holder, harness or VM is ever started. Flows that need
//! `task.create` (best-of-N, split, goal fan-out, `--isolate vm`) run end to end in
//! `crates/vibeke/tests/orchestrate.rs`.

use super::*;
use crate::ServerOpts;
use crate::api::{PaneScope, dispatch, pane_scope_of};
use crate::orch_vm::set_backend;
use crate::paths::Paths;
use std::path::{Path, PathBuf};
use std::process::Command;
use vk_orchestrate::config::OrchestrateConfig;
use vk_proto::model::*;
use vk_proto::rpc::RpcError;

fn init_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-orch-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
            std::env::set_var("VIBEKE_NO_OPEN", "1");
            std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
        }
    });
}

fn server() -> (tempfile::TempDir, Arc<Server>) {
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
        machine: "m".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![],
        shims: false,
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

fn kind(e: &RpcError) -> &str {
    &e.data.kind
}

fn enable_all(srv: &Server) {
    let mut c = OrchestrateConfig::default();
    c.best_of_n.enabled = true;
    c.split.enabled = true;
    c.learned_policy.enabled = true;
    c.learned_policy.min_approvals = 3;
    c.merge.enabled = true;
    c.planner.enabled = true;
    c.quota.enabled = true;
    set_cfg(srv, c);
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn repo(root: &Path) -> PathBuf {
    let r = root.join("repo");
    std::fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q", "-b", "main"]);
    git(&r, &["config", "user.name", "T"]);
    git(&r, &["config", "user.email", "t@example.invalid"]);
    git(&r, &["config", "commit.gpgsign", "false"]);
    for (f, b) in [
        ("a.txt", "a1\na2\na3\n"),
        ("b.txt", "b1\n"),
        ("c.txt", "c1\n"),
    ] {
        std::fs::write(r.join(f), b).unwrap();
    }
    git(&r, &["add", "-A"]);
    git(&r, &["commit", "-q", "-m", "base"]);
    r.canonicalize().unwrap()
}

fn worktree(repo: &Path, branch: &str) -> PathBuf {
    let p = repo
        .parent()
        .unwrap()
        .join(format!("wt-{}", branch.replace('/', "_")));
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            &p.to_string_lossy(),
            "main",
        ],
    );
    p.canonicalize().unwrap()
}

fn put_task(
    srv: &Server,
    id: &str,
    handle: &str,
    repo: &Path,
    wt: &Path,
    branch: &str,
    ws: Option<&str>,
) {
    let t = Task {
        id: id.into(),
        handle: handle.into(),
        title: id.into(),
        slug: id.into(),
        workspace: ws.map(str::to_string),
        repo_root: repo.to_string_lossy().into_owned(),
        worktree_path: Some(wt.to_string_lossy().into_owned()),
        branch: Some(branch.into()),
        base_ref: Some("main".into()),
        status: "active".into(),
        checkout: Some("worktree".into()),
        ..Default::default()
    };
    let mut c = srv.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.task(t);
    srv.commit(&mut c, tx).unwrap();
}

fn events(srv: &Server, ty: &str) -> Vec<Value> {
    srv.with_core(|c| {
        c.store
            .events_after(0, 1000, &[ty.to_string()])
            .unwrap_or_default()
            .into_iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    })
}

async fn ok(srv: &Arc<Server>, m: &str, p: Value) -> Value {
    dispatch(srv, &user(), m, &p)
        .await
        .unwrap_or_else(|e| panic!("{m} {p}: {e:?}"))
}

async fn fail(srv: &Arc<Server>, m: &str, p: Value) -> RpcError {
    dispatch(srv, &user(), m, &p).await.expect_err(m)
}

// ---- gating and scope -------------------------------------------------------------------------

#[tokio::test]
async fn every_feature_is_off_by_default_and_names_its_flag() {
    let (_d, srv) = server();
    set_cfg(&srv, OrchestrateConfig::default());
    for (m, p, flag) in [
        (
            "task.best_of_n",
            json!({"title": "t", "agents": "claude:2"}),
            "orchestrate.best_of_n.enabled",
        ),
        (
            "task.create",
            json!({"title": "t", "agents": "claude:2"}),
            "orchestrate.best_of_n.enabled",
        ),
        (
            "task.pick",
            json!({"family": "k1", "child": "k1.1"}),
            "orchestrate.best_of_n.enabled",
        ),
        (
            "family.check",
            json!({"family": "k1"}),
            "orchestrate.best_of_n.enabled",
        ),
        ("task.split", json!({}), "orchestrate.split.enabled"),
        (
            "policy.learned.list",
            json!({}),
            "orchestrate.learned_policy.enabled",
        ),
        (
            "policy.learned.accept",
            json!({"id": "x"}),
            "orchestrate.learned_policy.enabled",
        ),
        (
            "policy.learned.dismiss",
            json!({"id": "x"}),
            "orchestrate.learned_policy.enabled",
        ),
        ("merge.predict", json!({}), "orchestrate.merge.enabled"),
        ("merge.queue.list", json!({}), "orchestrate.merge.enabled"),
        ("merge.queue.run", json!({}), "orchestrate.merge.enabled"),
        (
            "goal.create",
            json!({"title": "t"}),
            "orchestrate.planner.enabled",
        ),
        ("goal.list", json!({}), "orchestrate.planner.enabled"),
        ("goal.briefing", json!({}), "orchestrate.planner.enabled"),
        ("quota.tick", json!({}), "orchestrate.quota.enabled"),
    ] {
        let e = fail(&srv, m, p).await;
        assert_eq!(kind(&e), "unsupported", "{m}: {e:?}");
        let flag_in_details = e.data.details["flag"].as_str().unwrap_or("");
        assert_eq!(flag_in_details, flag, "{m}");
    }
    let st = ok(&srv, "vm.status", json!({})).await;
    assert_eq!(st["enabled"], false);
    let e = fail(&srv, "vm.create", json!({})).await;
    assert_eq!(kind(&e), "unsupported");
    assert!(e.message.contains("isolation.vm"));
    // Reads that need no flag.
    assert_eq!(
        ok(&srv, "family.list", json!({})).await["families"],
        json!([])
    );
    assert_eq!(
        ok(&srv, "task.claim.list", json!({})).await["claims"],
        json!([])
    );
    assert_eq!(ok(&srv, "quota.status", json!({})).await["enabled"], false);
}

#[test]
fn pane_scope_is_forbidden_for_mutations_and_open_for_reads() {
    for m in PANE_FORBIDDEN {
        assert_eq!(pane_scope_of(m), PaneScope::Forbidden, "{m}");
        assert!(METHODS.iter().any(|(n, _)| n == m), "{m} is not in METHODS");
    }
    for (m, mutating) in METHODS {
        let forbidden = PANE_FORBIDDEN.contains(m);
        if *mutating && !forbidden {
            assert!(
                matches!(*m, "task.claim" | "task.claim.remove"),
                "{m} mutates but is open to panes"
            );
        }
    }
    for m in [
        "family.list",
        "task.compare",
        "merge.predict",
        "goal.get",
        "quota.status",
        "vm.status",
        "task.claim",
    ] {
        assert_eq!(pane_scope_of(m), PaneScope::Open, "{m}");
    }
}

fn put_pane_in_workspace(srv: &Server, ws: &str, pane: &str, task: Option<&str>) {
    let mut c = srv.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.ws(Workspace {
        id: ws.into(),
        handle: format!("{ws}h"),
        name: None,
        auto_name: ws.into(),
        root_path: "/tmp".into(),
        task: task.map(str::to_string),
        order: 0.0,
        branch: None,
    });
    tx.pane(Pane {
        id: pane.into(),
        handle: format!("{ws}h:{pane}"),
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
    });
    srv.commit(&mut c, tx).unwrap();
}

// ---- claims, prediction and the merge queue ----------------------------------------------------

/// Final review P2: entries added and cancelled while a merge runs survive the run's save (the
/// outcome is merged into the current queue instead of overwriting it with a stale snapshot).
#[tokio::test(flavor = "multi_thread")]
async fn queue_changes_during_a_merge_are_kept() {
    let (d, srv) = server();
    enable_all(&srv);
    let mut c = cfg(&srv);
    c.merge.queue_check = "sleep 1.5".into();
    set_cfg(&srv, c);
    let r = repo(d.path());
    let w1 = worktree(&r, "t/one");
    let w2 = worktree(&r, "t/two");
    let w3 = worktree(&r, "t/three");
    put_task(&srv, "ta", "k1", &r, &w1, "t/one", None);
    put_task(&srv, "tb", "k2", &r, &w2, "t/two", None);
    put_task(&srv, "tc", "k3", &r, &w3, "t/three", None);
    for (w, f) in [(&w1, "one.txt"), (&w2, "two.txt"), (&w3, "three.txt")] {
        std::fs::write(w.join(f), "x\n").unwrap();
        git(w, &["add", "-A"]);
        git(w, &["commit", "-q", "-m", f]);
    }
    ok(&srv, "merge.queue.add", json!({"task": "k1"})).await;
    ok(&srv, "merge.queue.add", json!({"task": "k2"})).await;
    let s2 = srv.clone();
    let run = tokio::spawn(async move { ok(&s2, "merge.queue.run", json!({})).await });
    // While k1's merge (and its 1.5 s check) runs: k3 is added, k2 cancelled.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    ok(&srv, "merge.queue.add", json!({"task": "k3"})).await;
    ok(&srv, "merge.queue.cancel", json!({"task": "k2"})).await;
    let run = run.await.unwrap();
    assert_eq!(run["results"][0]["event"], "merge.merged", "{run}");
    let all = ok(&srv, "merge.queue.list", json!({"all": true})).await;
    let state = |task: &str| {
        all["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["task"] == task)
            .map(|e| e["state"].as_str().unwrap().to_string())
    };
    assert_eq!(state("ta").as_deref(), Some("merged"), "{all}");
    assert_eq!(state("tb").as_deref(), Some("cancelled"), "{all}");
    assert_eq!(state("tc").as_deref(), Some("queued"), "{all}");
}

#[tokio::test]
async fn claims_prediction_and_the_queue_work_on_real_worktrees() {
    let (d, srv) = server();
    enable_all(&srv);
    let r = repo(d.path());
    let w1 = worktree(&r, "t/one");
    let w2 = worktree(&r, "t/two");
    put_task(&srv, "ta", "k1", &r, &w1, "t/one", Some("ws1"));
    put_task(&srv, "tb", "k2", &r, &w2, "t/two", Some("ws2"));
    put_pane_in_workspace(&srv, "ws1", "p1", Some("ta"));
    put_pane_in_workspace(&srv, "ws2", "p2", Some("tb"));
    // k1 claims b.txt; k2 then edits it.
    let c = ok(
        &srv,
        "task.claim",
        json!({"task": "k1", "glob": "b.txt", "note": "mine"}),
    )
    .await;
    let claim_id = c["claim"]["id"].as_str().unwrap().to_string();
    assert_eq!(c["conflicts"], json!([]));
    std::fs::write(w2.join("b.txt"), "b1\nfrom two\n").unwrap();
    // k1 and k2 both change a.txt in conflicting ways (committed).
    std::fs::write(w1.join("a.txt"), "a1\nONE\na3\n").unwrap();
    git(&w1, &["add", "-A"]);
    git(&w1, &["commit", "-q", "-m", "one"]);
    std::fs::write(w2.join("a.txt"), "a1\nTWO\na3\n").unwrap();
    git(&w2, &["add", "-A"]);
    git(&w2, &["commit", "-q", "-m", "two"]);
    let p = ok(&srv, "merge.predict", json!({})).await;
    let cs = p["conflicts"].as_array().unwrap();
    assert!(
        cs.iter().any(|c| c["kind"] == "claim"
            && c["a"] == "k2"
            && c["b"] == "k1"
            && c["paths"] == json!(["b.txt"])),
        "{cs:?}"
    );
    assert!(
        cs.iter()
            .any(|c| c["kind"] == "overlap" || c["kind"] == "textual"),
        "{cs:?}"
    );
    assert_eq!(p["tasks"].as_array().unwrap().len(), 2);
    // Adding the claim told us who already violates it.
    let c2 = ok(&srv, "task.claim", json!({"task": "k1", "glob": "a.txt"})).await;
    assert!(!c2["conflicts"].as_array().unwrap().is_empty());
    assert_eq!(
        ok(&srv, "task.claim.list", json!({"task": "k1"})).await["claims"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // A pane may claim for its own task only.
    let own = Ctx {
        pane_scope: Some("p1".into()),
        ..user()
    };
    dispatch(
        &srv,
        &own,
        "task.claim",
        &json!({"task": "k1", "glob": "src/**"}),
    )
    .await
    .unwrap();
    let other = Ctx {
        pane_scope: Some("p2".into()),
        ..user()
    };
    let e = dispatch(
        &srv,
        &other,
        "task.claim",
        &json!({"task": "k1", "glob": "src/**"}),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "permission_denied");
    let e = dispatch(
        &srv,
        &other,
        "task.claim.remove",
        &json!({"claim": claim_id}),
    )
    .await
    .unwrap_err();
    assert_eq!(kind(&e), "permission_denied");
    assert!(
        matches!(fail(&srv, "task.claim", json!({"task": "k1", "glob": "../x"})).await, e if kind(&e) == "invalid_params")
    );
    ok(&srv, "task.claim.remove", json!({"claim": claim_id})).await;
    assert_eq!(
        kind(&fail(&srv, "task.claim.remove", json!({"claim": claim_id})).await),
        "not_found"
    );
    assert_eq!(events(&srv, "task.claim_added").len(), 3);
    assert_eq!(events(&srv, "task.claim_removed").len(), 1);
    // Background pass announces medium-and-up conflicts once.
    crate::orch_merge::tick(&srv, &cfg(&srv)).await;
    let first = events(&srv, "merge.conflict_predicted").len();
    assert!(first >= 1);
    crate::orch_merge::tick(&srv, &cfg(&srv)).await;
    assert_eq!(
        events(&srv, "merge.conflict_predicted").len(),
        first,
        "no repeat while unchanged"
    );

    // The queue: k1 first (its branch is clean and committed), then k2 conflicts with it.
    let q1 = ok(
        &srv,
        "merge.queue.add",
        json!({"task": "k1", "priority": 1}),
    )
    .await;
    assert_eq!(q1["position"], 1);
    // k2's b.txt edit was committed with "two"; an uncommitted change on top of it would not
    // be merged, so the queue refuses it unless allowed.
    std::fs::write(w2.join("b.txt"), "b1\n").unwrap();
    let dirty = fail(&srv, "merge.queue.add", json!({"task": "k2"})).await;
    assert_eq!(kind(&dirty), "conflict");
    assert_eq!(dirty.data.details["reason"], "dirty_worktree");
    let q2 = ok(
        &srv,
        "merge.queue.add",
        json!({"task": "k2", "allow_dirty": true}),
    )
    .await;
    assert_eq!(q2["position"], 2);
    assert_eq!(
        kind(&fail(&srv, "merge.queue.add", json!({"task": "k2"})).await),
        "conflict",
        "already queued"
    );
    let listed = ok(&srv, "merge.queue.list", json!({})).await;
    assert_eq!(listed["order"].as_array().unwrap().len(), 2);
    let run = ok(&srv, "merge.queue.run", json!({"all": true})).await;
    assert_eq!(run["results"][0]["event"], "merge.merged", "{run}");
    assert_eq!(run["results"][1]["event"], "merge.conflict");
    assert_eq!(run["ran"], 2);
    assert!(git(&r, &["log", "--format=%s", "main"]).contains("Merge branch 't/one'"));
    assert!(
        std::fs::read_to_string(r.join("a.txt"))
            .unwrap()
            .contains("ONE")
    );
    let e2 = events(&srv, "merge.conflict");
    assert_eq!(e2[0]["data"]["paths"], json!(["a.txt"]));
    // Requeue and cancel.
    let entry = run["results"][1]["entry"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    ok(&srv, "merge.queue.requeue", json!({"entry": entry})).await;
    ok(&srv, "merge.queue.cancel", json!({"entry": entry})).await;
    assert_eq!(
        kind(&fail(&srv, "merge.queue.cancel", json!({"entry": entry})).await),
        "invalid_params"
    );
    let all = ok(&srv, "merge.queue.list", json!({"all": true})).await;
    assert_eq!(all["entries"].as_array().unwrap().len(), 2);
    assert_eq!(events(&srv, "merge.merged").len(), 1);
    assert_eq!(events(&srv, "merge.queued").len(), 2);
}

// ---- learned policy ----------------------------------------------------------------------------

fn approval(
    id: &str,
    run: &str,
    pane: &str,
    command: &str,
    decision: Decision,
    risk: Risk,
    at: i64,
) -> Interaction {
    Interaction {
        id: id.into(),
        handle: format!("h{id}"),
        run: run.into(),
        pane: pane.into(),
        kind: InteractionKind::Approval,
        status: InteractionStatus::Answered,
        title: "run".into(),
        body_md: None,
        action: Some(ActionInfo {
            tool: "Bash".into(),
            summary: command.into(),
            command: Some(command.into()),
            paths: vec![],
            diff: None,
            risk,
            risk_reasons: vec![],
        }),
        questions: vec![],
        plan_md: None,
        answer_channel: AnswerChannel::Native,
        native_ref: None,
        source: StateSource::Structured,
        confidence: 1.0,
        answerable: true,
        gate: true,
        decision_rev: 1,
        delivery: DeliveryState::Delivered,
        delivery_error: None,
        answer: Some(Answer {
            decision: Some(decision),
            choices: vec![],
            text: None,
        }),
        answered_by: Some("user".into()),
        answer_key: None,
        opened_at_ms: at,
        answered_at_ms: Some(at),
    }
}

fn seed_run(srv: &Server, id: &str, pane: &str, harness: &str, cwd: &str) {
    let t = vk_store::now_ms();
    let r = AgentRun {
        id: id.into(),
        handle: format!("h-{id}"),
        name: None,
        pane: pane.into(),
        harness: harness.into(),
        harness_version: None,
        integration: "hooks".into(),
        harness_session_id: None,
        transcript_path: None,
        resume_argv: vec![],
        cwd: Some(cwd.into()),
        model: None,
        task: None,
        execution: Facet {
            value: Execution::Working,
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
        turns_completed: 1,
        done_rev: 0,
        started_at_ms: t,
        ended_at_ms: None,
        capabilities: vec![],
        usage: Default::default(),
        rate_limit: None,
    };
    let mut c = srv.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.run(r);
    srv.commit(&mut c, tx).unwrap();
}

/// Run `body` on its own runtime thread and fail (instead of hanging) if it does not finish in
/// `limit`: a core-lock re-entry deadlocks a std mutex, which no async timeout can interrupt.
fn with_deadline<F: std::future::Future<Output = ()> + Send + 'static>(
    limit: std::time::Duration,
    body: F,
) {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let (done, rx) = channel::<()>();
    let h = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(body);
        let _ = done.send(());
    });
    match rx.recv_timeout(limit) {
        Ok(()) => h.join().unwrap(),
        Err(RecvTimeoutError::Disconnected) => {
            if let Err(p) = h.join() {
                std::panic::resume_unwind(p);
            }
        }
        Err(RecvTimeoutError::Timeout) => panic!("did not finish within {limit:?} (deadlock?)"),
    }
}

#[test]
fn learned_policy_suggests_accepts_and_dismisses() {
    with_deadline(
        std::time::Duration::from_secs(60),
        learned_policy_suggests_accepts_and_dismisses_body(),
    );
}

async fn learned_policy_suggests_accepts_and_dismisses_body() {
    let (d, srv) = server();
    enable_all(&srv);
    let ws = d.path().join("app");
    std::fs::create_dir_all(&ws).unwrap();
    let wsp = ws.to_string_lossy().into_owned();
    seed_run(&srv, "r1", "p1", "claude", &wsp);
    let now = vk_store::now_ms();
    {
        let mut c = srv.core.lock().unwrap();
        let mut tx = Tx::new();
        for i in 0..4 {
            tx.interaction(approval(
                &format!("i{i}"),
                "r1",
                "p1",
                "npm test --watch",
                Decision::Allow,
                Risk::Low,
                now - 1000 + i,
            ));
        }
        for i in 0..4 {
            tx.interaction(approval(
                &format!("d{i}"),
                "r1",
                "p1",
                "rm -rf build",
                Decision::Allow,
                Risk::High,
                now - 900 + i,
            ));
        }
        for i in 0..4 {
            tx.interaction(approval(
                &format!("m{i}"),
                "r1",
                "p1",
                "cargo build",
                Decision::Allow,
                Risk::Low,
                now - 800 + i,
            ));
        }
        tx.interaction(approval(
            "mdeny",
            "r1",
            "p1",
            "cargo build",
            Decision::Deny,
            Risk::Low,
            now - 700,
        ));
        srv.commit(&mut c, tx).unwrap();
    }
    let l = ok(&srv, "policy.learned.list", json!({})).await;
    let sug = l["suggestions"].as_array().unwrap();
    assert_eq!(sug.len(), 1, "{l}");
    assert_eq!(sug[0]["effect"], "allow");
    assert!(sug[0]["pattern"].as_str().unwrap().contains("npm test"));
    assert_eq!(sug[0]["approvals"], 4);
    assert!(
        sug[0]["toml"]
            .as_str()
            .unwrap()
            .starts_with("[[policy.rule]]")
    );
    assert_eq!(l["stats"]["decisions"], 13);
    let id = sug[0]["id"].as_str().unwrap().to_string();
    // Accepting writes a policy rule through policy.add...
    let acc = ok(&srv, "policy.learned.accept", json!({"id": id})).await;
    assert_eq!(acc["result"]["target"], "user");
    let rules = ok(&srv, "policy.list", json!({})).await;
    assert!(
        rules["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["match"]["tool"] == "Bash" && r["effect"] == "allow"),
        "{rules}"
    );
    // ...and the action is now decided, so it is no longer suggested.
    assert!(
        ok(&srv, "policy.learned.list", json!({})).await["suggestions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        kind(&fail(&srv, "policy.learned.accept", json!({"id": id})).await),
        "not_found"
    );
    // Dismissal hides a suggestion for good; the repo target writes a reviewable file.
    {
        let mut c = srv.core.lock().unwrap();
        let mut tx = Tx::new();
        for i in 0..4 {
            tx.interaction(approval(
                &format!("j{i}"),
                "r1",
                "p1",
                "pnpm lint",
                Decision::Allow,
                Risk::Low,
                now - 500 + i,
            ));
            tx.interaction(approval(
                &format!("k{i}"),
                "r1",
                "p1",
                "make build",
                Decision::Allow,
                Risk::Low,
                now - 400 + i,
            ));
        }
        srv.commit(&mut c, tx).unwrap();
    }
    let l = ok(&srv, "policy.learned.list", json!({"repo": wsp})).await;
    let sug = l["suggestions"].as_array().unwrap().clone();
    assert_eq!(sug.len(), 2, "{l}");
    let lint = sug
        .iter()
        .find(|s| s["pattern"].as_str().unwrap().contains("pnpm lint"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let make = sug
        .iter()
        .find(|s| s["pattern"].as_str().unwrap().contains("make build"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    ok(&srv, "policy.learned.dismiss", json!({"id": lint})).await;
    let r = ok(
        &srv,
        "policy.learned.accept",
        json!({"id": make, "target": "repo"}),
    )
    .await;
    assert_eq!(r["result"]["target"], "repo");
    let file = ws.join(".vibeke/policy.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("[[rule]]") && text.contains("make build"),
        "{text}"
    );
    let l = ok(&srv, "policy.learned.list", json!({})).await;
    assert!(
        l["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != json!(lint))
    );
    assert_eq!(l["stats"]["dismissed"], 1);
    assert_eq!(
        kind(
            &fail(
                &srv,
                "policy.learned.accept",
                json!({"id": "x", "target": "nowhere"})
            )
            .await
        ),
        "not_found"
    );
    assert_eq!(events(&srv, "policy.learned_accepted").len(), 2);
    assert_eq!(events(&srv, "policy.learned_dismissed").len(), 1);
}

// ---- quota -----------------------------------------------------------------------------------

#[tokio::test]
async fn quota_pauses_low_priority_runs_near_a_limit_and_resumes_them() {
    let (_d, srv) = server();
    enable_all(&srv);
    seed_run(&srv, "r1", "p1", "claude", "/tmp");
    seed_run(&srv, "r2", "p2", "codex", "/tmp");
    let now = vk_store::now_ms();
    {
        let mut c = srv.core.lock().unwrap();
        let mut tx = Tx::new();
        for (id, used) in [("r1", 93.0f32), ("r2", 20.0)] {
            let mut r = c.run(id).cloned().unwrap();
            r.rate_limit = Some(RateLimitInfo {
                limited: false,
                resets_at_ms: Some(now + 3_600_000),
                scope: Some("5h".into()),
                used_percent: Some(used),
                message: None,
                observed_at_ms: now,
            });
            tx.run(r);
        }
        srv.commit(&mut c, tx).unwrap();
    }
    let st = ok(&srv, "quota.status", json!({})).await;
    assert_eq!(st["accounts"].as_array().unwrap().len(), 2);
    let route = ok(
        &srv,
        "quota.route",
        json!({"harnesses": ["claude", "codex", "pi"]}),
    )
    .await;
    assert_eq!(route["ranking"][0]["harness"], "codex");
    let dry = ok(&srv, "quota.tick", json!({"dry_run": true})).await;
    let actions = dry["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 1, "{dry}");
    assert_eq!(actions[0]["action"], "pause");
    assert_eq!(actions[0]["runs"], json!(["r1"]));
    assert!(
        ok(&srv, "quota.status", json!({})).await["paused"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a dry run changes nothing"
    );
    let real = ok(&srv, "quota.tick", json!({})).await;
    assert_eq!(real["actions"].as_array().unwrap().len(), 1);
    assert_eq!(
        ok(&srv, "quota.status", json!({})).await["paused"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(events(&srv, "quota.paused").len(), 1);
    // The paused run is not paused twice.
    assert!(
        ok(&srv, "quota.tick", json!({"dry_run": true})).await["actions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // By hand.
    assert_eq!(
        kind(&fail(&srv, "quota.resume", json!({"key": "nope"})).await),
        "not_found"
    );
    let r = ok(&srv, "quota.resume", json!({"key": "r1"})).await;
    assert_eq!(r["resumed"], "r1");
    assert_eq!(events(&srv, "quota.resumed").len(), 1);
    assert!(
        ok(&srv, "quota.status", json!({})).await["paused"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

// ---- goals -------------------------------------------------------------------------------------

#[tokio::test]
async fn goals_plan_approve_gate_and_cancel() {
    let (d, srv) = server();
    enable_all(&srv);
    let r = repo(d.path());
    let g = ok(&srv, "goal.create", json!({"title": "Ship search", "repo": r, "text": "1. add the index in `src/search/`\n2. write tests for it"})).await;
    let id = g["goal"]["id"].as_str().unwrap().to_string();
    assert_eq!(g["goal"]["handle"], "G1");
    assert_eq!(g["goal"]["state"], "planned");
    assert_eq!(g["goal"]["plan"]["steps"].as_array().unwrap().len(), 2);
    assert_eq!(g["goal"]["plan"]["steps"][1]["depends_on"], json!(["s1"]));
    assert_eq!(g["progress"], json!({"done": 0, "total": 2}));
    // Starting before approval is refused.
    let e = fail(&srv, "goal.start", json!({"goal": "G1"})).await;
    assert_eq!(e.data.details["reason"], "not_approved");
    // Approving without starting; then editing the plan clears the approval.
    let a = ok(
        &srv,
        "goal.approve",
        json!({"goal": id, "by": "me", "start": false}),
    )
    .await;
    assert_eq!(a["goal"]["state"], "approved");
    assert_eq!(a["goal"]["approved_by"], "me");
    let edited = ok(
        &srv,
        "goal.plan_submit",
        json!({"goal": "G1", "plan": {"steps": [{"id": "only", "title": "Everything", "prompt": "do it all"}]}}),
    )
    .await;
    assert_eq!(edited["goal"]["state"], "planned");
    assert!(edited["goal"]["approved_rev"].is_null());
    assert_eq!(edited["goal"]["plan_rev"], 2);
    assert_eq!(
        kind(
            &fail(
                &srv,
                "goal.plan_submit",
                json!({"goal": "G1", "plan": "{\"steps\": []}"})
            )
            .await
        ),
        "invalid_params"
    );
    assert_eq!(
        kind(&fail(&srv, "goal.plan_submit", json!({"goal": "G1"})).await),
        "invalid_params"
    );
    // A step that never started cannot finish.
    assert_eq!(
        kind(
            &fail(
                &srv,
                "goal.step_done",
                json!({"goal": "G1", "step": "only"})
            )
            .await
        ),
        "invalid_params"
    );
    // external planning hands back the prompt.
    let ext = ok(
        &srv,
        "goal.plan",
        json!({"goal": "G1", "backend": "external"}),
    )
    .await;
    assert!(
        ext["prompt"]
            .as_str()
            .unwrap()
            .contains("Do not edit any file")
    );
    assert!(
        ext["submit_command"]
            .as_str()
            .unwrap()
            .contains("goal plan-submit G1")
    );
    assert_eq!(
        kind(
            &fail(
                &srv,
                "goal.plan",
                json!({"goal": "G1", "backend": "oracle"})
            )
            .await
        ),
        "invalid_params"
    );
    // list/get, then cancel closes it.
    assert_eq!(
        ok(&srv, "goal.list", json!({})).await["goals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        ok(&srv, "goal.get", json!({"goal": "G1"})).await["goal"]["title"],
        "Ship search"
    );
    let c = ok(&srv, "goal.cancel", json!({"goal": "G1"})).await;
    assert_eq!(c["goal"]["state"], "cancelled");
    assert_eq!(
        kind(&fail(&srv, "goal.cancel", json!({"goal": "G1"})).await),
        "conflict"
    );
    assert_eq!(
        ok(&srv, "goal.list", json!({})).await["goals"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "closed goals stay listed"
    );
    assert_eq!(
        kind(&fail(&srv, "goal.get", json!({"goal": "G9"})).await),
        "not_found"
    );
    // A second goal gets the next handle.
    let g2 = ok(
        &srv,
        "goal.create",
        json!({"title": "Other", "repo": r, "plan": false}),
    )
    .await;
    assert_eq!(g2["goal"]["handle"], "G2");
    assert_eq!(g2["goal"]["state"], "draft");
    // The briefing covers what the log recorded.
    let b = ok(&srv, "goal.briefing", json!({"since": "1h"})).await;
    let text = b["briefing"]["text"].as_str().unwrap();
    assert!(text.contains("Goals"), "{text}");
    assert!(text.contains("planned"), "{text}");
    assert!(b["briefing"]["counts"]["goal.created"].as_u64().unwrap() >= 2);
    assert!(events(&srv, "goal.approved").len() == 1 && events(&srv, "goal.cancelled").len() == 1);
}

// ---- vm ----------------------------------------------------------------------------------------

fn vm_server() -> (tempfile::TempDir, Arc<Server>, PathBuf) {
    let (d, srv) = server();
    let mut ic = vk_sandbox::config::IsolationConfig::default();
    ic.vm.enabled = true;
    ic.vm.provider = "fake".into();
    ic.vm.setup = vec!["echo toolchain > toolchain.txt".into()];
    crate::sandbox::extras::set_cfg(&srv, ic);
    let root = d.path().join("fakevm");
    set_backend(&srv, crate::orch_vm::fake_backend(&root));
    (d, srv, root)
}

#[tokio::test]
async fn vm_api_runs_templates_snapshots_and_forks_on_the_fake_backend() {
    let (d, srv, root) = vm_server();
    let st = ok(&srv, "vm.status", json!({})).await;
    assert_eq!(st["enabled"], true);
    assert_eq!(st["provider"], "fake");
    assert_eq!(st["available"], true);
    assert!(st["providers"].as_array().unwrap().len() >= 5);
    // A template is built once (setup runs in the builder) and the VM forks from it.
    let tpl = ok(&srv, "vm.template.build", json!({})).await;
    let key = tpl["template"]["key"].as_str().unwrap().to_string();
    assert_eq!(
        ok(&srv, "vm.template.list", json!({})).await["templates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let checkout = d.path().join("co");
    std::fs::create_dir_all(&checkout).unwrap();
    let c = ok(
        &srv,
        "vm.create",
        json!({"name": "vk-one", "task": "t1", "checkout": checkout}),
    )
    .await;
    assert_eq!(c["vm"]["state"], "running");
    assert!(c["fork_ms"].is_number());
    assert_eq!(
        std::fs::read_to_string(root.join("vms/vk-one/disk/toolchain.txt")).unwrap(),
        "toolchain\n"
    );
    assert!(root.join("vms/vk-one/disk/workspace").exists());
    let list = ok(&srv, "vm.list", json!({})).await;
    let vm = list["vms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "vk-one")
        .unwrap();
    assert_eq!(vm["state"], "running");
    assert_eq!(vm["template"], key);
    // Lifecycle.
    assert_eq!(
        ok(&srv, "vm.suspend", json!({"vm": "vk-one"})).await["state"],
        "suspended"
    );
    assert_eq!(
        ok(&srv, "vm.resume", json!({"vm": "vk-one"})).await["state"],
        "running"
    );
    assert_eq!(
        ok(&srv, "vm.stop", json!({"vm": "vk-one"})).await["state"],
        "stopped"
    );
    assert_eq!(
        ok(&srv, "vm.start", json!({"vm": "vk-one"})).await["state"],
        "running"
    );
    assert_eq!(
        kind(&fail(&srv, "vm.start", json!({"vm": "vk-one"})).await),
        "conflict"
    );
    assert_eq!(
        kind(&fail(&srv, "vm.start", json!({"vm": "nope"})).await),
        "not_found"
    );
    // Snapshot, then a best-of-N style fork of three.
    let s = ok(
        &srv,
        "vm.snapshot",
        json!({"vm": "vk-one", "label": "base"}),
    )
    .await;
    assert_eq!(s["snapshot"]["id"], "vk-one/base");
    let f = ok(
        &srv,
        "vm.fork",
        json!({"snapshot": "vk-one/base", "count": 3, "prefix": "vk-bon"}),
    )
    .await;
    assert_eq!(f["vms"].as_array().unwrap().len(), 3);
    for i in 1..=3 {
        assert!(
            root.join(format!("vms/vk-bon-{i}/disk/toolchain.txt"))
                .exists()
        );
    }
    assert_eq!(
        kind(
            &fail(
                &srv,
                "vm.fork",
                json!({"snapshot": "vk-one/base", "count": 1, "prefix": "vk-bon"})
            )
            .await
        ),
        "conflict"
    );
    assert_eq!(
        kind(&fail(&srv, "vm.fork", json!({"snapshot": "x/y"})).await),
        "not_found"
    );
    // Transport plan.
    let t = ok(&srv, "vm.transport", json!({"vm": "vk-one"})).await;
    assert_eq!(t["chain"], json!(["exec"]));
    assert_eq!(t["configured"], "auto");
    // Cleanup, with the template protected.
    for i in 1..=3 {
        ok(&srv, "vm.destroy", json!({"vm": format!("vk-bon-{i}")})).await;
    }
    ok(&srv, "vm.destroy", json!({"vm": "vk-one"})).await;
    ok(
        &srv,
        "vm.snapshot.delete",
        json!({"snapshot": "vk-one/base"}),
    )
    .await;
    let tsnap = ok(&srv, "vm.list", json!({})).await["snapshots"]
        .as_array()
        .unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        kind(&fail(&srv, "vm.snapshot.delete", json!({"snapshot": tsnap})).await),
        "conflict"
    );
    ok(&srv, "vm.template.delete", json!({"key": key})).await;
    assert!(
        ok(&srv, "vm.template.list", json!({})).await["templates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for k in [
        "vm.created",
        "vm.started",
        "vm.stopped",
        "vm.suspended",
        "vm.resumed",
        "vm.destroyed",
        "vm.snapshot_created",
        "vm.forked",
        "vm.template_built",
    ] {
        assert!(!events(&srv, k).is_empty(), "{k}");
    }
}

#[tokio::test]
async fn a_real_provider_refuses_unfiltered_network_profiles() {
    let (d, srv) = server();
    let mut ic = vk_sandbox::config::IsolationConfig::default();
    ic.vm.enabled = true;
    ic.vm.provider = "tart".into();
    crate::sandbox::extras::set_cfg(&srv, ic);
    // A recording Tart backend: nothing real runs.
    let cmd = Arc::new(vk_sandbox::vm_backends::FakeCmd::default());
    struct Shared(Arc<vk_sandbox::vm_backends::FakeCmd>);
    impl vk_sandbox::vm_backends::CmdRunner for Shared {
        fn run(&self, a: &[String]) -> std::io::Result<vk_sandbox::vm_backends::CmdOut> {
            self.0.run(a)
        }
        fn spawn_detached(&self, a: &[String]) -> std::io::Result<()> {
            self.0.spawn_detached(a)
        }
    }
    set_backend(
        &srv,
        Arc::new(vk_sandbox::vm_backends::TartBackend::with(
            "tart",
            Box::new(Shared(cmd.clone())),
            &d.path().join("tart"),
        )),
    );
    let co = d.path().join("co");
    std::fs::create_dir_all(&co).unwrap();
    let e = crate::orch_vm::build_runner(
        &srv,
        "k1",
        Some("t1"),
        &co,
        vk_sandbox::NetworkProfile::Dev,
        &[],
        true,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(kind(&e), "unsupported");
    assert!(e.message.contains("not enforced"), "{}", e.message);
    assert!(cmd.argvs().is_empty(), "nothing was run");
}

/// Final review P1 3: a VM's workspace is the checkout, mounted writable. The checkout is
/// registered for host git hardening, so a guest that plants a clean filter in `.git/config`
/// gets nothing executed by host-side task status; `.git` is mounted read-only where the
/// provider nests mounts (not Tart, which would link through the host checkout).
#[tokio::test]
async fn vm_checkout_git_metadata_never_executes_on_the_host() {
    let (d, srv, root) = vm_server();
    let co = repo(d.path());
    let task = "vtask01JABCDEFGHJKMNPQRS1";
    let mut r = crate::sandbox::IsoRequest {
        level: vk_sandbox::IsolationLevel::Vm,
        network: vk_sandbox::NetworkProfile::Open,
        ..Default::default()
    };
    r.yolo = true;
    crate::sandbox::prepare_box(&srv, task, Some(task), &co, r)
        .await
        .unwrap();
    assert!(vk_tasks::is_contained(&co), "registered for host hardening");
    // The guest writes through its workspace mount.
    let ws = std::fs::read_dir(root.join("vms"))
        .unwrap()
        .flatten()
        .map(|e| e.path().join("disk/workspace"))
        .find(|p| p.exists())
        .expect("vm workspace");
    let marker = d.path().join("pwned");
    let mut cfg = std::fs::read_to_string(ws.join(".git/config")).unwrap();
    cfg.push_str(&format!(
        "[filter \"evil\"]\n\tclean = touch {}\n\tsmudge = cat\n\trequired = true\n",
        marker.display()
    ));
    std::fs::write(ws.join(".git/config"), cfg).unwrap();
    std::fs::write(ws.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    std::fs::write(ws.join("a.txt"), "changed by the guest\n").unwrap();
    let _ = vk_tasks::branch_status(&co, Some("main"));
    assert!(!marker.exists(), "host task status ran the guest's filter");
    // Control: unhardened git in the same checkout would have run it. `git add` always runs
    // the clean filter (`git status` skips it while the stat data still matches the index).
    vk_tasks::unregister_contained_checkout(&co);
    let _ = Command::new("git")
        .arg("-C")
        .arg(&co)
        .args(["add", "a.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output();
    assert!(marker.exists(), "control: the planted filter is live");
}

#[test]
fn vm_protect_mounts_cover_git_except_on_tart() {
    let d = tempfile::tempdir().unwrap();
    let co = d.path().join("co");
    std::fs::create_dir_all(co.join(".git")).unwrap();
    std::fs::create_dir_all(co.join(".githooks")).unwrap();
    let prot = vec![
        co.join(".githooks"),
        co.join("missing"),
        d.path().join("outside"),
    ];
    for kind in [
        vk_sandbox::vm::VmProviderKind::Fake,
        vk_sandbox::vm::VmProviderKind::Lima,
    ] {
        let mut s = vk_sandbox::vm::VmSpec::new("x");
        crate::orch_vm::protect_mounts(&mut s, kind, &co, &prot);
        let got: Vec<_> = s
            .mounts
            .iter()
            .map(|m| (m.target.clone(), m.read_only))
            .collect();
        assert_eq!(
            got,
            vec![
                (format!("{}/.git", vk_sandbox::vm::VM_WORKSPACE), true),
                (format!("{}/.githooks", vk_sandbox::vm::VM_WORKSPACE), true),
            ]
        );
    }
    let mut s = vk_sandbox::vm::VmSpec::new("x");
    crate::orch_vm::protect_mounts(&mut s, vk_sandbox::vm::VmProviderKind::Tart, &co, &prot);
    assert!(s.mounts.is_empty());
}

#[test]
fn config_sections_parse_from_the_toml_extra() {
    let v: toml::Value = toml::from_str(
        "[best_of_n]\nenabled = true\n[merge]\nenabled = true\nstrategy = \"squash\"",
    )
    .unwrap();
    let (c, e) = OrchestrateConfig::from_toml(Some(&v));
    assert!(e.is_none());
    assert!(c.best_of_n.enabled && c.merge.squash() && !c.quota.enabled);
}
