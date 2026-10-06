//! In-process tests for spec 15 T2/T3 (§12 rows that need neither a browser nor an LLM).
//!
//! Each test builds a real `Server` on temp dirs, a throwaway git repository with isolated git
//! config, and runs/turns/tool items through the same `tracking::observe` path the hooks use.
//! Checks run only after an explicit `task.check.authorize` through the API.

use super::*;
use crate::ServerOpts;
use crate::api::Ctx;
use crate::paths::Paths;
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Paths::ensure creates the runtime/state roots: keep them out of the real defaults.
        let base = std::env::temp_dir().join(format!("vk-review-tests-{}", std::process::id()));
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
    _dir: tempfile::TempDir,
    server: Arc<Server>,
    repo: PathBuf,
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const CONFIG: &str = r#"
[[task.checks]]
name = "unit"
command = "sh ci/test.sh"
timeout_s = 60

[[task.checks]]
name = "sso"
command = "sh ci/sso.sh"
timeout_s = 60
"#;

impl Env {
    fn new() -> Env {
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
        let server = Server::new(paths, opts).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        let e = Env {
            _dir: dir,
            server,
            repo,
        };
        e.write(".vibeke/config.toml", CONFIG);
        e.write(
            "ci/test.sh",
            "#!/bin/sh\ngrep -q pass status.txt && [ ! -f \"$FLAKE\" ]\n",
        );
        e.write("ci/sso.sh", "#!/bin/sh\nexit 0\n");
        e.write("status.txt", "fail\n");
        e.commit("base");
        e
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    fn commit(&self, msg: &str) -> String {
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-q", "-m", msg]);
        git(&self.repo, &["rev-parse", "HEAD"])
    }
    fn run(&self, id: &str) -> AgentRun {
        self.server.with_core(|c| c.run(id).cloned()).unwrap()
    }
    fn add_run(&self, id: &str, cwd: &Path) -> AgentRun {
        let t = now();
        let r = AgentRun {
            id: id.into(),
            handle: format!("h-{id}"),
            name: Some(id.into()),
            pane: format!("pane-{id}"),
            harness: "claude".into(),
            harness_version: None,
            integration: "hooks".into(),
            harness_session_id: Some(format!("sess-{id}")),
            transcript_path: None,
            resume_argv: vec![],
            cwd: Some(cwd.to_string_lossy().into_owned()),
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
        };
        self.put_run(r.clone());
        r
    }
    fn put_run(&self, r: AgentRun) {
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.run(r);
        self.server.commit(&mut c, tx).unwrap();
    }
    fn set_exec(&self, id: &str, e: Execution) {
        let mut r = self.run(id);
        r.execution.value = e;
        self.put_run(r);
    }
    /// One harness turn reported like the hook shim does: prompt, tool calls, Stop.
    fn turn(&self, run: &str, prompt: &str, tools: &[(&str, i32)], last: &str) {
        let r = self.run(run);
        let sid = r.harness_session_id.clone().unwrap();
        tracking::observe(
            &self.server,
            &r,
            "UserPromptSubmit",
            &json!({"session_id": sid, "prompt": prompt}),
        );
        for (i, (cmd, code)) in tools.iter().enumerate() {
            let id = format!("tool-{}-{i}-{}", r.turns_completed, crate::core::ulid());
            tracking::observe(
                &self.server,
                &r,
                "PreToolUse",
                &json!({"session_id": sid, "tool_name": "Bash", "tool_use_id": id, "tool_input": {"command": cmd}}),
            );
            tracking::observe(
                &self.server,
                &r,
                "PostToolUse",
                &json!({"session_id": sid, "tool_name": "Bash", "tool_use_id": id, "tool_response": {"exit_code": code}}),
            );
        }
        tracking::observe(
            &self.server,
            &r,
            "Stop",
            &json!({"session_id": sid, "last_assistant_message": last}),
        );
        let mut r = self.run(run);
        r.turns_completed += 1;
        r.done_rev += 1;
        self.put_run(r);
    }
}

fn user_ctx() -> Ctx {
    Ctx {
        client_id: "tester".into(),
        kind: "tui".into(),
        pane_scope: None,
        remote: false,
    }
}

async fn call(e: &Env, method: &str, p: Value) -> R {
    let ctx = user_ctx();
    if method.starts_with("attention.") {
        return attention_api(&e.server, &ctx, method, &p).await.unwrap();
    }
    if let Some(r) = api(&e.server, &ctx, method, &p).await {
        return r;
    }
    tracking::api(&e.server, &ctx, method, &p).await.unwrap()
}

async fn ok(e: &Env, method: &str, p: Value) -> Value {
    match call(e, method, p.clone()).await {
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
        .map(str::to_string)
        .unwrap_or_else(|| format!("{e:?}"))
}

/// Track the latest turn of `run` with the given criteria and stopping point.
async fn track(e: &Env, run: &str, criteria: Value) -> String {
    let t = ok(
        e,
        "task.track",
        json!({"run": run, "criteria": criteria, "stop_at": "implementation"}),
    )
    .await;
    t["task"]["id"].as_str().unwrap().to_string()
}

async fn review(e: &Env, task: &str) -> Value {
    ok(e, "task.review.get", json!({"task": task})).await
}

fn criterion<'a>(pkg: &'a Value, text: &str) -> &'a Value {
    let id = pkg["intent"]["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["text"] == text)
        .unwrap()["id"]
        .clone();
    pkg["assessment"]["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["criterion_id"] == id)
        .unwrap()
}

fn blockers(pkg: &Value) -> Vec<String> {
    pkg["assessment"]["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["kind"].as_str().unwrap().to_string())
        .collect()
}

/// Authorize (explicit, per candidate) and run a check; wait for its terminal state.
async fn verify(e: &Env, task: &str, subject: &str, check: &str) -> Value {
    ok(
        e,
        "task.check.authorize",
        json!({"task": task, "check": check, "subject": subject}),
    )
    .await;
    run_check(e, task, subject, check).await
}

async fn run_check(e: &Env, task: &str, subject: &str, check: &str) -> Value {
    let r = ok(
        e,
        "task.check.run",
        json!({"task": task, "check": check, "subject": subject}),
    )
    .await;
    let id = r["check_run"]["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let g = ok(e, "task.check.get", json!({"check_run": id})).await;
        let st = g["check_run"]["state"].as_str().unwrap().to_string();
        if !matches!(st.as_str(), "queued" | "running") {
            return g["check_run"].clone();
        }
        assert!(Instant::now() < deadline, "check {id} did not finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn subject_of(pkg: &Value) -> String {
    pkg["subject"]["id"].as_str().unwrap().to_string()
}

fn events(e: &Env, kind: &str) -> Vec<Value> {
    e.server.with_core(|c| {
        c.store
            .events_after(0, 10_000, &[kind.to_string()])
            .unwrap()
            .into_iter()
            .map(|ev| serde_json::to_value(ev).unwrap())
            .collect()
    })
}

// ---- T2 -----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn claim_and_unbound_command_are_not_evidence() {
    let e = Env::new();
    e.add_run("r1", &e.repo);
    e.turn("r1", "Fix the login redirect", &[], "Working on it");
    let task = track(
        &e,
        "r1",
        json!([{"text": "Tests pass", "checks": ["unit"]}]),
    )
    .await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    e.turn(
        "r1",
        "go on",
        &[("sh  ci/test.sh", 0)],
        "Done. All tests pass.",
    );
    let pkg = review(&e, &task).await;
    // The agent's prose is a claim, never a check.
    assert_eq!(pkg["claims"][0]["category"], "agent_claim");
    assert_eq!(pkg["claims"][0]["command"], "Done. All tests pass.");
    assert_eq!(pkg["claims"][0]["label"], "Agent claim · not a check");
    // The observed command ran, but nothing established its code subject.
    let oc = &pkg["observed_commands"][0];
    assert_eq!(oc["label"], "Command passed · code binding unverified");
    assert_eq!(oc["subject"], "unbound");
    assert_eq!(oc["exit_code"], 0);
    assert!(oc["duration_ms"].is_number());
    let c = criterion(&pkg, "Tests pass");
    assert_eq!(c["status"], "unknown", "{c}");
    assert_ne!(pkg["label"], "ready_for_review");
    assert_eq!(pkg["accept_capable"], true);
    // Without the observed command, the criterion is simply missing (claim alone).
    let e2 = Env::new();
    e2.add_run("r2", &e2.repo);
    e2.turn("r2", "Fix it", &[], "ok");
    let t2 = track(
        &e2,
        "r2",
        json!([{"text": "Tests pass", "checks": ["unit"]}]),
    )
    .await;
    e2.write("status.txt", "pass\n");
    e2.commit("fix");
    e2.turn("r2", "more", &[], "tests pass");
    let p2 = review(&e2, &t2).await;
    assert_eq!(p2["claims"][0]["command"], "tests pass");
    assert_eq!(criterion(&p2, "Tests pass")["status"], "missing");
    assert_ne!(p2["label"], "ready_for_review");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_check_never_ready_then_red_green_and_flake() {
    let e = Env::new();
    e.add_run("r1", &e.repo);
    e.turn("r1", "Make the tests pass", &[], "starting");
    let task = track(
        &e,
        "r1",
        json!([{"text": "Tests pass", "checks": ["unit"]}]),
    )
    .await;
    // No commits yet: nothing to accept.
    let pkg = review(&e, &task).await;
    assert!(pkg["subject"].is_null());
    assert_eq!(pkg["accept_capable"], false);

    e.write("code.txt", "attempt 1\n");
    let c1 = e.commit("attempt 1");
    let pkg = review(&e, &task).await;
    let s1 = subject_of(&pkg);
    assert_eq!(pkg["subject"]["head_sha"], c1.as_str());
    // Running without an authorization is refused (no host execution).
    assert_eq!(
        reason(
            call(
                &e,
                "task.check.run",
                json!({"task": task, "check": "unit", "subject": s1})
            )
            .await
        ),
        "authorization_required"
    );
    let red = verify(&e, &task, &s1, "unit").await;
    assert_eq!(red["state"], "failed");
    // Idle agent after a failed check: never Ready.
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_available");
    assert_eq!(criterion(&pkg, "Tests pass")["status"], "failed");
    assert!(blockers(&pkg).contains(&"required_criterion".to_string()));

    // A fixed revision can pass; the earlier failure stays visible as history.
    e.write("status.txt", "pass\n");
    let c2 = e.commit("fix");
    let pkg = review(&e, &task).await;
    let s2 = subject_of(&pkg);
    assert_ne!(s1, s2);
    assert_eq!(pkg["subject"]["head_sha"], c2.as_str());
    assert_eq!(criterion(&pkg, "Tests pass")["status"], "missing");
    let green = verify(&e, &task, &s2, "unit").await;
    assert_eq!(green["state"], "passed", "{green}");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "ready_for_review", "{}", pkg["assessment"]);
    let c = criterion(&pkg, "Tests pass");
    assert_eq!(c["status"], "supported");
    assert!(
        c["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("older revision (history)")),
        "{c}"
    );
    // The projection/label reached the task record (event review.label_changed).
    let label = e
        .server
        .with_core(|c| c.task(&task).unwrap().review_label.clone());
    assert_eq!(label.as_deref(), Some("ready_for_review"));
    assert!(!events(&e, "review.label_changed").is_empty());
    assert!(!events(&e, "review.candidate_created").is_empty());
    assert!(!events(&e, "check.passed").is_empty());

    // Ready is unavailable while the run works …
    e.set_exec("r1", Execution::Working);
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_available");
    assert!(blockers(&pkg).contains(&"run_active".to_string()));
    e.set_exec("r1", Execution::Idle);
    // … while a question is open on the bound run …
    let it = Interaction {
        id: "int-1".into(),
        handle: "i1".into(),
        run: "r1".into(),
        pane: "pane-r1".into(),
        kind: InteractionKind::Question,
        status: InteractionStatus::Open,
        title: "Dashboard or error?".into(),
        body_md: None,
        action: None,
        questions: vec![],
        plan_md: None,
        answer_channel: AnswerChannel::Native,
        native_ref: None,
        source: StateSource::Structured,
        confidence: 1.0,
        answerable: true,
        gate: false,
        decision_rev: 1,
        delivery: DeliveryState::None,
        delivery_error: None,
        answer: None,
        answered_by: None,
        opened_at_ms: now(),
        answered_at_ms: None,
    };
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.interaction(it.clone());
        e.server.commit(&mut c, tx).unwrap();
    }
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_available");
    assert!(blockers(&pkg).contains(&"open_interaction".to_string()));
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        let mut done = it.clone();
        done.status = InteractionStatus::Answered;
        tx.interaction(done);
        e.server.commit(&mut c, tx).unwrap();
    }
    // … while a message delivery is uncertain …
    let msg = tracking::TaskMessage {
        id: "msg-1".into(),
        task: task.clone(),
        binding: "b".into(),
        run: "r1".into(),
        native_conversation_id: "sess-r1".into(),
        intent_revision: Some(1),
        text: "add a test".into(),
        state: MessageState::DeliveryUnknown,
        detail: None,
        covers: vec![],
        idempotency_key: None,
        created_at_ms: now(),
        updated_at_ms: now(),
    };
    let put_msg = |m: &tracking::TaskMessage| {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        if m.state == MessageState::Delivered {
            tx.m.close(tracking::K_MESSAGE, &m.id, None, m);
        } else {
            tx.m.put(tracking::K_MESSAGE, &m.id, None, m);
        }
        e.server.commit(&mut c, tx).unwrap();
    };
    put_msg(&msg);
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_available");
    assert!(blockers(&pkg).contains(&"unresolved_delivery".to_string()));
    let mut delivered = msg.clone();
    delivered.state = MessageState::Delivered;
    put_msg(&delivered);
    // … and while another known writer works in the same checkout.
    e.add_run("other", &e.repo.join("ci"));
    e.set_exec("other", Execution::Working);
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_available");
    assert!(blockers(&pkg).contains(&"known_writer".to_string()));
    e.set_exec("other", Execution::Idle);
    assert_eq!(review(&e, &task).await["label"], "ready_for_review");

    // Same subject, mixed outcomes (a flake): needs judgment, the failure is not erased.
    let flake = e.repo.join("../flake");
    let cfg = CONFIG.replace(
        "timeout_s = 60\n\n[[task.checks]]\nname = \"sso\"",
        &format!(
            "timeout_s = 60\nenv = {{ FLAKE = \"{}\" }}\n\n[[task.checks]]\nname = \"sso\"",
            flake.display()
        ),
    );
    e.write(".vibeke/config.toml", &cfg);
    e.commit("flake-capable check");
    let pkg = review(&e, &task).await;
    let s3 = subject_of(&pkg);
    assert_eq!(verify(&e, &task, &s3, "unit").await["state"], "passed");
    std::fs::write(&flake, "x").unwrap();
    assert_eq!(run_check(&e, &task, &s3, "unit").await["state"], "failed");
    std::fs::remove_file(&flake).unwrap();
    assert_eq!(run_check(&e, &task, &s3, "unit").await["state"], "passed");
    let pkg = review(&e, &task).await;
    assert_eq!(criterion(&pkg, "Tests pass")["status"], "needs_judgment");
    assert_ne!(pkg["label"], "ready_for_review");
}

#[tokio::test(flavor = "multi_thread")]
async fn edited_check_recipe_needs_fresh_authorization() {
    let e = Env::new();
    e.add_run("r1", &e.repo);
    e.turn("r1", "Fix", &[], "");
    let task = track(
        &e,
        "r1",
        json!([{"text": "Tests pass", "checks": ["unit"]}]),
    )
    .await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    let pkg = review(&e, &task).await;
    let s1 = subject_of(&pkg);
    let unit = pkg["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "unit")
        .unwrap()
        .clone();
    assert_eq!(unit["trust"], "project_recipe");
    assert_eq!(unit["authorization"]["status"], "required");
    assert_eq!(
        unit["confirmation_label"],
        "Runs code modified by this task"
    );
    assert_eq!(verify(&e, &task, &s1, "unit").await["state"], "passed");

    // The agent edits the check script: the resolved definition changes.
    e.write("ci/test.sh", "#!/bin/sh\nexit 0\n");
    e.commit("make tests trivially pass");
    let pkg = review(&e, &task).await;
    let s2 = subject_of(&pkg);
    let unit = pkg["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "unit")
        .unwrap()
        .clone();
    assert_eq!(unit["trust"], "task_modified", "{unit}");
    assert!(
        unit["provenance"]["changed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["file"] == "ci/test.sh"),
        "{unit}"
    );
    let err = call(
        &e,
        "task.check.run",
        json!({"task": task, "check": "unit", "subject": s2}),
    )
    .await
    .unwrap_err();
    let d = err.data.details.clone();
    assert_eq!(d["reason"], "authorization_required");
    let reasons: Vec<&str> = d["requirement"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["reason"].as_str().unwrap())
        .collect();
    assert!(reasons.contains(&"task_modified_definition"), "{d}");
    assert!(
        d["provenance"]["note"]
            .as_str()
            .unwrap()
            .contains("changed")
    );
    // The earlier grant does not carry over to the new candidate; no run was recorded.
    assert!(
        pkg["check_runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["subject_id"] == s1.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn acceptance_conflicts_exceptions_and_invalidation() {
    let e = Env::new();
    e.add_run("r1", &e.repo);
    e.turn("r1", "Fix login redirect; preserve SSO", &[], "");
    let task = track(
        &e,
        "r1",
        json!([
            {"text": "Tests pass", "checks": ["unit"]},
            {"text": "SSO smoke test", "checks": ["sso"]}
        ]),
    )
    .await;
    e.write("status.txt", "pass\n");
    e.commit("fix");
    // Dirty, uncommitted work is inspect-only.
    e.write("scratch.txt", "wip\n");
    let pkg = review(&e, &task).await;
    let s1 = subject_of(&pkg);
    let live = pkg["inspect_only"][0].clone();
    assert_eq!(live["kind"], "checkout_live");
    assert_eq!(live["accept_capable"], false);
    let r = call(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": live["id"]}),
    )
    .await;
    let err = r.unwrap_err();
    assert!(err.message.contains("Select a committed revision"));
    assert_eq!(err.data.details["reason"], "subject_not_committed");
    std::fs::remove_file(e.repo.join("scratch.txt")).unwrap();

    // Missing required checks need explicit exceptions.
    assert_eq!(verify(&e, &task, &s1, "unit").await["state"], "passed");
    let r = call(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s1}),
    )
    .await;
    assert_eq!(reason(r), "exceptions_required");
    // Wrong intent revision → review_changed.
    let r = call(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 9, "subject_id": s1}),
    )
    .await;
    assert_eq!(reason(r), "review_changed");

    // A commit lands while the user is looking at s1: accepting s1 conflicts.
    let pkg_before = review(&e, &task).await;
    e.write("more.txt", "x\n");
    e.commit("agent keeps going");
    let sso_id = criterion(&pkg_before, "SSO smoke test")["criterion_id"].clone();
    let r = call(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s1,
               "exceptions": [{"criterion": sso_id, "reason": "SSO verified manually"}]}),
    )
    .await;
    let err = r.unwrap_err();
    let d = err.data.details.clone();
    assert_eq!(d["reason"], "review_changed", "{d}");
    assert_ne!(d["current_subject"], s1.as_str());

    // Accept the new candidate with an explicit exception for the missing SSO check.
    let pkg = review(&e, &task).await;
    let s2 = subject_of(&pkg);
    assert_eq!(verify(&e, &task, &s2, "unit").await["state"], "passed");
    let p = json!({"task": task, "intent_revision": 1, "subject_id": s2,
                   "exceptions": [{"criterion": sso_id, "reason": "SSO verified manually"}],
                   "idempotency_key": "accept-1"});
    let acc = ok(&e, "task.review.accept", p.clone()).await;
    assert_eq!(acc["label"], "reviewed_with_exceptions");
    assert_eq!(acc["acceptance"]["subject_id"], s2.as_str());
    assert_eq!(
        acc["acceptance"]["exceptions"][0]["reason"],
        "SSO verified manually"
    );
    assert_eq!(ok(&e, "task.review.accept", p).await["replayed"], true);
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "reviewed_with_exceptions");
    assert_eq!(pkg["acceptance"]["status"], "current");
    // The SSO criterion stays missing — never turned green.
    assert_eq!(criterion(&pkg, "SSO smoke test")["status"], "missing");

    // A later commit invalidates the acceptance visibly; history keeps it.
    e.write("later.txt", "y\n");
    e.commit("later change");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_outdated");
    assert_eq!(pkg["acceptance"]["status"], "outdated");
    assert_eq!(pkg["acceptance_history"].as_array().unwrap().len(), 1);
    assert_eq!(events(&e, "review.invalidated").len(), 1);
    assert_eq!(events(&e, "review.accepted").len(), 1);
    let label = e
        .server
        .with_core(|c| c.task(&task).unwrap().review_label.clone());
    assert_eq!(label.as_deref(), Some("review_outdated"));
    // Re-review stays possible: readiness of the new candidate is reported separately.
    assert_eq!(pkg["readiness"]["label"], "review_available");
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_check_and_new_intent_revision_invalidate() {
    let e = Env::new();
    e.add_run("r1", &e.repo);
    e.turn("r1", "Fix", &[], "");
    let task = track(&e, "r1", json!(["Looks right"])).await;
    e.commit_empty();
    let pkg = review(&e, &task).await;
    let s1 = subject_of(&pkg);
    // A human criterion needs judgment: that is the review itself, no exception needed.
    let acc = ok(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s1}),
    )
    .await;
    assert_eq!(acc["label"], "reviewed");
    assert_eq!(review(&e, &task).await["label"], "reviewed");
    // A check that fails on the accepted revision afterwards outdates the acceptance.
    assert_eq!(verify(&e, &task, &s1, "unit").await["state"], "failed");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_outdated");
    assert!(
        pkg["acceptance"]["outdated_reasons"][0]
            .as_str()
            .unwrap()
            .contains("failed on the accepted revision"),
        "{}",
        pkg["acceptance"]
    );
    // Accept again (explicitly), then a new intent revision outdates that acceptance.
    let acc = ok(
        &e,
        "task.review.accept",
        json!({"task": task, "intent_revision": 1, "subject_id": s1}),
    )
    .await;
    assert_eq!(acc["label"], "reviewed");
    assert_eq!(review(&e, &task).await["label"], "reviewed");
    ok(
        &e,
        "task.intent.update",
        json!({"task": task, "expected_revision": 1, "add_criterion": "Docs updated"}),
    )
    .await;
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["label"], "review_outdated");
    assert!(
        pkg["acceptance"]["outdated_reasons"][0]
            .as_str()
            .unwrap()
            .contains("task details changed")
    );
    assert_eq!(pkg["acceptance_history"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn binding_ranges_and_pinned_end_candidates() {
    let e = Env::new();
    e.add_run("r1", &e.repo);
    e.turn("r1", "warm up", &[("echo before", 0)], "hi");
    e.turn("r1", "Fix the bug", &[("cargo test", 101)], "fixing");
    let t = ok(
        &e,
        "task.track",
        json!({"run": "r1", "turn": 2, "criteria": ["Bug fixed"], "stop_at": "implementation"}),
    )
    .await;
    let task = t["task"]["id"].as_str().unwrap().to_string();
    e.write("fix.txt", "fixed\n");
    let c1 = e.commit("fix");
    e.turn("r1", "lint too", &[("make lint", 0)], "linted");
    ok(&e, "task.unbind", json!({"task": task})).await;
    // The end candidate is pinned off the state path.
    let deadline = Instant::now() + Duration::from_secs(10);
    while events(&e, "review.end_candidate_pinned").is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Work after the binding closed (task B / an untracked shell) is not absorbed.
    e.turn("r1", "something else", &[("echo after", 0)], "other work");
    e.write("other.txt", "b\n");
    e.commit("unrelated");
    let pkg = review(&e, &task).await;
    assert_eq!(pkg["subject"]["head_sha"], c1.as_str());
    assert_eq!(pkg["subject_current"], true);
    assert_eq!(pkg["candidates"][0]["source"], "binding_end");
    let cmds: Vec<&str> = pkg["observed_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    assert_eq!(cmds, vec!["cargo test", "make lint"]);
    assert_eq!(pkg["observed_commands"][0]["outcome"], "failed");
    assert_eq!(pkg["claims"][0]["command"], "linted");

    // A binding that closes without commits records "no bound end candidate".
    let e2 = Env::new();
    e2.add_run("r2", &e2.repo);
    e2.turn("r2", "Investigate", &[], "");
    let t2 = track(&e2, "r2", json!(["Explain"])).await;
    e2.write("dirty.txt", "uncommitted\n");
    ok(&e2, "task.unbind", json!({"task": t2})).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while events(&e2, "review.end_candidate_pinned").is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let pkg = review(&e2, &t2).await;
    assert!(pkg["subject"].is_null());
    assert!(
        pkg["no_end_candidate"][0]["note"]
            .as_str()
            .unwrap()
            .contains("No bound end candidate")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_reconciles_checks_without_relaunch() {
    let e = Env::new();
    e.add_run("r1", &e.repo);
    e.turn("r1", "Fix", &[], "");
    let task = track(
        &e,
        "r1",
        json!([{"text": "Tests pass", "checks": ["unit"]}]),
    )
    .await;
    e.commit_empty();
    let pkg = review(&e, &task).await;
    let subj: ChangeSubject = serde_json::from_value(pkg["subject"].clone()).unwrap();
    let def: CheckDefinition =
        serde_json::from_value(pkg["checks"][0]["definition"].clone()).unwrap();
    let grant =
        checks::grant_per_candidate(&def, &subj, vk_review::Actor::user("u"), now()).unwrap();
    let mut running = CheckRunRec {
        task: task.clone(),
        run: CheckRun::queued(&def, &subj, &grant, "k1"),
        definition: def.clone(),
        subject: subj.clone(),
    };
    running.run.state = CheckState::Running;
    let queued = CheckRunRec {
        run: CheckRun::queued(&def, &subj, &grant, "k2"),
        ..running.clone()
    };
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        put_check(&mut tx, &running);
        put_check(&mut tx, &queued);
        e.server.commit(&mut c, tx).unwrap();
    }
    recover(&e.server);
    let r1 = ok(&e, "task.check.get", json!({"check_run": running.run.id})).await;
    let r2 = ok(&e, "task.check.get", json!({"check_run": queued.run.id})).await;
    assert_eq!(r1["check_run"]["state"], "unknown");
    assert_eq!(r2["check_run"]["state"], "interrupted");
    assert!(cancels().lock().unwrap().get(&running.run.id).is_none());
}

impl Env {
    fn commit_empty(&self) -> String {
        self.write("touch.txt", &crate::core::ulid());
        self.commit("touch")
    }
}

#[test]
fn pane_scope_cannot_accept_authorize_run_or_update_attention() {
    init_env();
    let e_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: e_dir.path().join("run"),
        state: e_dir.path().join("state"),
    };
    let server = Server::new(
        paths,
        ServerOpts {
            session: "t".into(),
            machine: "m".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
        },
    )
    .unwrap();
    let ctx = Ctx {
        client_id: "agent".into(),
        kind: "agent".into(),
        pane_scope: Some("pane-x".into()),
        remote: false,
    };
    for m in [
        "task.review.accept",
        "task.check.authorize",
        "task.check.run",
        "task.check.cancel",
        "attention.update",
    ] {
        let e = crate::api::authorize(&server, &ctx, m, &json!({})).unwrap_err();
        assert_eq!(e.code, ErrorKind::PermissionDenied.code(), "{m}");
    }
    // Reads stay open.
    assert!(crate::api::authorize(&server, &ctx, "task.review.get", &json!({})).is_ok());
    assert!(crate::api::authorize(&server, &ctx, "attention.list", &json!({})).is_ok());
}

#[test]
fn parses_task_checks() {
    let specs = parse_checks(CONFIG);
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].id, "unit");
    assert_eq!(
        specs[0].command,
        CheckCommand::Shell("sh ci/test.sh".into())
    );
    assert_eq!(specs[0].timeout_ms, 60_000);
    let argv = parse_checks("[[checks]]\nname = \"t\"\ncommand = [\"npm\", \"test\"]\n");
    assert_eq!(
        argv[0].command,
        CheckCommand::Argv(vec!["npm".into(), "test".into()])
    );
    assert!(parse_checks("not toml [").is_empty());
}

// ---- T3 -----------------------------------------------------------------------------------------

fn put_task(e: &Env, id: &str, title: &str, effort: Option<&str>) -> Task {
    let t = Task {
        id: id.into(),
        handle: format!("k-{id}"),
        title: title.into(),
        slug: id.into(),
        repo_root: e.repo.to_string_lossy().into_owned(),
        worktree_path: Some(e.repo.to_string_lossy().into_owned()),
        status: "active".into(),
        created_at_ms: now(),
        ownership: TaskOwnership::Attached,
        intent_revision: Some(1),
        effort: effort.map(str::to_string),
        rev: 1,
        ..Default::default()
    };
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.task(t.clone());
    e.server.commit(&mut c, tx).unwrap();
    t
}

fn put_projection(e: &Env, p: Projection) {
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put(K_PROJ, &p.task, None, &p);
    e.server.commit(&mut c, tx).unwrap();
}

fn projection(task: &str, label: &str, at: i64, rev: u64) -> Projection {
    Projection {
        task: task.into(),
        label: label.into(),
        label_text: label_text(label).into(),
        subject_id: Some(format!("subj-{task}")),
        head_sha: Some("0123456789abcdef".into()),
        candidate_at_ms: Some(at),
        intent_revision: Some(1),
        package_revision: 7,
        revision: rev,
        failed_checks: vec![],
        explanation: None,
        updated_at_ms: at,
    }
}

fn put_interaction(e: &Env, id: &str, run: &str, opened: i64) {
    let it = Interaction {
        id: id.into(),
        handle: format!("i-{id}"),
        run: run.into(),
        pane: format!("pane-{run}"),
        kind: InteractionKind::Question,
        status: InteractionStatus::Open,
        title: "Use the dashboard or show an error?".into(),
        body_md: None,
        action: None,
        questions: vec![],
        plan_md: None,
        answer_channel: AnswerChannel::Native,
        native_ref: None,
        source: StateSource::Structured,
        confidence: 1.0,
        answerable: true,
        gate: false,
        decision_rev: 1,
        delivery: DeliveryState::None,
        delivery_error: None,
        answer: None,
        answered_by: None,
        opened_at_ms: opened,
        answered_at_ms: None,
    };
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.interaction(it);
    e.server.commit(&mut c, tx).unwrap();
}

fn keys(v: &Value) -> Vec<(String, String)> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["key"]["kind"].as_str().unwrap().to_string(),
                i["key"]["id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn attention_ranks_classes_and_five_minute_view_keeps_urgent() {
    let e = Env::new();
    let t0 = now();
    e.add_run("r1", &e.repo);
    put_task(&e, "tA", "Fix invoice export", Some("deep"));
    put_task(&e, "tB", "Tidy logging", Some("quick"));
    put_projection(&e, projection("tA", "review_available", t0 - 600_000, 1));
    put_projection(&e, projection("tB", "ready_for_review", t0 - 60_000, 1));
    put_interaction(&e, "q1", "r1", t0 - 12 * 60_000);
    // A message whose delivery is uncertain (class 1, urgent).
    let msg = tracking::TaskMessage {
        id: "m1".into(),
        task: "tA".into(),
        binding: "b".into(),
        run: "r1".into(),
        native_conversation_id: "sess-r1".into(),
        intent_revision: Some(1),
        text: "Please add the SSO test".into(),
        state: MessageState::DeliveryUnknown,
        detail: None,
        covers: vec![],
        idempotency_key: None,
        created_at_ms: t0 - 1000,
        updated_at_ms: t0 - 1000,
    };
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(tracking::K_MESSAGE, &msg.id, None, &msg);
        e.server.commit(&mut c, tx).unwrap();
    }
    // A finished, unseen turn on an untracked run.
    e.add_run("r2", &e.repo);
    let mut r2 = e.run("r2");
    r2.done_rev = 1;
    r2.execution.since_ms = t0 - 5000;
    e.put_run(r2);

    let v = ok(&e, "attention.list", json!({})).await;
    let k = keys(&v);
    assert_eq!(
        k,
        vec![
            ("send_unknown".into(), "m1".into()),
            ("interaction".into(), "q1".into()),
            ("review".into(), "tA:subj-tA".into()),
            ("review".into(), "tB:subj-tB".into()),
            ("finished_turn".into(), "r2".into()),
        ],
        "{v}"
    );
    let items = v["items"].as_array().unwrap();
    assert_eq!(items[0]["class"], 1);
    assert_eq!(items[0]["urgent"], true);
    assert_eq!(items[1]["class"], 3);
    assert_eq!(items[1]["explanation"], "Waiting 12m · blocks this run");
    assert_eq!(items[1]["pane"], "pane-r1");
    assert_eq!(items[1]["interaction"], "q1");
    assert_eq!(items[2]["class"], 4);
    assert_eq!(items[2]["effort"], "deep");
    assert_eq!(items[2]["task"], "tA");
    assert_eq!(items[4]["class"], "finished_turns");
    assert!(items[4]["effort"].is_null());
    assert_eq!(v["coverage"]["complete"], true);
    assert!(v["five_minute"].is_null());

    // Five-minute view with a tiny budget: the urgent item stays, the rest is counted.
    let v = ok(&e, "attention.list", json!({"budget_ms": 60_000})).await;
    let five = &v["five_minute"];
    let fk: Vec<String> = five["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["id"].as_str().unwrap().to_string())
        .collect();
    assert!(fk.contains(&"m1".to_string()), "{five}");
    assert!(five["omitted_count"].as_u64().unwrap() >= 2, "{five}");
    assert!(five["note"].as_str().unwrap().contains("more in All items"));
    // All items remain listed.
    assert_eq!(v["items"].as_array().unwrap().len(), 5);
    // Pinning raises an item within its class only.
    ok(
        &e,
        "attention.update",
        json!({"key": {"kind": "review", "id": "tB:subj-tB"}, "pin": true}),
    )
    .await;
    let k = keys(&ok(&e, "attention.list", json!({})).await);
    assert_eq!(k[2].1, "tB:subj-tB");
    assert_eq!(k[0].1, "m1");
}

#[tokio::test(flavor = "multi_thread")]
async fn snooze_hides_until_deadline_or_material_change() {
    let e = Env::new();
    let t0 = now();
    put_task(&e, "tA", "Fix invoice export", None);
    put_projection(&e, projection("tA", "review_available", t0 - 1000, 1));
    let key = json!({"kind": "review", "id": "tA:subj-tA"});

    // Snooze until a deadline: hidden, then back when the deadline passes.
    let r = ok(
        &e,
        "attention.update",
        json!({"key": key, "snooze_until_ms": now() + 400}),
    )
    .await;
    assert!(r["snoozed_until_ms"].is_number());
    assert!(keys(&ok(&e, "attention.list", json!({})).await).is_empty());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let v = ok(&e, "attention.list", json!({})).await;
    assert_eq!(keys(&v).len(), 1);
    assert!(v["items"][0]["snoozed_until_ms"].is_null());

    // Snooze for an hour; an unrelated change does not wake it …
    ok(
        &e,
        "attention.update",
        json!({"key": key, "snooze_until_ms": now() + 3_600_000}),
    )
    .await;
    let mut p = projection("tA", "review_available", t0 - 1000, 1);
    p.explanation = Some("unrelated".into());
    put_projection(&e, p);
    assert!(keys(&ok(&e, "attention.list", json!({})).await).is_empty());
    // … a material revision does, with the reason.
    put_projection(&e, projection("tA", "review_available", t0 - 1000, 2));
    let v = ok(&e, "attention.list", json!({})).await;
    assert_eq!(keys(&v).len(), 1);
    assert_eq!(
        v["items"][0]["woke_from_snooze"],
        "Changed since you snoozed it"
    );
    assert!(v["items"][0]["snoozed_until_ms"].is_number());

    // Seen marks an item (revision-scoped) without removing a review candidate.
    ok(
        &e,
        "attention.update",
        json!({"key": key, "snooze_until_ms": null, "seen": true}),
    )
    .await;
    let v = ok(&e, "attention.list", json!({})).await;
    assert_eq!(keys(&v).len(), 1);
    assert!(
        !v["items"][0]["explanation"]
            .as_str()
            .unwrap()
            .contains("not yet seen")
    );

    // Urgent items are never hidden by a snooze.
    let msg = tracking::TaskMessage {
        id: "m1".into(),
        task: "tA".into(),
        binding: "b".into(),
        run: "r".into(),
        native_conversation_id: "s".into(),
        intent_revision: Some(1),
        text: "x".into(),
        state: MessageState::DeliveryUnknown,
        detail: None,
        covers: vec![],
        idempotency_key: None,
        created_at_ms: t0,
        updated_at_ms: t0,
    };
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.put(tracking::K_MESSAGE, &msg.id, None, &msg);
        e.server.commit(&mut c, tx).unwrap();
    }
    ok(
        &e,
        "attention.update",
        json!({"key": {"kind": "send_unknown", "id": "m1"}, "snooze_until_ms": now() + 3_600_000}),
    )
    .await;
    let v = ok(&e, "attention.list", json!({})).await;
    assert_eq!(v["items"][0]["key"]["id"], "m1");
    assert_eq!(v["items"][0]["woke_from_snooze"], "Became urgent");
    assert!(events(&e, "attention.preference_changed").len() >= 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn next_attention_reaches_oldest_unseen_done_run_without_tasks() {
    let e = Env::new();
    let t0 = now();
    for (id, age) in [("young", 1_000), ("old", 90_000), ("seen", 200_000)] {
        e.add_run(id, &e.repo);
        let mut r = e.run(id);
        r.done_rev = 1;
        r.execution.since_ms = t0 - age;
        e.put_run(r);
    }
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.read_mark("local", "pane-seen", 1);
        e.server.commit(&mut c, tx).unwrap();
    }
    let v = ok(&e, "attention.list", json!({})).await;
    let k = keys(&v);
    assert_eq!(
        k,
        vec![
            ("finished_turn".into(), "old".into()),
            ("finished_turn".into(), "young".into())
        ]
    );
    assert_eq!(v["items"][0]["pane"], "pane-old");
    // Marking it seen (from the inbox) moves on to the next one.
    ok(
        &e,
        "attention.update",
        json!({"key": {"kind": "finished_turn", "id": "old"}, "seen": true}),
    )
    .await;
    let k = keys(&ok(&e, "attention.list", json!({})).await);
    assert_eq!(k, vec![("finished_turn".into(), "young".into())]);
}

#[tokio::test(flavor = "multi_thread")]
async fn attention_list_with_100_tasks_and_20_runs_is_fast() {
    let e = Env::new();
    let t0 = now();
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        for i in 0..100 {
            let id = format!("task{i:03}");
            tx.task(Task {
                id: id.clone(),
                handle: format!("k{i}"),
                title: format!("Task {i}"),
                slug: id.clone(),
                repo_root: "/tmp".into(),
                status: "active".into(),
                created_at_ms: t0,
                ownership: TaskOwnership::Attached,
                intent_revision: Some(1),
                effort: Some(["quick", "minutes", "deep", "unknown"][i % 4].into()),
                priority: Some((i % 3) as i32),
                rev: 1,
                ..Default::default()
            });
            let mut p = projection(&id, "review_available", t0 - i as i64 * 1000, 1);
            if i % 10 == 0 {
                p.failed_checks.push(FailedCheck {
                    run: format!("cr{i}"),
                    check: "unit".into(),
                    state: CheckState::Failed,
                    ended_at_ms: t0,
                });
            }
            tx.m.put(K_PROJ, &id, None, &p);
        }
        e.server.commit(&mut c, tx).unwrap();
    }
    for i in 0..20 {
        let id = format!("run{i:02}");
        e.add_run(&id, &e.repo);
        let mut r = e.run(&id);
        r.done_rev = (i % 2) as u64;
        r.execution.value = if i % 3 == 0 {
            Execution::Working
        } else {
            Execution::Idle
        };
        e.put_run(r);
        if i % 2 == 0 {
            put_interaction(&e, &format!("int{i}"), &id, t0 - i as i64 * 10_000);
        }
    }
    let mut times = Vec::new();
    let mut n = 0;
    for _ in 0..30 {
        let t = Instant::now();
        let v = ok(&e, "attention.list", json!({"budget_ms": 300_000})).await;
        times.push(t.elapsed());
        n = v["items"].as_array().unwrap().len();
    }
    times.sort();
    let p95 = times[(times.len() * 95) / 100 - 1];
    assert!(n >= 100 + 10 + 10, "items: {n}");
    assert!(
        p95 <= Duration::from_millis(100),
        "attention.list p95 {p95:?} (all: {times:?})"
    );
}
