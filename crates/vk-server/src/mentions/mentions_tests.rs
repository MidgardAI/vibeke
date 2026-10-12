//! In-process tests for mention names, messages, the envelope and the hook output merge
//! (spec 19). No harness runs: runs are put into the model directly.

use super::delivery::{envelope, parse_envelope, summary};
use super::hooks::{
    merge_outputs, merge_reply, sanitize_envelope, sanitize_payload, strip_leading,
};
use super::*;
use crate::ServerOpts;
use crate::agents::harness::Harness;
use crate::paths::Paths;
use std::sync::Once;
use vk_proto::model::{Execution, Workspace};

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-mention-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
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
    _dir: tempfile::TempDir,
    server: Arc<Server>,
}

impl Env {
    /// Panes `a` and `b` in workspace `wa`, pane `c` in workspace `wb`.
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
        let e = Env { _dir: dir, server };
        e.put_ws("wa");
        e.put_ws("wb");
        e.put_pane("a", "wa");
        e.put_pane("b", "wa");
        e.put_pane("c", "wb");
        e
    }
    fn put_ws(&self, id: &str) {
        let w = Workspace {
            id: id.into(),
            handle: id.into(),
            name: Some(id.into()),
            auto_name: id.into(),
            root_path: "/tmp".into(),
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
            title: Some(format!("{id} pane")),
            auto_title: String::new(),
            cwd: Some("/tmp/project".into()),
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
            browser: None,
        };
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(p);
        self.server.commit(&mut c, tx).unwrap();
    }
    fn put_run(&self, pane: &str, harness: &str, state: Execution) {
        let mut r = crate::agents::harness_tests_run();
        r.id = format!("run-{pane}");
        r.handle = format!("a-{pane}");
        r.pane = pane.into();
        r.harness = harness.into();
        r.execution.value = state;
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.run(r);
        self.server.commit(&mut c, tx).unwrap();
    }
    async fn call(&self, ctx: &Ctx, method: &str, p: Value) -> R {
        crate::api::authorize(&self.server, ctx, method, &p)?;
        api(&self.server, ctx, method, &p)
            .await
            .expect("a mention method")
    }
    async fn ok(&self, ctx: &Ctx, method: &str, p: Value) -> Value {
        match self.call(ctx, method, p.clone()).await {
            Ok(v) => v,
            Err(e) => panic!("{method} {p}: {e:?}"),
        }
    }
    fn delivery(&self, id: &str) -> DeliveryRec {
        self.server
            .with_core(|c| c.store.get::<DeliveryRec>(K_DELIVERY, id).unwrap())
            .unwrap()
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

fn kind(r: R) -> (String, String) {
    let e = r.expect_err("expected an error");
    let reason = e
        .data
        .details
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (e.data.kind, reason)
}

#[test]
fn leading_mentions_are_stripped_with_their_commas() {
    let names = vec!["a".to_string(), "b-2".to_string()];
    assert_eq!(strip_leading("  @a, @b-2 fix this", &names), "fix this");
    assert_eq!(strip_leading("@a: fix", &names), ": fix");
    assert_eq!(strip_leading("@zed fix", &names), "@zed fix");
    assert_eq!(strip_leading("fix @a", &names), "fix @a");
}

#[test]
fn hook_outputs_merge_contexts_and_a_block_wins() {
    let ctx = |t: &str| json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": t}});
    let both = merge_outputs(ctx("collision"), ctx("mention"));
    assert_eq!(
        both["hookSpecificOutput"]["additionalContext"],
        "collision\n\nmention"
    );
    let block = json!({"decision": "block", "reason": "sent"});
    assert_eq!(merge_outputs(ctx("x"), block.clone()), block);
    let deny = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny"}});
    assert_eq!(merge_outputs(deny.clone(), ctx("y")), deny);
    // The reply wrapper: nothing to add keeps `{}`; one side only passes through.
    assert_eq!(merge_reply(json!({}), None), json!({}));
    assert_eq!(
        merge_reply(json!({}), Some(ctx("m"))),
        json!({"hook_output": ctx("m")})
    );
    assert_eq!(
        merge_reply(json!({"hook_output": ctx("c")}), Some(ctx("m")))["hook_output"]["hookSpecificOutput"]
            ["additionalContext"],
        "c\n\nm"
    );
}

#[test]
fn envelope_round_trips_and_keeps_one_line() {
    let m = Message {
        id: "m_01abc".into(),
        thread: "m_01abc".into(),
        workspace: "wa".into(),
        from: Author::Agent {
            pane: "a".into(),
            name: Some("coder".into()),
        },
        to: vec!["reviewer".into()],
        body: "\n  Review the [auth] change\nsecond line".into(),
        in_reply_to: None,
        hop: 0,
        sent_at_ms: 0,
    };
    let nonce = new_nonce();
    assert_eq!(nonce.len(), 12);
    assert!(
        nonce
            .chars()
            .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c))
    );
    let line = envelope("@coder", &m, &nonce);
    assert!(!line.contains('\n'), "{line}");
    assert!(line.starts_with("↪ vibeke message m_01abc from @coder · Review the  auth  change"));
    assert_eq!(
        parse_envelope(&line),
        Some(("m_01abc".to_string(), Some(nonce)))
    );
    // Looks like one, carries no nonce.
    assert_eq!(
        parse_envelope("↪ vibeke message m_x from @coder · hi"),
        Some(("m_x".to_string(), None))
    );
    assert_eq!(parse_envelope("please review"), None);
    let long = "x".repeat(100);
    assert_eq!(summary(&long).chars().count(), 61);
}

#[tokio::test(flavor = "multi_thread")]
async fn naming_rules_conflicts_and_pane_scope() {
    let e = Env::new();
    let u = user();
    let m = e
        .ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    assert_eq!(m["name"], "reviewer");
    assert_eq!(m["kind"], "agent");
    assert_eq!(m["delivery"], true);
    // Invalid and reserved names.
    for bad in ["Reviewer", "all", "current", "9lives", ""] {
        let r = e
            .call(&u, "mention.set", json!({"pane": "a", "name": bad}))
            .await;
        assert_eq!(kind(r).0, "invalid_params", "{bad}");
    }
    // Unique per workspace, free in another.
    let r = e
        .call(&u, "mention.set", json!({"pane": "a", "name": "reviewer"}))
        .await;
    assert_eq!(kind(r), ("conflict".into(), "name_taken".into()));
    e.ok(&u, "mention.set", json!({"pane": "c", "name": "reviewer"}))
        .await;
    // An agent names only its own pane.
    let r = e
        .call(
            &pane_ctx("a"),
            "mention.set",
            json!({"pane": "b", "name": "x"}),
        )
        .await;
    assert_eq!(kind(r).0, "permission_denied");
    let mine = e
        .ok(&pane_ctx("a"), "mention.set", json!({"name": "coder"}))
        .await;
    assert_eq!(mine["pane"], "a");
    // The model carries the names; `@name` resolves in the caller's workspace.
    assert_eq!(e.server.with_core(|c| c.model.mentions.len()), 3);
    let p = crate::api::resolve_pane(&e.server, &pane_ctx("a"), Some("@reviewer")).unwrap();
    assert_eq!(p.id, "b");
    let amb = crate::api::resolve_pane(&e.server, &u, Some("@reviewer")).unwrap_err();
    assert!(amb.kind_is(ErrorKind::AmbiguousTarget), "{amb:?}");
    // A suggestion avoids taken names.
    e.put_pane("d", "wa");
    let s = e.ok(&u, "mention.suggest", json!({"pane": "d"})).await;
    assert_eq!(s["named"], false);
    assert_eq!(s["name"], "d-pane");
    // Clearing, and closing a pane, remove the name.
    e.ok(&pane_ctx("a"), "mention.clear", json!({})).await;
    assert!(e.server.with_core(|c| c.model.mention_of("a").is_none()));
    {
        let mut c = e.server.core.lock().unwrap();
        let pane = c.pane("b").cloned().unwrap();
        let mut tx = Tx::new();
        tx.close_pane(&pane);
        e.server.commit(&mut c, tx).unwrap();
        assert!(c.model.mention_of("b").is_none());
        let stored: Option<vm::MentionName> = c.store.get(crate::core::K_MENTION, "b").unwrap();
        assert!(stored.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn send_reply_complete_and_refusals() {
    let e = Env::new();
    let u = user();
    e.put_run("a", "claude", Execution::Idle);
    e.put_run("b", "codex", Execution::Idle);
    e.ok(&u, "mention.set", json!({"pane": "a", "name": "coder"}))
        .await;
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    e.ok(&u, "mention.set", json!({"pane": "c", "name": "other"}))
        .await;
    let a = pane_ctx("a");
    // Unknown names list the known ones; another workspace is refused; so is yourself.
    let r = e
        .call(&a, "message.send", json!({"to": ["@nobody"], "body": "hi"}))
        .await;
    let err = r.unwrap_err();
    assert!(err.kind_is(ErrorKind::NotFound));
    assert_eq!(err.data.details["unknown"], json!(["@nobody"]));
    let r = e
        .call(
            &a,
            "message.send",
            json!({"to": ["other"], "body": "hi", "workspace": "wb"}),
        )
        .await;
    assert_eq!(kind(r).0, "permission_denied");
    let r = e
        .call(&a, "message.send", json!({"to": ["coder"], "body": "hi"}))
        .await;
    assert_eq!(kind(r).0, "invalid_params");
    let r = e
        .call(
            &a,
            "message.send",
            json!({"to": ["reviewer"], "body": "  "}),
        )
        .await;
    assert_eq!(kind(r).0, "invalid_params");
    let big = "x".repeat(MAX_BODY + 1);
    let r = e
        .call(&a, "message.send", json!({"to": ["reviewer"], "body": big}))
        .await;
    assert_eq!(kind(r).0, "invalid_params");
    // A send: one queued delivery, a redacted body, the nonce never in the result.
    let secret = "token ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let sent = e
        .ok(
            &a,
            "message.send",
            json!({"to": ["@reviewer"], "body": format!("please review\n{secret}")}),
        )
        .await;
    let mid = sent["message"]["id"].as_str().unwrap().to_string();
    assert!(mid.starts_with("m_"));
    assert_eq!(sent["message"]["from"]["kind"], "agent");
    assert_eq!(sent["message"]["from"]["name"], "coder");
    assert!(
        !sent["message"]["body"]
            .as_str()
            .unwrap()
            .contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        "{sent}"
    );
    let d = &sent["deliveries"][0];
    assert_eq!(d["state"], "queued");
    assert!(d.get("nonce").is_none());
    let did = d["id"].as_str().unwrap().to_string();
    // Another pane cannot read it; the recipient can.
    let r = e
        .call(&pane_ctx("c"), "message.get", json!({"id": mid}))
        .await;
    assert_eq!(kind(r).0, "permission_denied");
    let got = e
        .ok(&pane_ctx("b"), "message.get", json!({"id": mid}))
        .await;
    assert_eq!(got["deliveries"][0]["id"], did.as_str());
    let inbox = e
        .ok(
            &pane_ctx("b"),
            "message.list",
            json!({"direction": "in", "open_only": true}),
        )
        .await;
    assert_eq!(inbox["items"].as_array().unwrap().len(), 1);
    // Only the recipient replies; the reply goes back to the author's pane.
    let r = e
        .call(
            &pane_ctx("c"),
            "message.reply",
            json!({"message": mid, "body": "x"}),
        )
        .await;
    assert_eq!(kind(r).0, "permission_denied");
    let reply = e
        .ok(
            &pane_ctx("b"),
            "message.reply",
            json!({"message": mid, "body": "looks good"}),
        )
        .await;
    assert_eq!(reply["message"]["in_reply_to"], mid.as_str());
    assert_eq!(reply["message"]["hop"], 1);
    assert_eq!(reply["deliveries"][0]["pane"], "a");
    assert_eq!(reply["deliveries"][0]["expects_reply"], false);
    let answered = e.delivery(&did);
    assert_eq!(answered.delivery.state, DeliveryState::Replied);
    // Complete, cancel and retry.
    let rid = reply["deliveries"][0]["id"].as_str().unwrap().to_string();
    let rmid = reply["message"]["id"].as_str().unwrap().to_string();
    e.ok(&a, "message.complete", json!({"message": rmid})).await;
    assert_eq!(e.delivery(&rid).delivery.state, DeliveryState::Completed);
    let again = e
        .ok(
            &a,
            "message.send",
            json!({"to": ["reviewer"], "body": "one more"}),
        )
        .await;
    let d2 = again["deliveries"][0]["id"].as_str().unwrap().to_string();
    let r = e
        .call(&pane_ctx("b"), "message.cancel", json!({"delivery": d2}))
        .await;
    assert_eq!(kind(r).0, "permission_denied");
    let r = e.call(&a, "message.retry", json!({"delivery": d2})).await;
    assert_eq!(kind(r).0, "permission_denied");
    e.ok(&a, "message.cancel", json!({"delivery": d2})).await;
    assert_eq!(e.delivery(&d2).delivery.state, DeliveryState::Cancelled);
    let r = e.call(&u, "message.retry", json!({"delivery": d2})).await;
    assert_eq!(kind(r), ("conflict".into(), "not_held".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn hop_and_rate_limits() {
    let e = Env::new();
    let u = user();
    e.ok(&u, "mention.set", json!({"pane": "a", "name": "coder"}))
        .await;
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    let sent = e
        .ok(
            &pane_ctx("a"),
            "message.send",
            json!({"to": ["reviewer"], "body": "ping"}),
        )
        .await;
    let mut last = sent["message"]["id"].as_str().unwrap().to_string();
    // Replies alternate between the two panes until the chain is MAX_HOPS deep.
    for hop in 1..=vm::MAX_HOPS {
        let who = if hop % 2 == 1 { "b" } else { "a" };
        let r = e
            .ok(
                &pane_ctx(who),
                "message.reply",
                json!({"message": last, "body": format!("hop {hop}")}),
            )
            .await;
        assert_eq!(r["message"]["hop"], hop);
        last = r["message"]["id"].as_str().unwrap().to_string();
    }
    let who = if (vm::MAX_HOPS + 1) % 2 == 1 {
        "b"
    } else {
        "a"
    };
    let r = e
        .call(
            &pane_ctx(who),
            "message.reply",
            json!({"message": last, "body": "too deep"}),
        )
        .await;
    assert_eq!(kind(r), ("conflict".into(), "hop_limit".into()));
    // 20 a minute from one pane (5 sends from `a` above).
    let mut limited = false;
    for i in 0..RATE_PER_MIN {
        let r = e
            .call(
                &pane_ctx("a"),
                "message.send",
                json!({"to": ["reviewer"], "body": format!("n{i}")}),
            )
            .await;
        if let Err(err) = r {
            assert!(err.kind_is(ErrorKind::RateLimited), "{err:?}");
            limited = true;
            break;
        }
    }
    assert!(limited);
}

#[tokio::test(flavor = "multi_thread")]
async fn engine_holds_without_an_agent_and_waits_for_a_busy_one() {
    let e = Env::new();
    let u = user();
    e.ok(&u, "mention.set", json!({"pane": "a", "name": "coder"}))
        .await;
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    let sent = e
        .ok(
            &u,
            "message.send",
            json!({"to": ["reviewer"], "body": "hi", "workspace": "wa"}),
        )
        .await;
    assert_eq!(sent["message"]["from"]["kind"], "human");
    let did = sent["deliveries"][0]["id"].as_str().unwrap().to_string();
    super::delivery::pump(&e.server);
    let d = e.delivery(&did);
    assert_eq!(d.delivery.state, DeliveryState::Held);
    assert_eq!(d.delivery.reason.as_deref(), Some("no_agent"));
    // A busy agent: retried, it stays queued.
    e.put_run("b", "claude", Execution::Working);
    e.ok(&u, "message.retry", json!({"delivery": did})).await;
    super::delivery::pump(&e.server);
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Queued);
    // Delivery off keeps it queued even when the agent is idle.
    e.ok(&u, "mention.set", json!({"pane": "b", "delivery": false}))
        .await;
    e.put_run("b", "claude", Execution::Idle);
    super::delivery::pump(&e.server);
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Queued);
}

fn claude() -> Harness {
    Harness::from_id("claude").unwrap()
}

/// What the recipient's `UserPromptSubmit` hook adds for `prompt`.
fn hook_context(e: &Env, pane: &str, prompt: &str) -> String {
    let out = super::hooks::after_signal(
        &e.server,
        pane,
        claude(),
        "UserPromptSubmit",
        &json!({"prompt": prompt}),
    )
    .unwrap_or_default();
    out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// `@coder` (a) and `@reviewer` (b) in `wa`, both idle; a message from a to b, its delivery
/// marked `submitted` as the engine does before typing. (message id, delivery id, envelope)
async fn submitted(e: &Env) -> (String, String, String) {
    let u = user();
    e.put_run("a", "claude", Execution::Idle);
    e.put_run("b", "claude", Execution::Idle);
    e.ok(&u, "mention.set", json!({"pane": "a", "name": "coder"}))
        .await;
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    let sent = e
        .ok(
            &pane_ctx("a"),
            "message.send",
            json!({"to": ["reviewer"], "body": "the secret plan"}),
        )
        .await;
    let mid = sent["message"]["id"].as_str().unwrap().to_string();
    let did = sent["deliveries"][0]["id"].as_str().unwrap().to_string();
    update(&e.server, &did, |r| {
        r.delivery.state = DeliveryState::Submitted;
        true
    });
    let rec = e.delivery(&did);
    let m = e.server.with_core(|c| load_message(c, &mid)).unwrap();
    (mid, did, envelope("@coder", &m, &rec.nonce))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_envelope_expands_once_and_only_in_its_own_pane() {
    let e = Env::new();
    let (_mid, did, env) = submitted(&e).await;
    // Another pane typing the same line gets no body.
    let ctx = hook_context(&e, "a", &env);
    assert!(ctx.contains("not an authentic"), "{ctx}");
    assert!(!ctx.contains("the secret plan"), "{ctx}");
    // The recipient gets it once.
    let ctx = hook_context(&e, "b", &env);
    assert!(ctx.contains("the secret plan"), "{ctx}");
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Delivered);
    // A replay (from the screen, a transcript or an event) gets a note, not the body.
    let ctx = hook_context(&e, "b", &env);
    assert!(ctx.contains("already delivered"), "{ctx}");
    assert!(!ctx.contains("the secret plan"), "{ctx}");
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_rotates_the_nonce_in_one_step() {
    let e = Env::new();
    let (_mid, did, env) = submitted(&e).await;
    update(&e.server, &did, |r| {
        r.delivery.state = DeliveryState::Held;
        r.delivery.reason = Some("hooks_missing".into());
        true
    });
    let before = e.delivery(&did).nonce;
    e.ok(&user(), "message.retry", json!({"delivery": did}))
        .await;
    let after = e.delivery(&did);
    assert_eq!(after.delivery.state, DeliveryState::Queued);
    assert_ne!(after.nonce, before);
    // The envelope typed before the retry is no longer authentic.
    let ctx = hook_context(&e, "b", &env);
    assert!(ctx.contains("not an authentic"), "{ctx}");
    // A delivered message cannot be retried (and so cannot be replayed with a fresh nonce).
    update(&e.server, &did, |r| {
        r.delivery.state = DeliveryState::Delivered;
        true
    });
    let r = e
        .call(&user(), "message.retry", json!({"delivery": did}))
        .await;
    assert_eq!(kind(r), ("conflict".into(), "not_held".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn panes_that_moved_workspace_end_the_conversation() {
    let e = Env::new();
    let (mid, did, env) = submitted(&e).await;
    // The sender moves to another workspace: the envelope no longer expands.
    e.put_pane("a", "wb");
    let ctx = hook_context(&e, "b", &env);
    assert!(ctx.contains("moved to another workspace"), "{ctx}");
    assert!(!ctx.contains("the secret plan"), "{ctx}");
    let d = e.delivery(&did);
    assert_eq!(d.delivery.state, DeliveryState::Held);
    assert_eq!(d.delivery.reason.as_deref(), Some(MOVED));
    // Nor can the recipient answer across workspaces.
    let r = e
        .call(
            &pane_ctx("b"),
            "message.reply",
            json!({"message": mid, "body": "x"}),
        )
        .await;
    assert_eq!(kind(r), ("permission_denied".into(), MOVED.into()));
    // The engine holds a queued delivery whose recipient moved.
    e.put_pane("a", "wa");
    e.ok(
        &user(),
        "mention.set",
        json!({"pane": "b", "name": "reviewer"}),
    )
    .await;
    let sent = e
        .ok(
            &pane_ctx("a"),
            "message.send",
            json!({"to": ["reviewer"], "body": "again"}),
        )
        .await;
    let d2 = sent["deliveries"][0]["id"].as_str().unwrap().to_string();
    e.put_pane("b", "wb");
    super::delivery::pump(&e.server);
    let d = e.delivery(&d2);
    assert_eq!(d.delivery.state, DeliveryState::Held);
    assert_eq!(d.delivery.reason.as_deref(), Some(MOVED));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leading_mention_marks_the_prompt_as_sent() {
    let e = Env::new();
    let u = user();
    e.put_run("a", "claude", Execution::Idle);
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    let t0 = now_ms();
    let out = super::hooks::before_signal(
        &e.server,
        "a",
        claude(),
        "UserPromptSubmit",
        &json!({"prompt": "@reviewer check the tests"}),
    )
    .expect("a blocking reply");
    assert_eq!(out["decision"], "block");
    let id = e.server.mentions.consumed_since("a", t0).expect("marked");
    assert!(out["reason"].as_str().unwrap().contains(&id), "{out}");
    assert_eq!(e.server.mentions.consumed_since("a", now_ms() + 1), None);
    assert_eq!(e.server.mentions.consumed_since("b", t0), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_recipient_gets_the_framed_message() {
    let e = Env::new();
    let u = user();
    e.put_run("b", "claude", Execution::Idle);
    {
        let mut c = e.server.core.lock().unwrap();
        let mut r = c.run_for_pane("b").cloned().unwrap();
        r.integration = "headless:claude".into();
        let mut tx = Tx::new();
        tx.run(r);
        e.server.commit(&mut c, tx).unwrap();
    }
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    let sent = e
        .ok(
            &u,
            "message.send",
            json!({"to": ["reviewer"], "body": "hi", "workspace": "wa"}),
        )
        .await;
    let did = sent["deliveries"][0]["id"].as_str().unwrap().to_string();
    super::delivery::pump(&e.server);
    let d = e.delivery(&did);
    // `fallback` means typed: it is set only together with `delivered`.
    assert!(
        !d.fallback || d.delivery.state == DeliveryState::Delivered,
        "{d:?}"
    );
    assert_ne!(d.delivery.state, DeliveryState::Queued);
    assert_ne!(d.delivery.reason.as_deref(), Some("hooks_missing"));
}

impl Env {
    /// The run of `pane` finished a turn (`done_rev` moves on).
    fn end_turn(&self, pane: &str) {
        let mut c = self.server.core.lock().unwrap();
        let mut r = c.run_for_pane(pane).cloned().unwrap();
        r.done_rev += 1;
        let mut tx = Tx::new();
        tx.run(r);
        self.server.commit(&mut c, tx).unwrap();
    }
    fn make_headless(&self, pane: &str) {
        let mut c = self.server.core.lock().unwrap();
        let mut r = c.run_for_pane(pane).cloned().unwrap();
        r.integration = "headless:claude".into();
        let mut tx = Tx::new();
        tx.run(r);
        self.server.commit(&mut c, tx).unwrap();
    }
    /// A person's message to `@reviewer` (pane b); its delivery id.
    async fn to_reviewer(&self, expects_reply: bool) -> String {
        let sent = self
            .ok(
                &user(),
                "message.send",
                json!({"to": ["reviewer"], "body": "hi", "workspace": "wa", "expects_reply": expects_reply}),
            )
            .await;
        sent["deliveries"][0]["id"].as_str().unwrap().to_string()
    }
    /// Mark delivery `id` as typed into pane `pane` in the run's current turn.
    fn mark_delivered(&self, id: &str, pane: &str, fallback: bool) {
        let mark = self.server.with_core(|c| turn_mark(c, pane));
        assert!(mark.is_some());
        update(&self.server, id, |r| {
            r.delivery.state = DeliveryState::Delivered;
            r.fallback = fallback;
            r.delivered_turn = mark;
            true
        });
    }
    fn pump(&self) {
        self.server.mentions.poke();
        super::delivery::pump(&self.server);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_fallback_is_not_held_as_hooks_missing() {
    let e = Env::new();
    // pi has no mention hooks: a stale envelope falls back to typing the framed body.
    e.put_run("b", "pi", Execution::Idle);
    e.ok(
        &user(),
        "mention.set",
        json!({"pane": "b", "name": "reviewer"}),
    )
    .await;
    let did = e.to_reviewer(true).await;
    // The envelope went in long ago and no hook took it; the fallback is not typed yet.
    update(&e.server, &did, |r| {
        r.delivery.state = DeliveryState::Submitted;
        r.submitted_at_ms = Some(now_ms() - super::delivery::HOOK_ACK_MS - 1_000);
        true
    });
    // The pane's input box cannot be read here (no screen): the fallback waits.
    for _ in 0..3 {
        e.pump();
    }
    let d = e.delivery(&did);
    assert_eq!(d.delivery.state, DeliveryState::Submitted, "{d:?}");
    assert_eq!(d.delivery.reason, None);
    assert!(!d.fallback, "nothing was typed: {d:?}");
    // A fallback that was typed and still has no turn end is a missing hook.
    update(&e.server, &did, |r| {
        r.fallback = true;
        true
    });
    e.pump();
    let d = e.delivery(&did);
    assert_eq!(d.delivery.state, DeliveryState::Held);
    assert_eq!(d.delivery.reason.as_deref(), Some("hooks_missing"));
}

#[tokio::test(flavor = "multi_thread")]
async fn deliveries_without_a_stop_hook_complete_when_the_turn_ends() {
    let e = Env::new();
    e.put_run("b", "claude", Execution::Idle);
    e.make_headless("b");
    e.ok(
        &user(),
        "mention.set",
        json!({"pane": "b", "name": "reviewer"}),
    )
    .await;
    // Headless, no answer expected: completed without a reason.
    let quiet = e.to_reviewer(false).await;
    e.mark_delivered(&quiet, "b", true);
    // Headless, an answer expected but none sent: completed as `no_reply`.
    let asked = e.to_reviewer(true).await;
    e.mark_delivered(&asked, "b", true);
    e.pump();
    assert_eq!(e.delivery(&quiet).delivery.state, DeliveryState::Delivered);
    assert_eq!(e.delivery(&asked).delivery.state, DeliveryState::Delivered);
    e.end_turn("b");
    e.pump();
    let d = e.delivery(&quiet);
    assert_eq!(d.delivery.state, DeliveryState::Completed);
    assert_eq!(d.delivery.reason, None);
    let d = e.delivery(&asked);
    assert_eq!(d.delivery.state, DeliveryState::Completed);
    assert_eq!(d.delivery.reason.as_deref(), Some("no_reply"));
    // A harness without mention hooks (the framed body was typed) completes the same way.
    e.put_run("b", "pi", Execution::Idle);
    let typed = e.to_reviewer(false).await;
    e.mark_delivered(&typed, "b", true);
    e.end_turn("b");
    e.pump();
    assert_eq!(e.delivery(&typed).delivery.state, DeliveryState::Completed);
    // Claude with hooks: its Stop hook completes deliveries, not the turn counter.
    e.put_run("b", "claude", Execution::Idle);
    let hooked = e.to_reviewer(false).await;
    e.mark_delivered(&hooked, "b", false);
    e.end_turn("b");
    e.pump();
    assert_eq!(e.delivery(&hooked).delivery.state, DeliveryState::Delivered);
}

#[test]
fn a_replaced_or_gone_run_ends_the_turn_of_a_delivery() {
    let m = Message {
        id: "m_1".into(),
        thread: "m_1".into(),
        workspace: "wa".into(),
        from: Author::Human { pane: None },
        to: vec!["reviewer".into()],
        body: "hi".into(),
        in_reply_to: None,
        hop: 0,
        sent_at_ms: 0,
    };
    let fresh = || {
        let mut r = new_delivery(&m, "reviewer", "b", false, false);
        r.delivery.state = DeliveryState::Delivered;
        r.delivered_turn = Some(("run-b".into(), 4));
        r
    };
    let mut run = crate::agents::harness_tests_run();
    run.id = "run-b".into();
    run.done_rev = 4;
    // Same Claude run, same turn: nothing to do. A later turn: the Stop hook decides.
    let mut r = fresh();
    super::delivery::finish_turn(&mut r, Some(&run));
    assert_eq!(r.delivery.state, DeliveryState::Delivered);
    run.done_rev = 5;
    super::delivery::finish_turn(&mut r, Some(&run));
    assert_eq!(r.delivery.state, DeliveryState::Delivered);
    // Another run in the pane, or none: the turn is over.
    run.id = "run-b2".into();
    super::delivery::finish_turn(&mut r, Some(&run));
    assert_eq!(r.delivery.state, DeliveryState::Completed);
    let mut r = fresh();
    super::delivery::finish_turn(&mut r, None);
    assert_eq!(r.delivery.state, DeliveryState::Completed);
    // Without a mark (delivered before marks existed) it is left to the Stop hook.
    let mut r = fresh();
    r.delivered_turn = None;
    super::delivery::finish_turn(&mut r, None);
    assert_eq!(r.delivery.state, DeliveryState::Delivered);
}

#[tokio::test(flavor = "multi_thread")]
async fn hop_passes_on_only_within_the_turn_that_got_the_message() {
    let e = Env::new();
    let u = user();
    e.put_run("a", "claude", Execution::Idle);
    e.put_run("b", "claude", Execution::Idle);
    e.ok(&u, "mention.set", json!({"pane": "a", "name": "coder"}))
        .await;
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    let sent = e
        .ok(
            &pane_ctx("a"),
            "message.send",
            json!({"to": ["reviewer"], "body": "please delegate"}),
        )
        .await;
    assert_eq!(sent["message"]["hop"], 0);
    let did = sent["deliveries"][0]["id"].as_str().unwrap().to_string();
    e.mark_delivered(&did, "b", false);
    // Working on the message, b delegates: the hop goes on.
    let fwd = e
        .ok(
            &pane_ctx("b"),
            "message.send",
            json!({"to": ["coder"], "body": "sub task"}),
        )
        .await;
    assert_eq!(fwd["message"]["hop"], 1);
    // A later turn of b is unrelated work, though the delivery is still open.
    e.end_turn("b");
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Delivered);
    let later = e
        .ok(
            &pane_ctx("b"),
            "message.send",
            json!({"to": ["coder"], "body": "something else"}),
        )
        .await;
    assert_eq!(later["message"]["hop"], 0);
    // An open delivery without a turn mark (recorded before marks existed) lends nothing.
    let to_a = fwd["deliveries"][0]["id"].as_str().unwrap().to_string();
    update(&e.server, &to_a, |r| {
        r.delivery.state = DeliveryState::Delivered;
        r.delivered_turn = None;
        true
    });
    assert_eq!(e.server.with_core(|c| hop_of_sender(c, Some("a"))), 0);
}

#[test]
fn observed_envelopes_lose_nonce_and_preview() {
    let line = "↪ vibeke message m_01abc from @coder · the secret plan [ABCDEFGHJKMN]";
    let clean = sanitize_envelope(line).unwrap();
    assert_eq!(clean, "↪ vibeke message m_01abc from @coder · … […]");
    let p = sanitize_payload(
        "UserPromptSubmit",
        &json!({"prompt": line, "session_id": "s"}),
    )
    .unwrap();
    assert_eq!(p["prompt"], clean.as_str());
    assert_eq!(p["session_id"], "s");
    assert!(sanitize_payload("UserPromptSubmit", &json!({"prompt": "hello"})).is_none());
    assert!(sanitize_payload("Stop", &json!({"prompt": line})).is_none());
}

/// `@coder` (a) and `@reviewer` (b) in `wa`, both working; a queued message from a to b.
/// (message id, delivery id)
async fn queued_to_busy(e: &Env) -> (String, String) {
    let u = user();
    e.put_run("a", "claude", Execution::Working);
    e.put_run("b", "claude", Execution::Working);
    e.ok(&u, "mention.set", json!({"pane": "a", "name": "coder"}))
        .await;
    e.ok(&u, "mention.set", json!({"pane": "b", "name": "reviewer"}))
        .await;
    let sent = e
        .ok(
            &pane_ctx("a"),
            "message.send",
            json!({"to": ["reviewer"], "body": "the secret plan"}),
        )
        .await;
    let mid = sent["message"]["id"].as_str().unwrap().to_string();
    let did = sent["deliveries"][0]["id"].as_str().unwrap().to_string();
    super::delivery::pump(&e.server);
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Queued);
    (mid, did)
}

fn hook_after(e: &Env, pane: &str, h: Harness, event: &str, p: Value) -> String {
    super::hooks::after_signal(&e.server, pane, h, event, &p).unwrap_or_default()
        ["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_message_rides_along_on_the_next_prompt_once() {
    let e = Env::new();
    let (mid, did) = queued_to_busy(&e).await;
    // Another pane's prompt takes nothing.
    assert_eq!(hook_context(&e, "a", "hello"), "");
    let ctx = hook_context(&e, "b", "please also fix the lint");
    assert!(ctx.contains("the secret plan"), "{ctx}");
    assert!(ctx.contains(&mid), "{ctx}");
    assert!(ctx.contains("untrusted"), "{ctx}");
    let d = e.delivery(&did);
    assert_eq!(d.delivery.state, DeliveryState::Delivered);
    assert_eq!(d.delivery.reason.as_deref(), Some(RIDE_ALONG));
    assert_eq!(d.delivered_turn, Some(("run-b".into(), 0)));
    // Delivered once: the next prompt carries nothing.
    assert_eq!(hook_context(&e, "b", "and then the docs"), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_message_rides_along_on_a_tool_result() {
    let post = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
    for h in ["claude", "codex"] {
        let e = Env::new();
        let (_mid, did) = queued_to_busy(&e).await;
        let h = Harness::from_id(h).unwrap();
        assert_eq!(hook_after(&e, "b", h, "PreToolUse", post.clone()), "");
        assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Queued);
        let ctx = hook_after(&e, "b", h, "PostToolUse", post.clone());
        assert!(ctx.contains("the secret plan"), "{ctx}");
        assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Delivered);
    }
    // Harnesses without mention hooks take nothing from a hook.
    let e = Env::new();
    let (_mid, did) = queued_to_busy(&e).await;
    let gemini = Harness::from_id("gemini").unwrap();
    assert_eq!(hook_after(&e, "b", gemini, "PostToolUse", post), "");
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Queued);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_headless_recipient_is_steered_unless_it_is_acp() {
    for (integration, steered) in [("headless:claude", true), ("headless:acp", false)] {
        let e = Env::new();
        e.put_run("b", "claude", Execution::Working);
        {
            let mut c = e.server.core.lock().unwrap();
            let mut r = c.run_for_pane("b").cloned().unwrap();
            r.integration = integration.into();
            let mut tx = Tx::new();
            tx.run(r);
            e.server.commit(&mut c, tx).unwrap();
        }
        e.ok(
            &user(),
            "mention.set",
            json!({"pane": "b", "name": "reviewer"}),
        )
        .await;
        let did = e.to_reviewer(false).await;
        e.pump();
        let state = e.delivery(&did).delivery.state;
        assert_eq!(
            state != DeliveryState::Queued,
            steered,
            "{integration}: {state:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queued_message_keeps_the_turn_going_at_stop() {
    let e = Env::new();
    let (mid, did) = queued_to_busy(&e).await;
    let stop = |e: &Env| super::hooks::before_signal(&e.server, "b", claude(), "Stop", &json!({}));
    let out = stop(&e).expect("a blocked stop");
    assert_eq!(out["decision"], "block");
    let reason = out["reason"].as_str().unwrap();
    assert!(reason.contains("the secret plan"), "{reason}");
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Delivered);
    // The next stop reminds once about the answer, then lets the turn end.
    let out = stop(&e).expect("the reminder");
    assert!(out["reason"].as_str().unwrap().contains(&mid), "{out}");
    assert!(stop(&e).is_none());
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn ride_along_skips_deliveries_the_engine_would_not_deliver() {
    let e = Env::new();
    let u = user();
    let (_mid, did) = queued_to_busy(&e).await;
    // Delivery switched off for the name.
    e.ok(&u, "mention.set", json!({"pane": "b", "delivery": false}))
        .await;
    assert_eq!(hook_context(&e, "b", "go on"), "");
    e.ok(&u, "mention.set", json!({"pane": "b", "delivery": true}))
        .await;
    // An envelope being typed stays with the engine.
    e.server.mentions.typing.lock().unwrap().insert(did.clone());
    assert_eq!(hook_context(&e, "b", "go on"), "");
    e.server.mentions.typing.lock().unwrap().remove(&did);
    // A submitted envelope too.
    update(&e.server, &did, |r| {
        r.delivery.state = DeliveryState::Submitted;
        true
    });
    assert_eq!(hook_context(&e, "b", "go on"), "");
    update(&e.server, &did, |r| {
        r.delivery.state = DeliveryState::Queued;
        true
    });
    // A recipient that moved to another workspace.
    e.put_pane("b", "wb");
    assert_eq!(hook_context(&e, "b", "go on"), "");
    assert_eq!(e.delivery(&did).delivery.state, DeliveryState::Queued);
}
