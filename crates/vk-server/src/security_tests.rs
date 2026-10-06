//! In-process tests of the server security surface (09): `policy.*` with repository policy and
//! trust, `auth.revoke_token` / `auth.elevate` / `auth.elevate.decide` / `auth.list`, the
//! hash-chained audit log (`audit.tail|search|verify`) and integration tamper detection
//! (`integration.doctor`). A throwaway server (holders spawn `/bin/false`); harness configs live
//! in temp dirs (never the user's real ones); every call is bounded.

use crate::api::{Ctx, dispatch};
use crate::core::Tx;
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-security-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
            std::env::set_var("VIBEKE_NO_OPEN", "1");
            if std::env::var_os("VIBEKE_RUNTIME_DIR").is_none() {
                std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            }
            if std::env::var_os("VIBEKE_STATE_DIR").is_none() {
                std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            }
            if std::env::var_os("VIBEKE_CONFIG").is_none() {
                std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
            }
        }
    });
}

struct Env {
    dir: tempfile::TempDir,
    server: Arc<Server>,
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

fn pane(id: &str, cwd: Option<&Path>, child_pid: Option<u32>) -> Pane {
    Pane {
        id: id.into(),
        handle: id.into(),
        tab: "tab-a".into(),
        workspace: "ws-a".into(),
        title: None,
        auto_title: String::new(),
        cwd: cwd.map(|c| c.to_string_lossy().into_owned()),
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
        let e = Env { dir, server };
        e.put_pane(pane("pane-a", None, Some(4242)));
        e.put_pane(pane("pane-b", None, Some(4343)));
        e
    }
    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
    fn put_pane(&self, p: Pane) {
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(p);
        self.server.commit(&mut c, tx).unwrap();
    }
    async fn call(&self, ctx: &Ctx, method: &str, p: Value) -> Result<Value, RpcError> {
        tokio::time::timeout(
            Duration::from_secs(20),
            dispatch(&self.server, ctx, method, &p),
        )
        .await
        .unwrap_or_else(|_| panic!("{method} did not finish"))
    }
    /// A successful full-scope call whose params and result match the schema registry.
    async fn ok(&self, method: &str, p: Value) -> Value {
        let problems = crate::api_schema::validate_params(method, &p);
        assert!(problems.is_empty(), "{method} params {p}: {problems:?}");
        let v = self
            .call(&user(), method, p.clone())
            .await
            .unwrap_or_else(|e| panic!("{method} {p}: {e:?}"));
        let problems = crate::api_schema::validate_result(method, &v);
        assert!(problems.is_empty(), "{method} result {v}: {problems:?}");
        v
    }
    fn audit_types(&self) -> Vec<String> {
        crate::audit::read_entries(&self.server.paths.audit_log(), &[], None, None, 10_000)
            .iter()
            .map(|e| e["type"].as_str().unwrap_or("").to_string())
            .collect()
    }
    fn events(&self, kind: &str) -> Vec<vk_store::Event> {
        crate::audit::flush(&self.server, true);
        let evs = self.server.with_core(|c| {
            c.store
                .events_after(0, 100_000, &[kind.to_string()])
                .unwrap()
        });
        for e in &evs {
            let v = serde_json::to_value(e).unwrap();
            let problems = crate::api_schema::validate_event(&v);
            assert!(problems.is_empty(), "event {v}: {problems:?}");
        }
        evs
    }
}

fn kind(e: &RpcError) -> ErrorKind {
    ErrorKind::ALL
        .into_iter()
        .find(|k| k.code() == e.code)
        .unwrap_or(ErrorKind::Internal)
}

/// A repository with a `.vibeke/policy.toml`.
fn repo_with_policy(root: &Path, toml: &str) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
    std::fs::write(repo.join(".vibeke/policy.toml"), toml).unwrap();
    repo
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_rules_add_list_test_remove_and_repo_policy_only_tightens() {
    let e = Env::new();
    // Validation.
    let r = e
        .call(&user(), "policy.add", json!({"effect": "allow"}))
        .await;
    assert_eq!(
        kind(&r.unwrap_err()),
        ErrorKind::InvalidParams,
        "no matcher"
    );
    let r = e
        .call(
            &user(),
            "policy.add",
            json!({"tool": "Bash", "effect": "sometimes"}),
        )
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::InvalidParams);
    let r = e
        .call(
            &user(),
            "policy.add",
            json!({"rule": {"match": {"command_regex": "("}, "effect": "deny"}}),
        )
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::InvalidParams);

    // A user rule allowing every Bash command, added through the API.
    let added = e
        .ok(
            "policy.add",
            json!({"rule": {"match": {"tool": "Bash"}, "effect": "allow", "note": "trust bash"}}),
        )
        .await;
    let id = added["rule"]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("p-"), "{added}");
    assert_eq!(added["rule"]["source"], "user");
    assert_eq!(added["rule"]["created_by"], "tui");
    let listed = e.ok("policy.list", json!({})).await;
    assert!(
        listed["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id.as_str())
    );

    let repo = repo_with_policy(
        &e.root(),
        r#"
[[rule]]
match = { tool = "Bash", command_regex = '^rm -rf' }
effect = "deny"

[[rule]]
match = { tool = "Edit", path_glob = "src/**" }
effect = "allow"
"#,
    );
    let test = |cmd: &str| json!({"action": {"tool": "Bash", "command": cmd}, "scope": {"cwd": repo.join("sub")}});
    // Untrusted: the repository's rules are ignored; the user rule allows.
    let t = e.ok("policy.test", test("rm -rf /")).await;
    assert_eq!(t["effect"], "allow", "{t}");
    assert_eq!(t["rule"]["id"], id.as_str());
    let l = e.ok("policy.list", json!({"scope": repo})).await;
    assert_eq!(l["repos"][0]["trusted"], false, "{l}");
    assert!(
        l["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["source"] == "repo")
            .all(|r| r["ignored"].is_string())
    );

    // Trusted: the repository's deny tightens the user's allow (09 §4 rule 2) ...
    let tr = e.ok("policy.trust", json!({"path": repo})).await;
    assert_eq!(tr["allow_policy_grants"], false);
    assert_eq!(tr["policy_rules"].as_array().unwrap().len(), 2);
    let t = e.ok("policy.test", test("rm -rf /")).await;
    assert_eq!(t["effect"], "deny", "{t}");
    assert_eq!(t["rule"]["source"], "repo");
    assert_eq!(t["user_rule"]["id"], id.as_str());
    let t = e.ok("policy.test", test("ls")).await;
    assert_eq!(t["effect"], "allow");
    // ... but its allow rule is ignored without allow_policy_grants.
    let edit = json!({"action": {"tool": "Edit", "paths": ["src/a.rs"]}, "scope": repo});
    assert_eq!(e.ok("policy.test", edit.clone()).await["effect"], "ask");
    e.ok(
        "policy.trust",
        json!({"path": repo, "allow_policy_grants": true}),
    )
    .await;
    let t = e.ok("policy.test", edit.clone()).await;
    assert_eq!(t["effect"], "allow", "{t}");
    assert_eq!(t["rule"]["source"], "repo");
    // An edit of .vibeke/ (an agent writing policy) invalidates trust: back to ignored.
    std::fs::write(
        repo.join(".vibeke/policy.toml"),
        "[[rule]]\nmatch = { tool = \"Edit\" }\neffect = \"allow\"\n",
    )
    .unwrap();
    assert_eq!(e.ok("policy.test", edit).await["effect"], "ask");
    assert_eq!(
        e.ok("policy.test", test("rm -rf /")).await["effect"],
        "allow"
    );

    // The approval fast path uses the same evaluation (pane cwd in the repository).
    e.put_pane(pane("pane-r", Some(&repo), None));
    let it = Interaction {
        id: "i1".into(),
        handle: "i1".into(),
        run: "no-run".into(),
        pane: "pane-r".into(),
        kind: InteractionKind::Approval,
        status: InteractionStatus::Open,
        title: "Bash".into(),
        body_md: None,
        action: Some(ActionInfo {
            tool: "Bash".into(),
            summary: String::new(),
            command: Some("cargo test".into()),
            paths: vec![],
            diff: None,
            risk: Risk::Low,
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
        decision_rev: 0,
        delivery: DeliveryState::None,
        delivery_error: None,
        answer: None,
        answered_by: None,
        answer_key: None,
        opened_at_ms: 0,
        answered_at_ms: None,
    };
    assert_eq!(
        crate::policy_api::match_interaction(&e.server, &it),
        Some(("allow".into(), id.clone()))
    );

    // Removal: file rules can't be removed through the API; unknown ids are not found.
    let r = e
        .call(&user(), "policy.remove", json!({"rule_id": "repo:1"}))
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::InvalidParams);
    let r = e
        .call(&user(), "policy.remove", json!({"rule_id": "p-nope"}))
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::NotFound);
    let removed = e.ok("policy.remove", json!({"rule_id": id})).await;
    assert_eq!(removed["removed"], true);
    assert_eq!(crate::policy_api::match_interaction(&e.server, &it), None);

    // Panes can't read or change policy (09 §5.2), and the attempt is audited.
    for m in ["policy.add", "policy.list", "policy.test", "policy.remove"] {
        let r = e
            .call(
                &pane_ctx("pane-a"),
                m,
                json!({"tool": "Bash", "effect": "allow"}),
            )
            .await;
        assert_eq!(kind(&r.unwrap_err()), ErrorKind::PermissionDenied, "{m}");
    }
    let types = e.audit_types();
    for t in [
        "policy.rule_added",
        "policy.repo_trusted",
        "policy.rule_removed",
        "security.permission_denied",
    ] {
        assert!(types.iter().any(|x| x == t), "{t} not audited: {types:?}");
    }
    assert_eq!(e.events("policy.rule_added").len(), 1);
    assert_eq!(e.events("policy.rule_removed").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_log_is_chained_searchable_and_verified() {
    let e = Env::new();
    for i in 0..3 {
        e.ok(
            "policy.add",
            json!({"tool": format!("Tool{i}"), "effect": "deny", "note": "password=hunter2"}),
        )
        .await;
    }
    // A self-answer attempt from a pane is recorded as such.
    let r = e
        .call(
            &pane_ctx("pane-a"),
            "interaction.answer",
            json!({"interaction": "x", "decision": "allow"}),
        )
        .await;
    assert!(r.unwrap_err().message.starts_with("self_answer_forbidden"));
    let tail = e.ok("audit.tail", json!({"limit": 2})).await;
    let entries = tail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1]["type"], "security.self_answer_attempt");
    assert_eq!(entries[1]["actor"]["pane"], "pane-a");
    assert_eq!(
        entries[0]["seq"].as_u64().unwrap() + 1,
        entries[1]["seq"].as_u64().unwrap()
    );
    assert_eq!(entries[1]["prev_hash"], entries[0]["hash"]);
    // Free text is redacted before it is written.
    let log = std::fs::read_to_string(e.server.paths.audit_log()).unwrap();
    assert!(!log.contains("hunter2"), "{log}");
    let found = e.ok("audit.search", json!({"query": "tool1"})).await;
    assert_eq!(found["entries"].as_array().unwrap().len(), 1);
    let by_type = e.ok("audit.search", json!({"types": ["policy.*"]})).await;
    assert_eq!(by_type["entries"].as_array().unwrap().len(), 3);
    let r = e.call(&user(), "audit.search", json!({})).await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::InvalidParams);
    let v = e.ok("audit.verify", json!({})).await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["entries"], 4);
    // Every record is mirrored as an `audit.recorded` event.
    assert_eq!(e.events("audit.recorded").len(), 4);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(e.server.paths.audit_log())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // Tampering is detected: rewrite one entry's text.
    std::fs::write(
        e.server.paths.audit_log(),
        log.replacen("Tool1", "Tool9", 1),
    )
    .unwrap();
    let v = e.ok("audit.verify", json!({})).await;
    assert_eq!(v["ok"], false);
    // A restarted server finding the log cut short continues the chain and says so.
    let lines: Vec<&str> = log.lines().collect();
    std::fs::write(e.server.paths.audit_log(), lines[..2].join("\n") + "\n").unwrap();
    let Env { dir, server } = e;
    let root = dir.path().canonicalize().unwrap();
    drop(server);
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let server = Server::new(
        paths,
        ServerOpts {
            session: "t".into(),
            machine: "testbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
        },
    )
    .unwrap();
    let e = Env { dir, server };
    e.ok("policy.add", json!({"tool": "After", "effect": "deny"}))
        .await;
    let v = e.ok("audit.verify", json!({})).await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["discontinuities"], json!([3]));
    assert!(e.audit_types().contains(&"audit.discontinuity".to_string()));
    // Panes can't read the audit log.
    let r = e.call(&pane_ctx("pane-a"), "audit.tail", json!({})).await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::PermissionDenied);
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_pane_loses_api_access_until_restart() {
    let e = Env::new();
    let tok = e.server.token_for("pane-a");
    let _tok_b = e.server.token_for("pane-b");
    assert_eq!(e.server.pane_for_token(&tok).as_deref(), Some("pane-a"));
    e.call(&pane_ctx("pane-a"), "pane.list", json!({}))
        .await
        .unwrap();
    // Panes can't revoke.
    let r = e
        .call(
            &pane_ctx("pane-b"),
            "auth.revoke_token",
            json!({"pane": "pane-a"}),
        )
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::PermissionDenied);
    let r = e.ok("auth.revoke_token", json!({"pane": "pane-a"})).await;
    assert_eq!(r["revoked"], true);
    assert_eq!(r["tokens_removed"], 1);
    assert!(e.server.pane_for_token(&tok).is_none());
    // Every call from the pane (by token or ancestry) is refused, also on open connections.
    let err = e
        .call(&pane_ctx("pane-a"), "pane.list", json!({}))
        .await
        .unwrap_err();
    assert_eq!(kind(&err), ErrorKind::PermissionDenied);
    assert!(err.message.starts_with("token_revoked"), "{}", err.message);
    // Other panes are unaffected.
    e.call(&pane_ctx("pane-b"), "pane.list", json!({}))
        .await
        .unwrap();
    let l = e.ok("auth.list", json!({})).await;
    assert_eq!(l["revoked"], json!([{"pane": "pane-a"}]));
    assert_eq!(e.events("auth.token_revoked").len(), 1);
    assert!(e.audit_types().contains(&"auth.token_revoked".to_string()));
    // Survives a restart of the server (persisted), lifted by a restart of the pane.
    assert!(crate::auth::is_revoked(&e.server, "pane-a"));
    e.put_pane(pane("pane-a", None, Some(5151)));
    e.call(&pane_ctx("pane-a"), "pane.list", json!({}))
        .await
        .unwrap();
    assert_eq!(e.ok("auth.list", json!({})).await["revoked"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn elevation_needs_the_user_outside_the_pane() {
    let e = Env::new();
    // Full-scope callers have nothing to elevate.
    let r = e.call(&user(), "auth.elevate", json!({})).await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::InvalidParams);

    // The pane asks and waits; the user approves from outside.
    let srv = e.server.clone();
    let waiter = tokio::spawn(async move {
        dispatch(
            &srv,
            &pane_ctx("pane-a"),
            "auth.elevate",
            &json!({"reason": "install deps", "timeout_ms": 15000}),
        )
        .await
    });
    let mut request = None;
    for _ in 0..200 {
        let l = e.ok("auth.list", json!({})).await;
        if let Some(r) = l["pending"].as_array().and_then(|a| a.first()) {
            assert_eq!(r["pane"], "pane-a");
            assert_eq!(r["reason"], "install deps");
            request = r["request"].as_str().map(str::to_string);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let request = request.expect("pending elevation request");
    // The pane can't approve its own request, not even as another pane.
    for p in ["pane-a", "pane-b"] {
        let r = e
            .call(
                &pane_ctx(p),
                "auth.elevate.decide",
                json!({"request": request, "decision": "approve"}),
            )
            .await;
        assert_eq!(kind(&r.unwrap_err()), ErrorKind::PermissionDenied);
    }
    let d = e
        .ok(
            "auth.elevate.decide",
            json!({"request": request, "decision": "approve"}),
        )
        .await;
    assert_eq!(d["decision"], "approved");
    let got = waiter.await.unwrap().unwrap();
    assert!(
        crate::api_schema::validate_result("auth.elevate", &got).is_empty(),
        "{got}"
    );
    let token = got["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("vke_"));
    assert_eq!(got["ttl_s"], 600);
    // Decided twice → conflict.
    let r = e
        .call(
            &user(),
            "auth.elevate.decide",
            json!({"request": request, "decision": "deny"}),
        )
        .await;
    assert!(r.is_err());
    // The token gives full scope only from its own pane's tree (or outside every pane).
    let k = crate::auth::elevated_hello(&e.server, &token, Some("pane-a")).expect("valid");
    assert!(k.starts_with(crate::auth::ELEVATED_KIND));
    assert!(crate::auth::elevated_hello(&e.server, &token, Some("pane-b")).is_none());
    assert!(crate::auth::elevated_hello(&e.server, "vke_wrong", None).is_none());
    let elevated = Ctx {
        client_id: "c-el".into(),
        kind: k,
        pane_scope: None,
        remote: false,
    };
    e.call(&elevated, "policy.list", json!({})).await.unwrap();
    // ... but never to decide elevation requests.
    let r = e
        .call(
            &elevated,
            "auth.elevate.decide",
            json!({"request": "x", "decision": "approve"}),
        )
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::PermissionDenied);
    assert_eq!(
        e.ok("auth.list", json!({})).await["elevated"][0]["pane"],
        "pane-a"
    );
    // Revoking the pane's token ends its elevation too.
    e.ok("auth.revoke_token", json!({"pane": "pane-a"})).await;
    let r = e.call(&elevated, "policy.list", json!({})).await;
    assert!(
        r.unwrap_err().message.starts_with("elevation_expired"),
        "elevation ends with the revocation"
    );

    // Deny, and the timeout + resume path (pane-b).
    let pending = e
        .call(
            &pane_ctx("pane-b"),
            "auth.elevate",
            json!({"wait": false, "reason": "x"}),
        )
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    assert!(crate::api_schema::validate_result("auth.elevate", &pending).is_empty());
    let rid = pending["request"].as_str().unwrap().to_string();
    let r = e
        .call(
            &pane_ctx("pane-b"),
            "auth.elevate",
            json!({"request": rid, "timeout_ms": 50}),
        )
        .await;
    let err = r.unwrap_err();
    assert_eq!(kind(&err), ErrorKind::Timeout);
    assert_eq!(err.data.details["request"], rid.as_str());
    e.ok(
        "auth.elevate.decide",
        json!({"request": rid, "decision": "deny"}),
    )
    .await;
    let r = e
        .call(&pane_ctx("pane-b"), "auth.elevate", json!({"request": rid}))
        .await;
    assert!(r.unwrap_err().message.starts_with("elevation_denied"));
    let types = e.audit_types();
    for t in [
        "auth.elevate_requested",
        "auth.elevate_granted",
        "auth.elevate_denied",
    ] {
        assert!(types.iter().any(|x| x == t), "{t}: {types:?}");
    }
    // The user was notified out of band.
    assert!(e.server.with_core(|c| {
        c.notifications
            .iter()
            .any(|n| n.kind == "auth.elevate" && n.pane.as_deref() == Some("pane-a"))
    }));
}

fn harness_dirs(root: &Path) -> vk_agents::Dirs {
    vk_agents::Dirs {
        claude: root.join("claude"),
        codex: root.join("codex"),
        pi: root.join("pi"),
        omp: root.join("omp"),
        opencode: root.join("opencode"),
        gemini: root.join("gemini"),
    }
}

fn run(id: &str, pane: &str, harness: &str) -> AgentRun {
    AgentRun {
        id: id.into(),
        handle: id.into(),
        name: None,
        pane: pane.into(),
        harness: harness.into(),
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

#[tokio::test(flavor = "multi_thread")]
async fn integration_tampering_is_detected_at_agent_start_and_while_running() {
    use crate::integrity::{check, on_agent_start, poll, record_install};
    use vk_agents::Harness;
    let e = Env::new();
    let dirs = harness_dirs(&e.root());
    let h = Harness::Claude;
    assert_eq!(check(h, &dirs).status, "not_installed");
    let plan = vk_agents::plan_install(h, &dirs, Path::new("/usr/local/bin/vibeke")).unwrap();
    vk_agents::apply(&plan).unwrap();
    assert_eq!(check(h, &dirs).status, "unrecorded");
    record_install(h, &dirs).unwrap();
    assert_eq!(check(h, &dirs).status, "ok");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(crate::integrity::record_path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // Unrelated settings may change freely.
    let file = dirs.config_file(h);
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    v["theme"] = json!("dark");
    std::fs::write(&file, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    assert_eq!(check(h, &dirs).status, "ok");

    // A run starts: baseline taken, nothing reported.
    {
        let mut c = e.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.run(run("run-1", "pane-a", "claude"));
        e.server.commit(&mut c, tx).unwrap();
    }
    on_agent_start(&e.server, h, "run-1", &dirs);
    assert!(e.events("integration.tampered").is_empty());
    poll(&e.server, &dirs);
    assert!(e.events("integration.tampered").is_empty());

    // The agent removes Vibeke's hooks while running: reported once.
    v["hooks"] = json!({});
    std::fs::write(&file, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    poll(&e.server, &dirs);
    poll(&e.server, &dirs);
    let ev = e.events("integration.tampered");
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0].data["reason"], "changed_during_run");
    assert_eq!(ev[0].data["harness"], "claude");
    assert!(e.server.with_core(|c| {
        c.notifications
            .iter()
            .any(|n| n.kind == "integration_tampered" && n.urgency == "high")
    }));
    assert!(
        e.audit_types()
            .contains(&"integration.tampered".to_string())
    );

    // The next agent start compares with what install wrote: removed.
    assert_eq!(check(h, &dirs).status, "removed");
    on_agent_start(&e.server, h, "run-1", &dirs);
    let ev = e.events("integration.tampered");
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[1].data["reason"], "removed");
    let _ = crate::integrity::forget(h);

    // The API method (harness dirs from the environment; only the parameter check here, the
    // binary-level test drives it against temp harness dirs).
    let r = e
        .call(&user(), "integration.doctor", json!({"harness": "nope"}))
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::InvalidParams);
    let r = e
        .call(&pane_ctx("pane-a"), "integration.doctor", json!({}))
        .await;
    assert_eq!(kind(&r.unwrap_err()), ErrorKind::PermissionDenied);
}

#[test]
fn debug_bundle_has_no_secrets_and_scrubs_printed_tokens() {
    init_env();
    let d = tempfile::tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let paths = Paths {
        session: "t".into(),
        runtime: root.join("run"),
        state: root.join("state"),
    };
    let server = Server::new(
        paths.clone(),
        ServerOpts {
            session: "t".into(),
            machine: "testbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
        },
    )
    .unwrap();
    let token = server.token_for("pane-a");
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        server.persist_tokens(&mut tx);
        server.commit(&mut c, tx).unwrap();
    }
    drop(server);
    std::fs::write(
        paths.logs().join("server.log"),
        format!("started\nAuthorization: Bearer abcdefghijklmnop123\nprinted {token}\n"),
    )
    .unwrap();
    let out = root.join("b.tar");
    let b = crate::debug_bundle::build(
        &paths,
        &crate::debug_bundle::Options {
            out: Some(out.clone()),
            doctor: Some(format!("doctor ok; env TOKEN={token}")),
            panes: vec![("pane-a".into(), format!("$ echo {token}\n{token}\n"))],
            ..Default::default()
        },
    )
    .unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let all = String::from_utf8_lossy(&bytes);
    assert!(!all.contains(&token), "pane token leaked");
    assert!(!all.contains("abcdefghijklmnop123"));
    let members = crate::debug_bundle::read_tar(&bytes);
    let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
    for n in [
        "manifest.json",
        "version.json",
        "config.json",
        "db.json",
        "dirs.json",
        "audit.json",
        "integrations.json",
        "logs/server.log",
        "doctor.txt",
        "panes/pane-a.txt",
    ] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    assert!(!names.iter().any(|n| n.starts_with("scrollback/")));
    let db: Value =
        serde_json::from_slice(&members.iter().find(|(n, _)| n == "db.json").unwrap().1).unwrap();
    assert!(
        db["tables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["table"] == "kv")
    );
    let manifest: Value = serde_json::from_slice(&members[0].1).unwrap();
    assert!(
        manifest["excluded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x.as_str().unwrap().starts_with("scrollback"))
    );
    assert_eq!(b.path, out);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
