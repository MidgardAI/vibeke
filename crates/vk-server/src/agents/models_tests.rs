//! In-process tests for `agent.commands`, `agent.models`, `agent.set_model` and the extension
//! control channel (`adapter.control`), against a real `Server` with runs created directly. The
//! extension side is played by the test: it polls with the pane's scope and replies. Headless
//! protocol paths are covered in `headless/tests.rs`.

use super::*;
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use std::sync::Once;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Same values as the other in-process test modules: one root per binary, no real config.
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

struct Env {
    server: Arc<Server>,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
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
    Env {
        server: Server::new(paths, opts).unwrap(),
        _dir: dir,
    }
}

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

fn pane_ctx(pane: &str) -> Ctx {
    Ctx {
        client_id: format!("c-{pane}"),
        kind: "agent".into(),
        pane_scope: Some(pane.into()),
        remote: false,
    }
}

/// A run on its own (unique) pane.
fn add_run(e: &Env, harness: &str, integration: &str) -> AgentRun {
    let mut r = harness_tests_run();
    let id = ulid();
    r.id = format!("r-{id}");
    r.handle = r.id.clone();
    r.pane = format!("p-{id}");
    r.harness = harness.into();
    r.integration = integration.into();
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.run(r.clone());
    e.server.commit(&mut c, tx).unwrap();
    r
}

async fn call(e: &Env, ctx: &Ctx, method: &str, p: Value) -> R {
    super::api(&e.server, ctx, method, &p).await.expect(method)
}

/// The extension's poll: parks until a request arrives (`reply`: the previous result).
async fn poll(e: &Env, pane: &str, reply: Option<Value>, wait_ms: u64) -> Value {
    let mut p =
        json!({"harness": "pi", "ops": ["models", "set_model", "commands"], "wait_ms": wait_ms});
    if let Some(r) = reply {
        p["reply"] = r;
    }
    call(e, &pane_ctx(pane), "adapter.control", p)
        .await
        .unwrap()
}

/// Park a poll in the background and wait until the channel counts as reachable.
async fn parked(e: &Env, pane: &str) -> tokio::task::JoinHandle<Value> {
    let server = e.server.clone();
    let pane_s = pane.to_string();
    let h = tokio::spawn(async move {
        let p =
            json!({"harness": "pi", "ops": ["models", "set_model", "commands"], "wait_ms": 5000});
        super::api(&server, &pane_ctx(&pane_s), "adapter.control", &p)
            .await
            .unwrap()
            .unwrap()
    });
    for _ in 0..200 {
        if models::reachable(pane) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    h
}

#[tokio::test]
async fn commands_come_from_the_harness_catalog() {
    let e = env();
    let claude = add_run(&e, "claude", "hooks");
    let v = call(&e, &user(), "agent.commands", json!({"target": claude.id}))
        .await
        .unwrap();
    assert_eq!(v["source"], "catalog");
    let cmds = v["commands"].as_array().unwrap();
    let get = |n: &str| cmds.iter().find(|c| c["name"] == n).cloned().unwrap();
    assert_eq!(
        get("model"),
        json!({"name": "model", "description": "Set the AI model for Claude Code", "takes_arg": true, "opens_picker": true, "dangerous": false})
    );
    assert_eq!(get("clear")["dangerous"], true);
    assert_eq!(get("effort")["opens_picker"], true);

    let codex = add_run(&e, "codex", "hooks");
    let v = call(
        &e,
        &user(),
        "agent.commands",
        json!({"target": codex.handle}),
    )
    .await
    .unwrap();
    let names: Vec<&str> = v["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"permissions") && names.contains(&"review"));

    // A harness without a catalog: an empty list, not an error.
    let aider = add_run(&e, "aider", "screen");
    let v = call(&e, &user(), "agent.commands", json!({"target": aider.id}))
        .await
        .unwrap();
    assert_eq!(v, json!({"commands": [], "source": "catalog"}));
}

#[tokio::test]
async fn models_are_unsupported_without_a_structured_way() {
    let e = env();
    let claude = add_run(&e, "claude", "hooks");
    for (m, p) in [
        ("agent.models", json!({"target": claude.id})),
        (
            "agent.set_model",
            json!({"target": claude.id, "model": "opus"}),
        ),
    ] {
        let err = call(&e, &user(), m, p).await.unwrap_err();
        assert_eq!(err.data.kind, "unsupported", "{m}");
        assert_eq!(err.data.details["fallback"], "/model");
    }
    // Interactive Codex: its app-server is embedded in the TUI process, out of reach.
    let codex = add_run(&e, "codex", "hooks");
    let err = call(&e, &user(), "agent.models", json!({"target": codex.id}))
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "unsupported");
    // Interactive pi whose extension does not poll (old extension, or none).
    let pi = add_run(&e, "pi", "extension");
    let err = call(&e, &user(), "agent.models", json!({"target": pi.id}))
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "unsupported");
    assert_eq!(err.data.details["reason"], "extension_unavailable");
    // A headless run whose process is gone.
    let hl = add_run(&e, "codex", "headless:app-server");
    let err = call(&e, &user(), "agent.models", json!({"target": hl.id}))
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "conflict");
}

#[tokio::test]
async fn set_model_validates_params_and_scope() {
    let e = env();
    let pi = add_run(&e, "omp", "extension");
    let poller = parked(&e, &pi.pane).await;
    for p in [
        json!({"target": pi.id}),
        json!({"target": pi.id, "model": "two words"}),
        json!({"target": pi.id, "model": "a/b", "scope": "forever"}),
        // omp keeps a switch to the session: no structured default.
        json!({"target": pi.id, "model": "a/b", "scope": "default"}),
    ] {
        let err = call(&e, &user(), "agent.set_model", p.clone())
            .await
            .unwrap_err();
        assert_eq!(err.data.kind, "invalid_params", "{p}");
    }
    poller.abort();
}

#[tokio::test]
async fn extension_channel_lists_and_switches_models() {
    let e = env();
    let run = add_run(&e, "pi", "extension");
    let pane = run.pane.clone();

    // agent.models → the parked poll gets the request → the reply answers the caller.
    let poller = parked(&e, &pane).await;
    let server = e.server.clone();
    let rid = run.id.clone();
    let caller = tokio::spawn(async move {
        super::api(&server, &user(), "agent.models", &json!({"target": rid}))
            .await
            .unwrap()
    });
    let req = poller.await.unwrap()["request"].clone();
    assert_eq!(req["op"], "models");
    let id = req["id"].as_str().unwrap().to_string();
    let next = poll(
        &e,
        &pane,
        Some(json!({"id": id, "ok": true, "result": {"models": [
            {"id": "anthropic/m-1", "label": "M 1", "description": "anthropic", "current": true},
            {"id": "openai/m-2", "current": false},
            {"label": "no id: dropped"}
        ]}})),
        20,
    )
    .await;
    assert_eq!(next, json!({"request": null}));
    assert_eq!(
        caller.await.unwrap().unwrap(),
        json!({"source": "protocol", "models": [
            {"id": "anthropic/m-1", "label": "M 1", "description": "anthropic", "current": true},
            {"id": "openai/m-2", "label": "openai/m-2", "current": false}
        ]})
    );

    // agent.set_model: the request carries model and scope; the run's model follows.
    let poller = parked(&e, &pane).await;
    let server = e.server.clone();
    let rid = run.id.clone();
    let caller = tokio::spawn(async move {
        super::api(
            &server,
            &user(),
            "agent.set_model",
            &json!({"target": rid, "model": "openai/m-2"}),
        )
        .await
        .unwrap()
    });
    let req = poller.await.unwrap()["request"].clone();
    assert_eq!(req["op"], "set_model");
    assert_eq!(
        req["params"],
        json!({"model": "openai/m-2", "scope": "session"})
    );
    poll(
        &e,
        &pane,
        Some(json!({"id": req["id"], "ok": true, "result": {"model": "openai/m-2", "default_changed": true}})),
        20,
    )
    .await;
    let v = caller.await.unwrap().unwrap();
    assert_eq!(v["default_changed"], true);
    assert_eq!(v["run"]["model"], "openai/m-2");
    assert_eq!(
        e.server
            .with_core(|c| c.run(&run.id).and_then(|r| r.model.clone())),
        Some("openai/m-2".into())
    );

    // A refusal is a conflict carrying the extension's message.
    let poller = parked(&e, &pane).await;
    let server = e.server.clone();
    let rid = run.id.clone();
    let caller = tokio::spawn(async move {
        super::api(
            &server,
            &user(),
            "agent.set_model",
            &json!({"target": rid, "model": "x/y", "scope": "default"}),
        )
        .await
        .unwrap()
    });
    let req = poller.await.unwrap()["request"].clone();
    assert_eq!(req["params"]["scope"], "default");
    poll(
        &e,
        &pane,
        Some(json!({"id": req["id"], "ok": false, "error": "unknown model: x/y"})),
        20,
    )
    .await;
    let err = caller.await.unwrap().unwrap_err();
    assert_eq!(err.data.kind, "conflict");
    assert!(err.message.contains("unknown model"));

    // agent.commands merges the session's own commands into the catalog.
    let poller = parked(&e, &pane).await;
    let server = e.server.clone();
    let rid = run.id.clone();
    let caller = tokio::spawn(async move {
        super::api(&server, &user(), "agent.commands", &json!({"target": rid}))
            .await
            .unwrap()
    });
    let req = poller.await.unwrap()["request"].clone();
    assert_eq!(req["op"], "commands");
    poll(
        &e,
        &pane,
        Some(json!({"id": req["id"], "ok": true, "result": {"commands": [
            {"name": "review-pr", "description": "Prompt template"},
            {"name": "model", "description": "duplicate of a built-in: skipped"},
            {"name": "bad name"}
        ]}})),
        20,
    )
    .await;
    let v = caller.await.unwrap().unwrap();
    assert_eq!(v["source"], "protocol");
    let cmds = v["commands"].as_array().unwrap();
    assert_eq!(cmds.iter().filter(|c| c["name"] == "model").count(), 1);
    assert!(
        cmds.iter()
            .any(|c| c["name"] == "review-pr" && c["takes_arg"] == true)
    );
    assert!(!cmds.iter().any(|c| c["name"] == "bad name"));
}

#[tokio::test]
async fn control_channel_is_pane_bound() {
    let e = env();
    // No pane token: refused.
    let err = call(&e, &user(), "adapter.control", json!({"ops": ["models"]}))
        .await
        .unwrap_err();
    assert_eq!(err.data.kind, "permission_denied");

    // Another pane cannot answer this pane's request.
    let run = add_run(&e, "pi", "extension");
    let poller = parked(&e, &run.pane).await;
    let server = e.server.clone();
    let rid = run.id.clone();
    let caller = tokio::spawn(async move {
        super::api(&server, &user(), "agent.models", &json!({"target": rid}))
            .await
            .unwrap()
    });
    let req = poller.await.unwrap()["request"].clone();
    poll(
        &e,
        "p-intruder",
        Some(json!({"id": req["id"], "ok": true, "result": {"models": []}})),
        10,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!caller.is_finished(), "a foreign reply must not answer");
    poll(
        &e,
        &run.pane,
        Some(json!({"id": req["id"], "ok": true, "result": {"models": []}})),
        10,
    )
    .await;
    assert_eq!(
        caller.await.unwrap().unwrap(),
        json!({"models": [], "source": "protocol"})
    );

    // A pane-scoped caller may switch only its own run's model.
    let other = add_run(&e, "pi", "extension");
    let p = json!({"target": other.id, "model": "a/b"});
    assert!(crate::api::authorize(&e.server, &pane_ctx(&run.pane), "agent.set_model", &p).is_err());
    assert!(crate::api::authorize(&e.server, &pane_ctx(&run.pane), "agent.models", &p).is_ok());
}
