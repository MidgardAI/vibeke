//! In-process tests for the 2D coordinator pieces: staleness and live targets, `forget`,
//! the derived-data store, navigation candidates, remote sources, transient deltas and scope
//! checks. No provider is contacted and no harness runs; state lives in temp dirs.

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::sync::Once;
use vk_assist::consent::{self, Grant};

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-assist-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them for this module.
        unsafe {
            std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
        }
    });
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    server: Arc<Server>,
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
            gateway: None,
        };
        let server = Server::new(paths, opts).unwrap();
        Env {
            _dir: dir,
            root,
            server,
        }
    }
    fn dir(&self, name: &str) -> String {
        let p = self.root.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }
    fn commit(&self, f: impl FnOnce(&mut Tx)) {
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        f(&mut tx);
        self.server.commit(&mut c, tx).unwrap();
    }
    fn ws(&self, id: &str) -> Workspace {
        let w = Workspace {
            id: id.into(),
            handle: id.into(),
            name: Some(id.into()),
            auto_name: id.into(),
            root_path: self.dir(id),
            task: None,
            order: 1.0,
            branch: None,
        };
        self.commit(|tx| {
            tx.ws(w.clone());
        });
        w
    }
    fn pane(&self, id: &str, ws: &str) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: format!("tab-{ws}"),
            workspace: ws.into(),
            title: Some(format!("shell {id}")),
            auto_title: String::new(),
            cwd: Some("/tmp".into()),
            cols: 80,
            rows: 24,
            child_pid: None,
            fg_cmdline: vec!["zsh".into()],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: "user".into(),
            isolation: Default::default(),
            recovered: None,
            browser: None,
        };
        self.commit(|tx| {
            tx.pane(p);
        });
    }
    fn run(&self, id: &str, pane: &str, exec: Execution, name: &str) {
        let t = vk_store::now_ms();
        let r = AgentRun {
            id: id.into(),
            handle: id.into(),
            name: Some(name.into()),
            pane: pane.into(),
            harness: "claude".into(),
            harness_version: None,
            integration: "hooks".into(),
            harness_session_id: Some(format!("sess-{id}")),
            transcript_path: None,
            resume_argv: vec![],
            cwd: Some("/tmp".into()),
            model: None,
            task: None,
            execution: Facet {
                value: exec,
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
        };
        self.commit(|tx| {
            tx.run(r);
        });
    }
    fn interaction(&self, id: &str, run: &str, pane: &str, status: InteractionStatus) {
        let i = Interaction {
            id: id.into(),
            handle: id.into(),
            run: run.into(),
            pane: pane.into(),
            kind: InteractionKind::Approval,
            status,
            title: "Run rm -rf dist".into(),
            body_md: Some("Allow execution of rm?".into()),
            action: None,
            questions: vec![],
            plan_md: None,
            answer_channel: AnswerChannel::Keystrokes,
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
            answer_key: None,
            opened_at_ms: vk_store::now_ms(),
            answered_at_ms: None,
            picker: None,
        };
        self.commit(|tx| {
            tx.interaction(i);
        });
    }
}

fn request(id: &str, ws: &Workspace, state: ReqState) -> AssistRequest {
    serde_json::from_value(json!({
        "id": id, "record": "assist", "operation": "briefing", "state": state,
        "workspace": ws.id, "workspace_path": canonical(&ws.root_path),
        "inputs": {"workspace": ws.id}, "profile": "interactive", "connection": "c",
        "adapter": "ollama", "model": "m", "endpoint_host": "127.0.0.1:1", "machine": "testbox",
        "prompt_version": "v1", "sources": [], "omitted": [], "redactions": 0,
        "redaction_notice": "", "context_digest": "d", "preview_digest": "p", "payload_bytes": 1,
        "estimated_input_tokens": 1, "estimation_method": "x", "max_output_tokens": 1,
        "auto_sent": false, "usage": {}, "attempts": 0, "estimated_cost_usd": null,
        "finish_reason": null, "error": null, "output": null, "created_by": "c-1",
        "idempotency_key": null, "retry_of": null, "created_at_ms": vk_store::now_ms(),
        "queued_at_ms": null, "started_at_ms": null, "finished_at_ms": null,
    }))
    .unwrap()
}

fn resolved(endpoint: &str) -> Resolved {
    AssistConfig::from_json(json!({
        "connections": {"c": {"adapter": "ollama", "endpoint": endpoint}},
        "profiles": {"interactive": {"connection": "c", "model": "m-1"}},
    }))
    .unwrap()
    .resolve(None)
    .unwrap()
}

// ---- staleness and live targets -----------------------------------------------------------------------

#[test]
fn fingerprints_follow_the_live_state_of_what_a_result_is_about() {
    let e = Env::new();
    e.ws("w1");
    e.pane("p1", "w1");
    e.run("r1", "p1", Execution::Idle, "login agent");
    e.interaction("i1", "r1", "p1", InteractionStatus::Open);
    let s = &e.server;
    let i1 = stale::fingerprint(s, "interaction", "i1").unwrap();
    let r1 = stale::fingerprint(s, "run", "r1").unwrap();
    let w1 = stale::fingerprint(s, "workspace", "w1").unwrap();
    assert!(stale::fingerprint(s, "interaction", "nope").is_none());
    assert!(stale::fingerprint(s, "workspace", "nope").is_none());
    assert!(stale::fingerprint(s, "bogus", "x").is_none());
    // Unchanged state: the same fingerprints.
    assert_eq!(stale::fingerprint(s, "interaction", "i1").unwrap(), i1);
    assert_eq!(stale::fingerprint(s, "workspace", "w1").unwrap(), w1);
    // The interaction resolves: its own fingerprint and the workspace's both change.
    e.interaction("i1", "r1", "p1", InteractionStatus::ResolvedElsewhere);
    assert_ne!(stale::fingerprint(s, "interaction", "i1").unwrap(), i1);
    assert_ne!(stale::fingerprint(s, "workspace", "w1").unwrap(), w1);
    // The run changes state.
    e.run("r1", "p1", Execution::Working, "login agent");
    assert_ne!(stale::fingerprint(s, "run", "r1").unwrap(), r1);
    // Another workspace's objects never affect this workspace's fingerprint.
    let w1b = stale::fingerprint(s, "workspace", "w1").unwrap();
    e.ws("w2");
    e.pane("p2", "w2");
    e.run("r2", "p2", Execution::Working, "other");
    assert_eq!(stale::fingerprint(s, "workspace", "w1").unwrap(), w1b);
}

#[test]
fn a_finished_result_is_marked_stale_when_a_cited_object_changes_and_reports_live_state() {
    let e = Env::new();
    let w = e.ws("w1");
    e.pane("p1", "w1");
    e.run("r1", "p1", Execution::Idle, "login agent");
    e.interaction("i1", "r1", "p1", InteractionStatus::Open);
    let scope = json!({"workspace": "w1", "interaction": "i1", "run": "r1"});
    let mut r = request("as_1", &w, ReqState::Done);
    r.live = stale::capture(&e.server, Operation::DecisionCard, &scope);
    assert!(r.live.contains_key("interaction:i1") && r.live.contains_key("run:r1"));
    assert!(
        !r.live.contains_key("workspace:w1"),
        "a decision card is about its interaction, not the whole workspace"
    );
    r.output = Some(json!({
        "items": [{"text": "x", "targets": ["i1", "r1", "ghost"], "source_refs": []}],
        "interaction": "i1",
    }));
    let mut v = view(&r, true);
    stale::decorate(&e.server, &r, &mut v);
    assert_eq!(v["stale"], false, "{v}");
    let lt = v["live_targets"].as_array().unwrap();
    assert_eq!(lt.len(), 3);
    assert_eq!(lt[0]["id"], "i1");
    assert_eq!(lt[0]["kind"], "interaction");
    assert_eq!(lt[0]["status"], "open");
    assert_eq!(lt[0]["answerable"], true);
    assert_eq!(lt[1]["kind"], "run");
    assert_eq!(lt[1]["status"], "idle");
    assert_eq!(lt[2]["exists"], false, "an unknown id is reported as gone");
    // The interaction resolves: stale, naming which object changed, with the live status.
    e.interaction("i1", "r1", "p1", InteractionStatus::ResolvedElsewhere);
    let mut v = view(&r, true);
    stale::decorate(&e.server, &r, &mut v);
    assert_eq!(v["stale"], true);
    assert!(
        v["stale_targets"][0]
            .as_str()
            .unwrap()
            .contains("interaction i1 changed"),
        "{v}"
    );
    assert!(v["refresh_hint"].is_string());
    assert_ne!(v["live_targets"][0]["status"], "open");
    assert_eq!(v["live_targets"][0]["answerable"], false);
    // Only finished results are decorated.
    let pending = request("as_2", &w, ReqState::Running);
    let mut v = view(&pending, true);
    stale::decorate(&e.server, &pending, &mut v);
    assert!(v.get("stale").is_none());
}

// ---- forget ---------------------------------------------------------------------------------------------

#[test]
fn forget_removes_derived_records_and_cached_results_for_its_scope_only() {
    let e = Env::new();
    let a = e.ws("wa");
    let b = e.ws("wb");
    e.pane("pa", "wa");
    e.pane("pb", "wb");
    for (id, w) in [("as_a1", &a), ("as_a2", &a), ("as_b1", &b)] {
        save(&e.server, &request(id, w, ReqState::Done), None).unwrap();
    }
    let entry = |key: &str, w: &Workspace, origin: &str| vk_assist::cache::Entry {
        key: key.into(),
        operation: "briefing".into(),
        workspaces: vec![canonical(&w.root_path)],
        created_ms: vk_store::now_ms(),
        expires_ms: vk_store::now_ms() + 3_600_000,
        data: json!({"output": {}, "origin": origin}),
    };
    data::cache_put(&e.server, entry("ka", &a, "as_a1"));
    data::cache_put(&e.server, entry("kb", &b, "as_b1"));
    // A scope that names no workspace, pane, repo, time or "all" is not attributable.
    assert_eq!(forget_scope(&e.server, &json!({"session": "x"})), 0);
    assert_eq!(all_requests(&e.server).len(), 3);
    // The workspace scope removes that workspace's records and cache, nothing else.
    assert_eq!(forget_scope(&e.server, &json!({"workspace": "wa"})), 2);
    let left = all_requests(&e.server);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, "as_b1");
    assert!(data::cache_get(&e.server, "ka").is_none());
    assert!(data::cache_get(&e.server, "kb").is_some());
    // The event records counts and the reason, never content.
    let ev = e
        .server
        .with_core(|c| {
            c.store
                .events_after(0, 1000, &["assistant.purged".to_string()])
        })
        .unwrap();
    let last = ev.last().unwrap();
    assert_eq!(last.data["reason"], "forget");
    assert_eq!(last.data["count"], 2);
    assert_eq!(last.data["cache_entries"], 1);
    // A pane scope covers only requests about that pane's content (or workspace-wide ones).
    let mut about_pane = request("as_b2", &b, ReqState::Done);
    about_pane.inputs = json!({"pane": "pb"});
    let mut other = request("as_b3", &b, ReqState::Done);
    other.inputs = json!({"pane": "elsewhere"});
    save(&e.server, &about_pane, None).unwrap();
    save(&e.server, &other, None).unwrap();
    let n = forget_scope(&e.server, &json!({"pane": "pb"}));
    assert!(n >= 1);
    assert!(
        !all_requests(&e.server).iter().any(|r| r.id == "as_b2"),
        "the request about the forgotten pane went"
    );
    assert!(all_requests(&e.server).iter().any(|r| r.id == "as_b3"));
    // `all` removes the rest, `before` only what is older.
    let mut old = request("as_old", &b, ReqState::Done);
    old.created_at_ms = 1_000;
    save(&e.server, &old, None).unwrap();
    assert_eq!(forget_scope(&e.server, &json!({"before": 2_000})), 1);
    assert!(!all_requests(&e.server).iter().any(|r| r.id == "as_old"));
    forget_scope(&e.server, &json!({"all": true}));
    assert!(all_requests(&e.server).is_empty());
}

#[test]
fn forget_cancels_unfinished_requests_before_deleting_them() {
    let e = Env::new();
    let a = e.ws("wa");
    save(
        &e.server,
        &request("as_open", &a, ReqState::AwaitingConfirmation),
        None,
    )
    .unwrap();
    assert_eq!(forget_scope(&e.server, &json!({"workspace": "wa"})), 1);
    assert!(all_requests(&e.server).is_empty());
}

// ---- the derived-data store -----------------------------------------------------------------------------

#[test]
fn capability_records_cache_and_cursors_are_derived_data_in_the_store() {
    use vk_assist::capability::{Feature, Source, Support};
    let e = Env::new();
    let r = resolved("http://127.0.0.1:11434");
    // Observations are recorded per connection, endpoint and model.
    assert!(!data::capabilities(&e.server, &r).usable(Feature::Streaming));
    data::observe(
        &e.server,
        &r,
        Feature::Streaming,
        Support::Supported,
        "a stream finished",
    );
    let caps = data::capabilities(&e.server, &r);
    assert!(caps.usable(Feature::Streaming));
    assert_eq!(caps.streaming.source, Source::Observed);
    let moved = resolved("http://127.0.0.1:9");
    assert!(
        !data::capabilities(&e.server, &moved).usable(Feature::Streaming),
        "an endpoint change starts from unknown again"
    );
    // The model list cache is bound to the connection fingerprint.
    let list = vk_assist::models::bundled("c", vk_assist::config::Adapter::Anthropic);
    data::store_models(&e.server, &r.fingerprint, &list);
    assert!(data::cached_models(&e.server, "c", &r.fingerprint).is_some());
    assert!(data::cached_models(&e.server, "c", &moved.fingerprint).is_none());
    assert!(data::cached_models(&e.server, "other", &r.fingerprint).is_none());
    // The result cache: hits, expiry and purge by origin and by workspace.
    let entry = |key: &str, origin: &str, expires: i64| vk_assist::cache::Entry {
        key: key.into(),
        operation: "briefing".into(),
        workspaces: vec!["/w".into()],
        created_ms: 1,
        expires_ms: expires,
        data: json!({"output": {"x": 1}, "origin": origin}),
    };
    let far = vk_store::now_ms() + 3_600_000;
    data::cache_put(&e.server, entry("k1", "as_1", far));
    data::cache_put(&e.server, entry("k2", "as_2", far));
    data::cache_put(&e.server, entry("k3", "as_3", 5));
    assert_eq!(
        data::cache_get(&e.server, "k1").unwrap().data["output"]["x"],
        1
    );
    assert!(
        data::cache_get(&e.server, "k3").is_none(),
        "expired entries never hit"
    );
    assert_eq!(
        data::cache_purge(&e.server, data::CacheScope::Origin("as_1")),
        1
    );
    assert!(data::cache_get(&e.server, "k1").is_none());
    assert_eq!(
        data::cache_purge(&e.server, data::CacheScope::Workspace("/w")),
        2,
        "the remaining live entry and the expired one"
    );
    assert_eq!(data::cache_len(&e.server), 0);
    // Final review P2: a result completed before a purge is not inserted after it (the
    // purge generation moved); one completed after the purge is.
    let gen_at_done = state(&e.server)
        .cache_gen
        .load(std::sync::atomic::Ordering::SeqCst);
    data::cache_purge(&e.server, data::CacheScope::All);
    assert!(!data::cache_put_unless_purged(
        &e.server,
        gen_at_done,
        entry("k4", "as_4", far)
    ));
    assert!(
        data::cache_get(&e.server, "k4").is_none(),
        "purged result came back"
    );
    let now_gen = state(&e.server)
        .cache_gen
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(data::cache_put_unless_purged(
        &e.server,
        now_gen,
        entry("k5", "as_5", far)
    ));
    assert!(data::cache_get(&e.server, "k5").is_some());
    data::cache_purge(&e.server, data::CacheScope::All);
    // Cursors and the background planner's state survive.
    let mut cur = vk_assist::remote::Cursors::default();
    cur.0.insert("devbox/main".into(), "42".into());
    data::store_cursors(&e.server, &cur);
    assert_eq!(data::cursors(&e.server).0["devbox/main"], "42");
    let mut bg = vk_assist::background::BgState::default();
    bg.notified.insert("r1".into(), "digest".into());
    data::store_bg_state(&e.server, &bg);
    assert_eq!(data::bg_state(&e.server).notified["r1"], "digest");
}

// ---- access on read ---------------------------------------------------------------------------------------

#[test]
fn a_stored_result_is_readable_only_while_its_consent_stands() {
    let e = Env::new();
    let w = e.ws("wc");
    let mut r = request("as_c", &w, ReqState::Done);
    r.connection = "c-access-test".into();
    assert!(!access_ok(&r), "no grant at all");
    let path = consent_path();
    let grant = |fp: &str| Grant {
        workspace: canonical(&w.root_path),
        connection: "c-access-test".into(),
        fingerprint: fp.into(),
        adapter: "ollama".into(),
        endpoint_host: "h".into(),
        operations: vec![],
        classes: vec!["structured_state".into()],
        auto_send: vec![],
        granted_at_ms: 1,
        granted_by: "u".into(),
    };
    consent::grant(&path, grant("fp")).unwrap();
    assert!(
        access_ok(&r),
        "a current grant (the connection no longer resolves: grant alone)"
    );
    // A request that also drew from a remote workspace needs that grant too.
    r.other_workspace_paths = vec!["devbox:/home/u/repo".into()];
    assert!(!access_ok(&r));
    let mut remote = grant("fp");
    remote.workspace = "devbox:/home/u/repo".into();
    consent::grant(&path, remote).unwrap();
    assert!(access_ok(&r));
    consent::revoke(&path, &canonical(&w.root_path), Some("c-access-test")).unwrap();
    assert!(!access_ok(&r), "revoked: the output is withheld");
    consent::revoke(&path, "devbox:/home/u/repo", Some("c-access-test")).unwrap();
}

// ---- navigation candidates --------------------------------------------------------------------------------

fn target(e: &Env, w: Workspace) -> Target {
    let _ = e;
    Target {
        ws: w,
        others: vec![],
        bound_runs: vec![],
        excluded_runs: vec![],
        run: None,
        task: None,
        pane: None,
        turns: vec![],
        include_screen: false,
        query: Some("login".into()),
        interaction: None,
    }
}

#[test]
fn navigation_offers_only_the_consented_workspaces_objects() {
    let e = Env::new();
    let w1 = e.ws("w1");
    e.ws("w2");
    e.pane("p1", "w1");
    e.pane("p1b", "w1");
    e.pane("p2", "w2");
    e.run("r1", "p1", Execution::Idle, "login agent");
    e.run("r2", "p2", Execution::Idle, "other workspace agent");
    e.interaction("i1", "r1", "p1", InteractionStatus::Open);
    e.interaction("i2", "r2", "p2", InteractionStatus::Open);
    let c = gather_ext::candidates(&e.server, &target(&e, w1));
    let ids: Vec<&str> = c.iter().map(|x| x.id.as_str()).collect();
    assert!(ids.contains(&"r1") && ids.contains(&"i1"));
    assert!(ids.contains(&"p1b"), "a pane without a run is offered");
    assert!(
        !ids.contains(&"p1"),
        "a pane that has a run is covered by the run"
    );
    assert!(
        !ids.contains(&"r2") && !ids.contains(&"i2") && !ids.contains(&"p2"),
        "another workspace's objects are never offered: {ids:?}"
    );
    let run = c.iter().find(|x| x.id == "r1").unwrap();
    assert_eq!(run.kind, "run");
    assert!(run.label.contains("login agent"));
    // Resolved interactions are not candidates.
    e.interaction("i1", "r1", "p1", InteractionStatus::ResolvedElsewhere);
    let w1 = e.server.with_core(|c| c.ws("w1").cloned()).unwrap();
    let c = gather_ext::candidates(&e.server, &target(&e, w1));
    assert!(!c.iter().any(|x| x.id == "i1"));
}

// ---- remote sources -------------------------------------------------------------------------------------------

fn grant_for(workspace: &str, r: &Resolved) -> Grant {
    Grant {
        workspace: workspace.into(),
        connection: r.connection_id.clone(),
        fingerprint: r.fingerprint.clone(),
        adapter: "ollama".into(),
        endpoint_host: "h".into(),
        operations: vec![],
        classes: vec!["structured_state".into(), "selected_text".into()],
        auto_send: vec![],
        granted_at_ms: 1,
        granted_by: "u".into(),
    }
}

#[test]
fn remote_sources_are_gated_by_config_op_and_per_workspace_consent() {
    let e = Env::new();
    let r = resolved("http://127.0.0.1:11434");
    let mut cfg = AssistConfig::default();
    let now = vk_store::now_ms();
    let params = |cursor: &str, from: &str| {
        json!({"remote_sources": [
            {"machine": "devbox", "session": "main", "workspace": "/home/u/repo", "status": "ok",
             "cursor": cursor, "from_cursor": from, "observed_at_ms": now,
             "items": [{"kind": "run_state", "object": {"run": "r1"}, "label": "remote agent", "text": "running tests"}]},
            {"machine": "lab", "session": "main", "workspace": "/srv/x", "status": "offline"},
        ]})
    };
    let classes = ["structured_state"];
    let grants = vec![grant_for("devbox:/home/u/repo", &r)];
    let call = |cfg: &AssistConfig, p: &Value, op: Operation, g: &[Grant]| {
        gather_ext::remote_prepare(&e.server, cfg, p, &r, op, &classes, g)
    };
    // No remote_sources parameter: nothing to do.
    assert!(
        call(&cfg, &json!({}), Operation::Briefing, &grants)
            .unwrap()
            .is_none()
    );
    // Off by default.
    let err = call(&cfg, &params("5", "0"), Operation::Briefing, &grants)
        .err()
        .unwrap();
    assert_eq!(err.data.details["reason"], "remote_sources_off");
    cfg.remote_sources = true;
    // Only the workspace-level operations take remote sources.
    assert!(call(&cfg, &params("5", "0"), Operation::Handoff, &grants).is_err());
    // A local-style grant list never covers a remote workspace.
    let none: Vec<Grant> = vec![grant_for("/home/u/repo", &r)];
    let err = call(&cfg, &params("5", "0"), Operation::Briefing, &none)
        .err()
        .unwrap();
    assert_eq!(err.data.details["reason"], "consent_required");
    assert!(err.message.contains("devbox:/home/u/repo"));
    // With the grant: content from the live source only, identities for dispatch re-checks,
    // coverage notes naming the offline machine.
    let prep = call(&cfg, &params("5", "0"), Operation::Briefing, &grants)
        .unwrap()
        .unwrap();
    assert_eq!(prep.identities, vec!["devbox:/home/u/repo".to_string()]);
    assert_eq!(prep.inputs.len(), 1);
    assert!(prep.inputs[0].label.contains("[devbox/main]"));
    assert_eq!(prep.inputs[0].object["machine"], "devbox");
    assert_eq!(prep.inputs[0].object["cursor"], "5");
    assert!(prep.notes.iter().any(|n| n.contains("lab/main is offline")));
    assert!(
        prep.notes
            .iter()
            .any(|n| n.contains("coordinator machine: testbox"))
    );
    // The cursor was recorded by the caller; a later request that starts past it has a gap.
    data::store_cursors(&e.server, &prep.cursors);
    let prep = call(&cfg, &params("20", "9"), Operation::Briefing, &grants)
        .unwrap()
        .unwrap();
    assert!(
        prep.notes
            .iter()
            .any(|n| n.contains("between cursor 5 and 9 are missing")),
        "{:?}",
        prep.notes
    );
    // Malformed input is an invalid-params error, not a panic.
    assert!(
        call(
            &cfg,
            &json!({"remote_sources": "x"}),
            Operation::Briefing,
            &grants
        )
        .is_err()
    );
}

// ---- transient deltas and scope -----------------------------------------------------------------------------

#[test]
fn deltas_are_transient_sanitized_notifications_that_are_not_history() {
    let e = Env::new();
    let mut rx = e.server.events.subscribe();
    emit_delta(&e.server, "as_1", 3, "he\u{1b}[2Jllo \u{202E}");
    let ev = rx.try_recv().unwrap();
    assert_eq!(ev.seq, 0);
    assert_eq!(ev.tier, "transient");
    assert_eq!(ev.kind, "assistant.delta");
    assert_eq!(ev.subject["assistant_request"], "as_1");
    assert_eq!(ev.data["seq"], 3);
    let text = ev.data["text"].as_str().unwrap();
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{202E}'),
        "{text:?}"
    );
    assert!(text.starts_with("he") && text.contains("llo"));
    // Not in the durable log.
    let logged = e
        .server
        .with_core(|c| {
            c.store
                .events_after(0, 100, &["assistant.delta".to_string()])
        })
        .unwrap();
    assert!(logged.is_empty());
}

#[tokio::test]
async fn the_new_methods_are_refused_to_pane_scoped_callers() {
    let e = Env::new();
    let ctx = Ctx {
        client_id: "c-pane".into(),
        kind: "pane".into(),
        pane_scope: Some("p1".into()),
        remote: false,
    };
    for m in [
        "assistant.models",
        "assistant.test",
        "assistant.background",
        "assistant.generate",
    ] {
        let r = api(&e.server, &ctx, m, &json!({})).await.expect("handled");
        let err = r.unwrap_err();
        assert!(err.kind_is(ErrorKind::PermissionDenied), "{m}");
        assert_eq!(err.data.details["scope"], "pane");
    }
    // Every method of the table is classified forbidden for pane scope.
    for (m, _) in METHODS {
        assert_eq!(
            crate::api::pane_scope_of(m),
            crate::api::PaneScope::Forbidden,
            "{m}"
        );
    }
}

#[test]
fn views_show_how_many_clients_share_a_request_but_not_who() {
    let e = Env::new();
    let w = e.ws("wv");
    let mut r = request("as_v", &w, ReqState::AwaitingConfirmation);
    r.consumers = vec!["c-1".into(), "c-2".into()];
    r.coalesce_key = Some("briefing:wv:::".into());
    let v = view(&r, false);
    assert_eq!(v["consumer_count"], 2);
    assert!(v.get("consumers").is_none());
    assert!(!v.to_string().contains("c-2"));
}

#[test]
fn old_records_without_the_new_fields_still_load() {
    let e = Env::new();
    let w = e.ws("wo");
    let r = request("as_old_shape", &w, ReqState::Done);
    assert_eq!(r.priority, "interactive");
    assert!(r.consumers.is_empty() && r.live.is_empty() && !r.cached && !r.streamed);
    assert!(r.coalesce_key.is_none() && r.structured.is_none() && !r.repaired);
}
